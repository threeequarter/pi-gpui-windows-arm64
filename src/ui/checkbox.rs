//! A tri-state checkbox: the markdown task glyph (`src/markdown.rs`) made
//! interactive, with the button layer's hover, disabled and click semantics.

use gpui::{AnyView, App, ClickEvent, ElementId, MouseButton, Window, div, prelude::*, px, svg};

use super::icon::{IconName, IconSize};
use super::theme::ActiveTheme as _;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type TooltipBuilder = Box<dyn Fn(&mut Window, &mut App) -> AnyView + 'static>;

/// The clickable square, like the slot the markdown checkbox sits in.
const HIT_SIZE: f32 = 20.;
/// The box itself, a little smaller than the markdown glyph's 16 px.
const BOX_SIZE: f32 = 14.;

/// The value a checkbox shows: off, on, or a group whose parts disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToggleState {
    Unselected,
    /// Some of what this box stands for is selected.
    Indeterminate,
    Selected,
}

#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,
    state: ToggleState,
    disabled: bool,
    on_click: Option<ClickHandler>,
    tooltip: Option<TooltipBuilder>,
}

impl Checkbox {
    pub fn new(id: impl Into<ElementId>, state: ToggleState) -> Self {
        Self {
            id: id.into(),
            state,
            disabled: false,
            on_click: None,
            tooltip: None,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Box::new(tooltip));
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Checkbox {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = &cx.theme().colors;
        let interactive = !self.disabled;

        let (background, border) = match (self.state, self.disabled) {
            (ToggleState::Unselected, false) => (colors.element_background, colors.border),
            (ToggleState::Unselected, true) => (colors.element_background, colors.border_variant),
            // The accent is both fill and border, so the checked square reads
            // as one block of color with no seam.
            (ToggleState::Selected | ToggleState::Indeterminate, false) => {
                (colors.icon_accent, colors.icon_accent)
            }
            // A checked box that cannot be clicked keeps its tint but loses
            // the accent's strength.
            (ToggleState::Selected | ToggleState::Indeterminate, true) => {
                (colors.info_background, colors.border_variant)
            }
        };
        // Themes have no on-accent token: the glyph is cut out of the fill.
        let glyph = if self.disabled {
            colors.icon_disabled
        } else {
            colors.editor_background
        };
        let icon = match self.state {
            ToggleState::Unselected => None,
            ToggleState::Indeterminate => Some(IconName::Dash),
            ToggleState::Selected => Some(IconName::Check),
        };

        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(HIT_SIZE))
            .when(interactive, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .active(|style| style.bg(colors.ghost_element_active))
            })
            .when(self.disabled, |this| this.cursor_not_allowed())
            .when_some(self.on_click.filter(|_| interactive), |this, on_click| {
                // Keep focus where it is (e.g. in a text field) when pressing the box.
                this.on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(move |event, window, cx| {
                        // A checkbox inside a clickable row must not also trigger the row.
                        cx.stop_propagation();
                        on_click(event, window, cx);
                    })
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| tooltip(window, cx))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(BOX_SIZE))
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .when_some(icon, |this, icon| {
                        this.child(
                            svg()
                                .path(icon.path())
                                .flex_none()
                                .size(IconSize::XSmall.pixels())
                                .text_color(glyph),
                        )
                    }),
            )
    }
}
