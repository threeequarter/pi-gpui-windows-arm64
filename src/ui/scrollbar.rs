//! Pieces shared by the thin draggable scrollbars drawn over scrolling
//! content (file viewer, chat).

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, DispatchPhase, HitboxBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ScrollHandle, ScrollWheelEvent, Window, canvas, fill,
    point, prelude::*, px, size,
};

use super::ActiveTheme as _;

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

#[derive(Clone, Default)]
pub struct PreviewScroll {
    pub handle: ScrollHandle,
    grab: Rc<Cell<Option<Pixels>>>,
}

impl PreviewScroll {
    pub fn shift_wheel(&self) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
        let handle = self.handle.clone();
        move |event, window, cx| {
            if !event.modifiers.shift || event.modifiers.secondary() {
                return;
            }
            let delta = event.delta.pixel_delta(px(20.));
            let max = handle.max_offset().width;
            if max <= px(0.) || delta.x != px(0.) || delta.y == px(0.) {
                return;
            }
            let mut offset = handle.offset();
            offset.x = (offset.x + delta.y).clamp(-max, px(0.));
            handle.set_offset(offset);
            window.refresh();
            cx.stop_propagation();
        }
    }

    pub fn horizontal_bar(&self, cx: &App) -> AnyElement {
        let scroll = self.clone();
        let color = cx.theme().colors.text_placeholder;
        canvas(
            move |bounds, window, _| {
                let max = scroll.handle.max_offset().width;
                let track = bounds.size.width;
                if max <= px(0.) || track <= px(1.) {
                    scroll.grab.set(None);
                    return None;
                }
                let thumb_width = thumb_height(track, track + max).min(track - px(1.));
                let travel = track - thumb_width;
                let offset = -scroll.handle.offset().x;
                let thumb = Bounds {
                    origin: bounds.origin
                        + point(travel * (offset.clamp(px(0.), max) / max), px(2.)),
                    size: size(thumb_width, bounds.size.height - px(4.)),
                };
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll);
                Some((scroll, hitbox, thumb, travel))
            },
            move |bounds, metrics, window, _| {
                let Some((scroll, hitbox, thumb, travel)) = metrics else {
                    return;
                };
                let active = scroll.grab.get().is_some();
                let mut quad = fill(thumb, color.opacity(if active { 0.7 } else { 0.45 }));
                quad.corner_radii = px(3.).into();
                window.paint_quad(quad);

                let sync =
                    move |scroll: &Self, mouse_x: Pixels, grab: Pixels, window: &mut Window| {
                        let mut offset = scroll.handle.offset();
                        let fraction: f32 = (mouse_x - bounds.origin.x - grab) / travel;
                        offset.x = -scroll.handle.max_offset().width * fraction.clamp(0., 1.);
                        scroll.handle.set_offset(offset);
                        window.refresh();
                    };
                let pressed = scroll.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !hitbox.is_hovered(window)
                    {
                        return;
                    }
                    let from_thumb = event.position.x - thumb.origin.x;
                    let grab = if from_thumb >= px(0.) && from_thumb <= thumb.size.width {
                        from_thumb
                    } else {
                        thumb.size.width * 0.5
                    };
                    pressed.grab.set(Some(grab));
                    sync(&pressed, event.position.x, grab, window);
                    cx.stop_propagation();
                });
                let moving = scroll.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some(grab) = moving.grab.get() else {
                        return;
                    };
                    if event.pressed_button == Some(MouseButton::Left) {
                        sync(&moving, event.position.x, grab, window);
                        cx.stop_propagation();
                    } else {
                        moving.grab.set(None);
                        window.refresh();
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, _| {
                    if phase == DispatchPhase::Capture
                        && event.button == MouseButton::Left
                        && scroll.grab.take().is_some()
                    {
                        window.refresh();
                    }
                });
            },
        )
        .absolute()
        .bottom_0()
        .left_0()
        .right_0()
        .h(px(10.))
        .into_any_element()
    }
}
