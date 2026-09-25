// gesture.rs — touchpad pinch on Linux. The app runs on X11 (Xwayland; see
// main) and winit 0.30 turns a pinch into egui's zoom event on macOS only.
// The X server delivers it as XInput 2.4 gesture events, which nothing up
// the stack selects, so select them on winit's window from a second X
// connection, on a thread that sleeps until an event arrives (a window's
// gesture events go to one client only; winit is not it). The factor
// accumulates here and the view applies it next frame, like egui's own
// zoom_delta. Native Wayland (HIK_WAYLAND=1) has no pinch.

use std::sync::{Arc, Mutex};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};

pub struct Pinch {
    factor: Arc<Mutex<f32>>,
}

impl Pinch {
    /// `window`: winit's X window. None when the server has no XInput 2.4.
    pub fn start(window: u32, ctx: eframe::egui::Context) -> Option<Pinch> {
        let (conn, _) = x11rb::connect(None)
            .map_err(|e| debug(&format!("connect: {e}")))
            .ok()?;
        let v = conn
            .xinput_xi_query_version(2, 4)
            .ok()?
            .reply()
            .map_err(|e| debug(&format!("xinput: {e}")))
            .ok()?;
        if (v.major_version, v.minor_version) < (2, 4) {
            debug(&format!(
                "xinput {}.{} has no gestures",
                v.major_version, v.minor_version
            ));
            return None;
        }
        let bits = 1u32 << xinput::GESTURE_PINCH_BEGIN_EVENT
            | 1 << xinput::GESTURE_PINCH_UPDATE_EVENT
            | 1 << xinput::GESTURE_PINCH_END_EVENT;
        let mask = xinput::EventMask {
            deviceid: 1, // XIAllMasterDevices
            mask: vec![xinput::XIEventMask::from(bits)],
        };
        conn.xinput_xi_select_events(window, &[mask])
            .ok()?
            .check()
            .map_err(|e| debug(&format!("select: {e}")))
            .ok()?;
        let factor = Arc::new(Mutex::new(1.0f32));
        let acc = factor.clone();
        std::thread::Builder::new()
            .name("pinch".into())
            .spawn(move || {
                // XI reports the scale relative to the gesture's start.
                let mut last = 1.0f32;
                loop {
                    match conn.wait_for_event() {
                        Ok(Event::XinputGesturePinchBegin(_)) => last = 1.0,
                        Ok(Event::XinputGesturePinchUpdate(e)) => {
                            let scale = e.scale as f32 / 65536.0;
                            if scale > 0.0 {
                                *acc.lock().unwrap() *= scale / last;
                                last = scale;
                                ctx.request_repaint();
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            debug(&format!("{e}"));
                            return;
                        }
                    }
                }
            })
            .ok()?;
        debug("pinch gestures selected");
        Some(Pinch { factor })
    }

    /// The zoom factor gathered since the last call (1 = none).
    pub fn take(&self) -> f32 {
        std::mem::replace(&mut *self.factor.lock().unwrap(), 1.0)
    }
}

fn debug(msg: &str) {
    if std::env::var_os("HIK_DEBUG").is_some() {
        eprintln!("[gesture] {msg}");
    }
}
