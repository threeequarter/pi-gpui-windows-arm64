//! A thin draggable scrollbar over the right edge of the message list, like
//! the file viewer's. Dragging or wheeling it to the end re-enters follow
//! mode, as scrolling the list itself does.

use gpui::{MouseDownEvent, MouseMoveEvent, Pixels, ScrollWheelEvent, point};

use super::*;
use crate::ui::scrollbar::{ScrollDrag, thumb_height};

impl ChatView {
    /// Track height, whole content height and the current offset, or `None`
    /// when the rows fit and need no scrollbar.
    fn scroll_metrics(&self) -> Option<(Pixels, Pixels, Pixels)> {
        let track = self.list_state.viewport_bounds().size.height;
        let max = self.list_state.max_offset_for_scrollbar().height;
        if track <= px(0.) || max <= px(0.) {
            return None;
        }
        let offset = -self.list_state.scroll_px_offset_for_scrollbar().y;
        Some((track, track + max, offset.clamp(px(0.), max)))
    }

    fn apply_scroll(&self, desired: Pixels) {
        self.list_state
            .set_offset_from_scrollbar(point(px(0.), -desired.max(px(0.))));
    }

    fn start_scroll_drag(&mut self, mouse_y: Pixels, cx: &mut Context<Self>) {
        let Some((track, content, offset)) = self.scroll_metrics() else {
            return;
        };
        self.list_state.scrollbar_drag_started();
        self.scroll_drag = Some(ScrollDrag {
            mouse_y,
            scroll: offset,
            content,
            track,
        });
        cx.notify();
    }

    pub(super) fn end_scroll_drag(&mut self, cx: &mut Context<Self>) {
        if self.scroll_drag.take().is_some() {
            self.list_state.scrollbar_drag_ended();
            cx.notify();
        }
    }

    pub(super) fn drag_scroll(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.scroll_drag else {
            return;
        };
        // Released outside the window: the move arrives without the button.
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_scroll_drag(cx);
            return;
        }
        let scale = f32::from(drag.content) / f32::from(drag.track).max(1.);
        self.apply_scroll(drag.scroll + (event.position.y - drag.mouse_y) * scale);
        cx.notify();
    }

    pub(super) fn render_scrollbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (track, content, offset) = self.scroll_metrics()?;
        let colors = cx.theme().colors.clone();
        let thumb = thumb_height(track, content);
        let thumb_y = (track - thumb) * (f32::from(offset) / f32::from(content - track));
        let top = self.list_state.viewport_bounds().origin.y;
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(10.))
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    let delta = event.delta.pixel_delta(px(20.));
                    let offset = -this.list_state.scroll_px_offset_for_scrollbar().y;
                    this.apply_scroll(offset - delta.y);
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        let Some((track, content, _)) = this.scroll_metrics() else {
                            return;
                        };
                        let thumb = thumb_height(track, content);
                        let scale = f32::from(content) / f32::from(track);
                        this.apply_scroll((event.position.y - top - thumb * 0.5) * scale);
                        this.start_scroll_drag(event.position.y, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(2.))
                        .w(px(6.))
                        .top(thumb_y)
                        .h(thumb)
                        .rounded(px(3.))
                        .bg(colors.text_placeholder.opacity(0.45))
                        .hover(|style| style.bg(colors.text_placeholder.opacity(0.7)))
                        // On the thumb, a drag keeps the grab point.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                this.start_scroll_drag(event.position.y, cx);
                                cx.stop_propagation();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }
}
