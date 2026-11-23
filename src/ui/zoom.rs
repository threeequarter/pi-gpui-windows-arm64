//! Zoom for the image and diagram previews: Ctrl+wheel and a +/- button
//! cluster step one factor between fixed bounds.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{App, ElementId, ScrollWheelEvent, Window, prelude::*, px};

use super::button::IconButton;
use super::h_flex;
use super::icon::{IconName, IconSize};
use super::list_item::Tooltip;
use super::scrollbar::PreviewScroll;
use super::theme::ActiveTheme as _;

pub const MIN_FACTOR: f32 = 0.25;
pub const MAX_FACTOR: f32 = 4.;
const STEP: f32 = 1.25;

#[derive(Clone, Copy)]
pub enum Zoom {
    In,
    Out,
}

pub fn step(factor: f32, direction: Zoom, minimum: f32) -> f32 {
    match direction {
        Zoom::In => factor * STEP,
        Zoom::Out => factor / STEP,
    }
    .clamp(minimum, MAX_FACTOR)
}

pub type ZoomSink = Rc<dyn Fn(Zoom, &mut Window, &mut App)>;
pub type DiagramZoomSink = Rc<dyn Fn(String, Zoom, &mut Window, &mut App)>;

pub struct DiagramView {
    pub factor: f32,
    pub scroll: PreviewScroll,
}

impl Default for DiagramView {
    fn default() -> Self {
        Self {
            factor: 1.,
            scroll: PreviewScroll::default(),
        }
    }
}

pub struct DiagramZoom<'a> {
    views: &'a HashMap<String, DiagramView>,
    on_zoom: DiagramZoomSink,
}

impl<'a> DiagramZoom<'a> {
    pub fn new(views: &'a HashMap<String, DiagramView>, on_zoom: DiagramZoomSink) -> Self {
        Self { views, on_zoom }
    }

    pub fn view(&self, key: &str) -> Option<&DiagramView> {
        self.views.get(key)
    }

    pub fn sink(&self, key: &str) -> ZoomSink {
        let on_zoom = self.on_zoom.clone();
        let key = key.to_string();
        Rc::new(move |direction, window, cx| on_zoom(key.clone(), direction, window, cx))
    }
}

/// The +/- cluster; callers position it over their preview.
pub fn controls(id: &str, factor: f32, minimum: f32, sink: ZoomSink, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    h_flex()
        .gap(px(2.))
        .p(px(2.))
        .rounded(px(4.))
        .bg(colors.elevated_surface_background)
        .child(
            IconButton::new(
                ElementId::Name(format!("{id}:zoom-out").into()),
                IconName::Dash,
            )
            .icon_size(IconSize::XSmall)
            .disabled(factor <= minimum)
            .tooltip(Tooltip::text("Zoom out"))
            .on_click({
                let sink = sink.clone();
                move |_, window, cx| sink(Zoom::Out, window, cx)
            }),
        )
        .child(
            IconButton::new(
                ElementId::Name(format!("{id}:zoom-in").into()),
                IconName::Plus,
            )
            .icon_size(IconSize::XSmall)
            .disabled(factor >= MAX_FACTOR)
            .tooltip(Tooltip::text("Zoom in"))
            .on_click(move |_, window, cx| sink(Zoom::In, window, cx)),
        )
}

/// Ctrl+wheel zooms instead of scrolling; other wheels pass through. Attach
/// to a child of the scroller so the zoom wins over the default scroll.
pub fn wheel(sink: ZoomSink) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) {
    move |event: &ScrollWheelEvent, window, cx| {
        if !event.modifiers.secondary() {
            return;
        }
        let delta = event.delta.pixel_delta(px(20.)).y;
        if delta == px(0.) {
            return;
        }
        cx.stop_propagation();
        let direction = if delta > px(0.) { Zoom::In } else { Zoom::Out };
        sink(direction, window, cx);
    }
}
