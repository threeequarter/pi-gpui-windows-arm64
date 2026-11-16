//! Pieces shared by the thin draggable scrollbars drawn over scrolling
//! content (file viewer, chat).

use gpui::{Pixels, px};

/// A scrollbar thumb drag in progress.
#[derive(Clone, Copy)]
pub struct ScrollDrag {
    pub mouse_y: Pixels,
    pub scroll: Pixels,
    pub content: Pixels,
    pub track: Pixels,
}

pub fn thumb_height(track: Pixels, content: Pixels) -> Pixels {
    (track * (f32::from(track) / f32::from(content))).max(px(24.))
}
