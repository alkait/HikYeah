// snapshot.rs — last-known JPEG per camera on disk, so the grid paints
// instantly at launch (before any network) with a clearly-marked "cached"
// frame; a fresh ISAPI snapshot then replaces it and refreshes the cache.
// Port of SnapshotCache + ISAPI.snapshot from the Mac app.

use crate::config::StoredCamera;
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home_dir().join(".cache"))
        .join("hikviewer")
}

fn cache_path(host: &str) -> PathBuf {
    cache_dir().join(format!("snapshots/{}.jpg", host.replace('/', "_")))
}

fn decode(jpeg: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// Last-known frame from disk (possibly stale — callers mark it "cached").
pub fn load_cached(host: &str) -> Option<egui::ColorImage> {
    decode(&std::fs::read(cache_path(host)).ok()?)
}

/// One JPEG frame over ISAPI.
fn fetch(cam: &StoredCamera, channel: &str) -> Option<Vec<u8>> {
    let path = format!("/ISAPI/Streaming/channels/{channel}/picture");
    let jpeg = crate::isapi::get(&cam.host, &cam.user, &cam.password, &path)?;
    // Cameras answer errors as XML bodies with status 200 sometimes — accept
    // only something that looks like a JPEG.
    (jpeg.len() > 4 && jpeg[..2] == [0xFF, 0xD8]).then_some(jpeg)
}

/// Background fetch: fresh snapshot -> refresh the disk cache -> hand the
/// decoded image to the UI (which swaps it in, unbadged). `cam_id` is the
/// tile's stable id — indices shift under reorders and rebuilds.
pub fn spawn_fetch(
    cam: StoredCamera,
    cam_id: u64,
    channel: &'static str,
    tx: Sender<(u64, egui::ColorImage)>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let Some(jpeg) = fetch(&cam, channel) else {
            return;
        };
        let path = cache_path(&cam.host);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("jpg.tmp");
        if std::fs::write(&tmp, &jpeg).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
        if let Some(img) = decode(&jpeg)
            && tx.send((cam_id, img)).is_ok()
        {
            ctx.request_repaint();
        }
    });
}
