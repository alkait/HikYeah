// tile.rs — drawing shared by the grid and the focused view: aspect fit,
// overlay labels and snapshot placeholders, in TileView.swift's look (white
// text on translucent black; a cached frame dimmed and badged).

use crate::stream;
use eframe::egui;

pub const WHITE: egui::Color32 = egui::Color32::from_rgba_premultiplied(240, 240, 240, 255);
pub const DIM: egui::Color32 = egui::Color32::from_rgba_premultiplied(150, 150, 150, 255);
/// NSColor.systemRed — the grid keyboard cursor.
pub const CURSOR_RED: egui::Color32 = egui::Color32::from_rgb(255, 59, 48);

/// Aspect-fit `dims` (or 16:9 if unknown) inside `cell`.
pub fn fit(cell: egui::Rect, dims: Option<egui::Vec2>) -> egui::Rect {
    let ts = dims.unwrap_or(egui::vec2(16.0, 9.0));
    let scale = (cell.width() / ts.x).min(cell.height() / ts.y);
    egui::Rect::from_center_size(cell.center(), ts * scale)
}

pub fn frame_dims(shared: &stream::Shared) -> Option<egui::Vec2> {
    shared
        .current
        .lock()
        .unwrap()
        .as_ref()
        .map(|f| egui::vec2(f.width as f32, f.height as f32))
}

pub fn label(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
) {
    let galley = painter.layout(text.to_string(), font, color, f32::INFINITY);
    let rect = align.anchor_size(pos, galley.size());
    painter.rect_filled(
        rect.expand2(egui::vec2(5.0, 3.0)),
        3.0,
        egui::Color32::from_black_alpha(140),
    );
    painter.galley(rect.min, galley, color);
}

/// Placeholder JPEG, aspect-fit in `cell`; a cached one is dimmed to 75%
/// and badged so it's never mistaken for live (TileView.setPlaceholder).
pub fn draw_placeholder(
    painter: &egui::Painter,
    cell: egui::Rect,
    tex: &egui::TextureHandle,
    cached: bool,
) -> egui::Rect {
    let rect = fit(cell, Some(tex.size_vec2()));
    let tint = if cached {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 191)
    } else {
        egui::Color32::WHITE
    };
    painter.image(
        tex.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        tint,
    );
    if cached {
        label(
            painter,
            rect.right_top() + egui::vec2(-6.0, 6.0),
            egui::Align2::RIGHT_TOP,
            "cached",
            egui::FontId::proportional(10.0),
            WHITE,
        );
    }
    rect
}
