// Diagnostic: an empty egui window repainting at ~60 Hz, to measure the
// eframe/winit/present CPU baseline on this machine with no video at all.

use eframe::egui;

struct Blank;

impl eframe::App for Blank {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.label("blank");
        let ms = std::env::var("BLANK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(ms));
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "blank",
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::new(Blank))),
    )
}
