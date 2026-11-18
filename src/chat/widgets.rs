//! Extension widgets above and below the composer (pi's `setWidget`), one
//! card per widget like the transcript's tool rows. A card shows its first
//! line until expanded. The bridge extension renders component widgets to
//! lines, so every widget arrives as terminal-styled text.

use std::collections::HashSet;

use gpui::{AnyElement, Context, ElementId, StyledText, div, px};
use serde_json::Value;

use super::ansi::{self, StyledLine};
use super::{ChatView, disclosure_button};
use crate::settings::Settings;
use crate::ui::prelude::*;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Placement {
    AboveComposer,
    BelowComposer,
}

struct Widget {
    key: String,
    placement: Placement,
    lines: Vec<StyledLine>,
}

#[derive(Default)]
pub(super) struct Widgets {
    widgets: Vec<Widget>,
    /// Keys of expanded cards; kept while a widget is replaced or re-added.
    expanded: HashSet<String>,
}

impl Widgets {
    /// Applies a `setWidget` request: no lines removes the widget.
    pub fn apply(&mut self, record: &Value) {
        let Some(key) = record["widgetKey"].as_str() else {
            return;
        };
        let mut lines: Vec<StyledLine> = record["widgetLines"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ansi::parse)
                    .collect()
            })
            .unwrap_or_default();
        while lines.last().is_some_and(|line| line.text.is_empty()) {
            lines.pop();
        }
        let placement = if record["widgetPlacement"] == "belowEditor" {
            Placement::BelowComposer
        } else {
            Placement::AboveComposer
        };
        let existing = self.widgets.iter().position(|widget| widget.key == key);
        match existing {
            Some(index) if lines.is_empty() => {
                self.widgets.remove(index);
            }
            Some(index) => {
                self.widgets[index] = Widget {
                    key: key.to_string(),
                    placement,
                    lines,
                }
            }
            None if !lines.is_empty() => self.widgets.push(Widget {
                key: key.to_string(),
                placement,
                lines,
            }),
            None => {}
        }
    }

    pub fn clear(&mut self) {
        self.widgets.clear();
        self.expanded.clear();
    }

    pub fn toggle(&mut self, key: &str) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
    }

    pub fn render(&self, placement: Placement, cx: &mut Context<ChatView>) -> Option<AnyElement> {
        let widgets: Vec<&Widget> = self
            .widgets
            .iter()
            .filter(|widget| widget.placement == placement)
            .collect();
        if widgets.is_empty() {
            return None;
        }
        let settings = Settings::get(cx);
        let font_family = settings.buffer_font_family.clone();
        let font_size = settings.buffer_font_size;
        let colors = cx.theme().colors.clone();
        let cards: Vec<AnyElement> = widgets
            .into_iter()
            .map(|widget| {
                let open = self.expanded.contains(&widget.key);
                let key = widget.key.clone();
                let header = h_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(render_line(&widget.lines[0], open)),
                    )
                    .child(disclosure_button(
                        ElementId::Name(format!("widget-disclosure:{key}").into()),
                        open,
                        "details",
                        cx.listener(move |this, _, _, cx| {
                            this.widgets.toggle(&key);
                            cx.notify();
                        }),
                    ));
                let rest =
                    open.then(|| widget.lines[1..].iter().map(|line| render_line(line, true)));
                v_flex()
                    .pl(px(8.))
                    .pr(px(4.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.panel_background)
                    .child(header)
                    .children(rest.into_iter().flatten())
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .gap(px(4.))
                .font_family(font_family)
                .text_size(px(font_size))
                .children(cards)
                .into_any_element(),
        )
    }
}

fn render_line(line: &StyledLine, wrap: bool) -> AnyElement {
    let text = if line.text.is_empty() {
        StyledText::new(" ")
    } else {
        StyledText::new(line.text.clone()).with_highlights(line.highlights.clone())
    };
    if wrap {
        return div().child(text).into_any_element();
    }
    div().truncate().child(text).into_any_element()
}
