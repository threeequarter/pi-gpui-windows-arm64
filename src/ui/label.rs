use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, FontWeight, GlobalElementId,
    InspectorElementId, LayoutId, Pixels, SharedString, Size, Window, WrappedLine, div, prelude::*,
    pulsating_between, px,
};

use super::animation::phase;
use super::theme::Color;

const PULSE_PERIOD: Duration = Duration::from_millis(1600);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LabelSize {
    Large,
    #[default]
    Default,
    Small,
    XSmall,
}

impl LabelSize {
    /// Scaled by the interface size setting.
    pub fn pixels(self, cx: &App) -> Pixels {
        self.base_pixels() * crate::settings::Settings::get(cx).ui_scale()
    }

    pub(crate) fn line_height(self, cx: &App) -> Pixels {
        (self.base_line_height() * crate::settings::Settings::get(cx).ui_scale()).round()
    }

    fn base_pixels(self) -> Pixels {
        match self {
            LabelSize::Large => px(16.),
            LabelSize::Default => px(14.),
            LabelSize::Small => px(12.),
            LabelSize::XSmall => px(11.),
        }
    }

    fn base_line_height(self) -> Pixels {
        match self {
            LabelSize::Large => px(22.),
            LabelSize::Default => px(20.),
            LabelSize::Small => px(17.),
            LabelSize::XSmall => px(15.),
        }
    }
}

/// Text with a semantic size and color, like Zed's `ui::Label`.
#[derive(IntoElement)]
pub struct Label {
    text: SharedString,
    size: LabelSize,
    color: Color,
    weight: Option<FontWeight>,
    italic: bool,
    truncate: bool,
    truncate_start: bool,
    monospace: bool,
}

impl Label {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            size: LabelSize::default(),
            color: Color::default(),
            weight: None,
            italic: false,
            truncate: false,
            truncate_start: false,
            monospace: false,
        }
    }

    pub fn size(mut self, size: LabelSize) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn weight(mut self, weight: FontWeight) -> Self {
        self.weight = Some(weight);
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Single line with an ellipsis. The parent must allow shrinking (`min_w(px(0.))`).
    pub fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    /// Single line that keeps its end visible: when the text does not fit,
    /// the beginning is clipped instead of the end, without a marker. Names
    /// that read from the right (repositories, branches) stay recognizable
    /// when narrow. The parent must allow shrinking (`min_w(px(0.))`).
    pub fn truncate_start(mut self) -> Self {
        self.truncate_start = true;
        self
    }

    pub fn monospace(mut self) -> Self {
        self.monospace = true;
        self
    }
}

impl RenderOnce for Label {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let content = if self.truncate_start {
            StartTruncatedText {
                text: self.text.clone(),
                state: Rc::new(RefCell::new(None)),
            }
            .into_any_element()
        } else {
            self.text.clone().into_any_element()
        };
        div()
            .text_size(self.size.pixels(cx))
            .line_height(self.size.line_height(cx))
            .text_color(self.color.text(cx))
            .when_some(self.weight, |this, weight| this.font_weight(weight))
            .when(self.italic, |this| this.italic())
            .when(self.monospace, |this| {
                this.font_family(
                    crate::settings::Settings::get(cx)
                        .buffer_font_family
                        .clone(),
                )
            })
            .when(self.truncate, |this| this.min_w(px(0.)).truncate())
            .when(self.truncate_start, |this| {
                this.min_w(px(0.)).overflow_hidden()
            })
            .child(content)
    }
}

/// Single-line text measured against the width it is given: when the full
/// text does not fit, the widest suffix that fits is shaped and painted
/// instead, without a marker. GPUI's own truncation only cuts the end.
struct StartTruncatedText {
    text: SharedString,
    state: Rc<RefCell<Option<StartTruncateState>>>,
}

struct StartTruncateState {
    line: WrappedLine,
    line_height: Pixels,
    width: Option<Pixels>,
    bounds: Option<Bounds<Pixels>>,
}

impl IntoElement for StartTruncatedText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for StartTruncatedText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = text_style
            .line_height
            .to_pixels(font_size.into(), window.rem_size());
        let text = self.text.clone();
        let state = self.state.clone();
        let layout_id = window.request_measured_layout(
            Default::default(),
            move |known_dimensions, available_space, window, _cx| {
                let width = known_dimensions.width.or(match available_space.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                // The measure runs several times per frame; shape once per width.
                if let Some(inner) = state.borrow().as_ref()
                    && inner.width == width
                {
                    return Size {
                        width: inner.line.width(),
                        height: inner.line_height,
                    };
                }
                let shape = |content: SharedString| {
                    let runs = vec![text_style.to_run(content.len())];
                    window
                        .text_system()
                        .shape_text(content, font_size, &runs, None, None)
                        .ok()
                        .and_then(|mut lines| lines.pop())
                };
                let Some(mut line) = shape(text.clone()) else {
                    return Size::default();
                };
                if let Some(width) = width
                    && line.width() > width
                {
                    // Suffix widths shrink monotonically as the cut moves
                    // right, so binary-search the first one that fits.
                    let boundaries: Vec<usize> =
                        text.char_indices().map(|(index, _)| index).collect();
                    let (mut low, mut high) = (0, boundaries.len());
                    let mut fitted = None;
                    while low < high {
                        let middle = (low + high) / 2;
                        let candidate = SharedString::from(text[boundaries[middle]..].to_string());
                        match shape(candidate.clone()) {
                            Some(candidate_line) if candidate_line.width() <= width => {
                                fitted = Some(candidate_line);
                                high = middle;
                            }
                            _ => low = middle + 1,
                        }
                    }
                    line = fitted
                        .unwrap_or_else(|| shape(SharedString::from("")).unwrap_or(line.clone()));
                }
                let size = Size {
                    width: line.width().min(width.unwrap_or(line.width())),
                    height: line_height,
                };
                *state.borrow_mut() = Some(StartTruncateState {
                    line,
                    line_height,
                    width,
                    bounds: None,
                });
                size
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if let Some(inner) = self.state.borrow_mut().as_mut() {
            inner.bounds = Some(bounds);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = self.state.borrow();
        if let Some(inner) = state.as_ref()
            && let Some(bounds) = inner.bounds
        {
            let _ = inner.line.paint(
                bounds.origin,
                inner.line_height,
                Default::default(),
                Some(bounds),
                window,
                cx,
            );
        }
    }
}

/// A label that breathes while something is in progress. Like [`super::Spinner`],
/// it needs the owning view's [`super::AnimationTicker`] to move.
#[derive(IntoElement)]
pub struct LoadingLabel {
    label: Label,
}

impl LoadingLabel {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self {
            label: Label::new(text).color(Color::Muted),
        }
    }

    pub fn size(mut self, size: LabelSize) -> Self {
        self.label = self.label.size(size);
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.label = self.label.color(color);
        self
    }
}

impl RenderOnce for LoadingLabel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let opacity = pulsating_between(0.45, 1.)(phase(PULSE_PERIOD));
        div().opacity(opacity).child(self.label)
    }
}
