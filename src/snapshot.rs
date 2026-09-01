// snapshot.rs — last-known JPEG per camera on disk, so the grid paints
// instantly at launch (before any network) with a clearly-marked "cached"
// frame; a fresh ISAPI snapshot then replaces it and refreshes the cache.
// Port of SnapshotCache + ISAPI.snapshot from the Mac app. Hikvision's ISAPI
// wants HTTP digest auth (URLSession did that for free; here it's explicit).

use crate::config::StoredCamera;
use eframe::egui;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("hikviewer/snapshots"))
}

fn cache_path(host: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(host.replace('/', "_").to_string() + ".jpg"))
}

fn decode(jpeg: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// Last-known frame from disk (possibly stale — callers mark it "cached").
pub fn load_cached(host: &str) -> Option<egui::ColorImage> {
    decode(&std::fs::read(cache_path(host)?).ok()?)
}

/// One JPEG frame over ISAPI with digest auth (6 s timeout, like the Mac app).
fn fetch(cam: &StoredCamera, channel: &str) -> Option<Vec<u8>> {
    let path = format!("/ISAPI/Streaming/channels/{channel}/picture");
    let url = format!("http://{}{}", cam.host, path);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(6))
        .build();
    let resp = match agent.get(&url).call() {
        Ok(r) => r, // camera without auth on this endpoint — take it
        Err(ureq::Error::Status(401, r)) => {
            let www = r.header("www-authenticate")?.to_string();
            let mut prompt = digest_auth::parse(&www).ok()?;
            let ctx = digest_auth::AuthContext::new(&cam.user, &cam.password, &path);
            let answer = prompt.respond(&ctx).ok()?.to_header_string();
            agent.get(&url).set("Authorization", &answer).call().ok()?
        }
        Err(_) => return None,
    };
    let mut jpeg = Vec::new();
    resp.into_reader()
        .take(20 << 20)
        .read_to_end(&mut jpeg)
        .ok()?;
    // Cameras answer errors as XML bodies with status 200 sometimes — accept
    // only something that looks like a JPEG.
    (jpeg.len() > 4 && jpeg[..2] == [0xFF, 0xD8]).then_some(jpeg)
}

/// Background fetch: fresh snapshot -> refresh the disk cache -> hand the
/// decoded image to the UI (which swaps it in, unbadged).
pub fn spawn_fetch(
    cam: StoredCamera,
    idx: usize,
    channel: &'static str,
    tx: Sender<(usize, egui::ColorImage)>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let Some(jpeg) = fetch(&cam, channel) else {
            return;
        };
        if let Some(path) = cache_path(&cam.host) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("jpg.tmp");
            if std::fs::write(&tmp, &jpeg).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        if let Some(img) = decode(&jpeg)
            && tx.send((idx, img)).is_ok()
        {
            ctx.request_repaint();
        }
    });
}
