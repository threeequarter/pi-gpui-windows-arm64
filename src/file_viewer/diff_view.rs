//! The viewed file's change against HEAD, laid out for the viewer's two diff
//! modes: one column with old and new line numbers, or the old and new sides
//! next to each other.

use std::ops::Range;

use gpui::{AnyElement, App, Entity, Hsla, Pixels, SharedString, div, px};

use crate::diff::{self, DiffLine, FileDiff, LineKind};
use crate::selectable::{SelectableText, SpanStyle, TextSelection};
use crate::ui::SyntaxColors;
use crate::ui::prelude::*;

pub(super) struct DiffView {
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    lines: Vec<DiffLine>,
    texts: Vec<SharedString>,
    highlights: Vec<Vec<(Range<usize>, Hsla)>>,
    /// The old and the new line number of each line.
    numbers: Vec<(Option<usize>, Option<usize>)>,
    split_rows: Vec<SplitRow>,
}

/// A row of the side-by-side layout, by index into the diff lines.
#[derive(Clone, Copy)]
enum SplitRow {
    Hunk(usize),
    Pair(Option<usize>, Option<usize>),
}

/// Sizes shared by every row of one frame.
#[derive(Clone, Copy)]
pub(super) struct RowStyle {
    pub font_size: Pixels,
    pub line_height: Pixels,
    /// Width of one line number column.
    pub gutter: Pixels,
}

impl DiffView {
    pub(super) fn new(file: FileDiff, syntax: &SyntaxColors) -> Self {
        let texts: Vec<String> = file
            .lines
            .iter()
            .map(|line| line.text.replace('\t', "    "))
            .collect();
        let highlights = diff::line_highlights(&file.path, &file.lines, &texts, syntax);
        Self {
            additions: file.additions,
            deletions: file.deletions,
            binary: file.binary,
            numbers: line_numbers(&file.lines),
            split_rows: split_rows(&file.lines),
            texts: texts.into_iter().map(SharedString::from).collect(),
            highlights,
            lines: file.lines,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub(super) fn inline_row_count(&self) -> usize {
        self.lines.len()
    }

    pub(super) fn split_row_count(&self) -> usize {
        self.split_rows.len()
    }

    /// The inline layout's selectable blocks, in row order.
    pub(super) fn inline_blocks(&self) -> Vec<(SharedString, SharedString)> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind != LineKind::Hunk)
            .map(|(index, _)| {
                (
                    SharedString::from(format!("diff:{index}")),
                    self.texts[index].clone(),
                )
            })
            .collect()
    }

    /// The split layout's two sides, each selectable on its own top to bottom.
    pub(super) fn split_blocks(&self) -> Vec<Vec<(SharedString, SharedString)>> {
        let mut old = Vec::new();
        let mut new = Vec::new();
        for row in &self.split_rows {
            let SplitRow::Pair(old_index, new_index) = row else {
                continue;
            };
            if let Some(index) = old_index {
                old.push((
                    SharedString::from(format!("old:{index}")),
                    self.texts[*index].clone(),
                ));
            }
            if let Some(index) = new_index {
                new.push((
                    SharedString::from(format!("new:{index}")),
                    self.texts[*index].clone(),
                ));
            }
        }
        vec![old, new]
    }

    /// Digits of the largest line number, for the gutter width.
    pub(super) fn number_digits(&self) -> usize {
        self.numbers
            .iter()
            .filter_map(|&(old, new)| old.max(new))
            .max()
            .unwrap_or(1)
            .to_string()
            .len()
    }

    pub(super) fn render_inline_row(
        &self,
        index: usize,
        selection: &Entity<TextSelection>,
        style: RowStyle,
        cx: &App,
    ) -> AnyElement {
        let Some(line) = self.lines.get(index) else {
            return div().into_any_element();
        };
        if line.kind == LineKind::Hunk {
            return self.render_hunk(index, style, cx);
        }
        let colors = &cx.theme().colors;
        let (old, new) = self.numbers[index];
        let sign = match line.kind {
            LineKind::Added => "+",
            LineKind::Removed => "−",
            _ => " ",
        };
        h_flex()
            .w_full()
            .items_start()
            .pr(px(14.))
            .text_size(style.font_size)
            .line_height(style.line_height)
            .when_some(line_background(line.kind, cx), |this, background| {
                this.bg(background)
            })
            .child(render_number(old, style, cx))
            .child(render_number(new, style, cx))
            .child(
                div()
                    .flex_none()
                    .w(px(16.))
                    .text_color(colors.text_placeholder)
                    .child(sign),
            )
            .child(div().flex_1().min_w(px(0.)).child(self.render_text(
                format!("diff:{index}"),
                index,
                selection,
                style,
            )))
            .into_any_element()
    }

    pub(super) fn render_split_row(
        &self,
        row: usize,
        selection: &Entity<TextSelection>,
        style: RowStyle,
        cx: &App,
    ) -> AnyElement {
        match self.split_rows.get(row) {
            Some(SplitRow::Hunk(index)) => self.render_hunk(*index, style, cx),
            // A plain flex row stretches both halves to the taller one.
            Some(SplitRow::Pair(old, new)) => div()
                .flex()
                .w_full()
                .text_size(style.font_size)
                .line_height(style.line_height)
                .child(
                    self.render_half(*old, Side::Old, selection, style, cx)
                        .border_r_1()
                        .border_color(cx.theme().colors.border_variant),
                )
                .child(self.render_half(*new, Side::New, selection, style, cx))
                .into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn render_half(
        &self,
        index: Option<usize>,
        side: Side,
        selection: &Entity<TextSelection>,
        style: RowStyle,
        cx: &App,
    ) -> gpui::Div {
        let half = h_flex().flex_1().min_w(px(0.)).items_start().pr(px(10.));
        let Some(index) = index else {
            return half.bg(cx.theme().colors.border_variant.opacity(0.35));
        };
        let line = &self.lines[index];
        let (old, new) = self.numbers[index];
        let (number, key) = match side {
            Side::Old => (old, format!("old:{index}")),
            Side::New => (new, format!("new:{index}")),
        };
        half.when_some(line_background(line.kind, cx), |this, background| {
            this.bg(background)
        })
        .child(render_number(number, style, cx))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(self.render_text(key, index, selection, style)),
        )
    }

    fn render_hunk(&self, index: usize, style: RowStyle, cx: &App) -> AnyElement {
        let colors = &cx.theme().colors;
        div()
            .w_full()
            .px(px(10.))
            .my(px(2.))
            .text_size(style.font_size)
            .line_height(style.line_height)
            .bg(colors.info_background.opacity(0.5))
            .text_color(colors.text_muted)
            .child(self.texts[index].clone())
            .into_any_element()
    }

    fn render_text(
        &self,
        key: String,
        index: usize,
        selection: &Entity<TextSelection>,
        style: RowStyle,
    ) -> SelectableText {
        let text = &self.texts[index];
        let spans = self.highlights[index]
            .iter()
            .map(|(range, color)| {
                (
                    range.clone(),
                    SpanStyle {
                        color: Some(*color),
                        ..SpanStyle::default()
                    },
                )
            })
            .collect();
        SelectableText::new(
            key,
            // Keeps empty lines one line tall.
            if text.is_empty() {
                SharedString::new_static(" ")
            } else {
                text.clone()
            },
            selection,
        )
        .spans(spans)
        .text_size(style.font_size, style.line_height)
        .monospace()
    }
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

fn line_background(kind: LineKind, cx: &App) -> Option<Hsla> {
    let colors = &cx.theme().colors;
    match kind {
        LineKind::Added => Some(colors.success.opacity(0.14)),
        LineKind::Removed => Some(colors.error.opacity(0.14)),
        LineKind::Context | LineKind::Hunk => None,
    }
}

fn render_number(number: Option<usize>, style: RowStyle, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .w(style.gutter)
        .pr(px(10.))
        .flex()
        .justify_end()
        .text_color(cx.theme().colors.text_placeholder)
        .children(number.map(|number| number.to_string()))
}

/// Old and new line numbers, counted from each hunk header. A file without
/// headers (an untracked one) starts at line 1.
fn line_numbers(lines: &[DiffLine]) -> Vec<(Option<usize>, Option<usize>)> {
    let (mut old, mut new) = (1, 1);
    lines
        .iter()
        .map(|line| match line.kind {
            LineKind::Hunk => {
                if let Some(starts) = hunk_starts(&line.text) {
                    (old, new) = starts;
                }
                (None, None)
            }
            LineKind::Context => {
                let numbers = (Some(old), Some(new));
                old += 1;
                new += 1;
                numbers
            }
            LineKind::Removed => {
                old += 1;
                (Some(old - 1), None)
            }
            LineKind::Added => {
                new += 1;
                (None, Some(new - 1))
            }
        })
        .collect()
}

/// `@@ -12,5 +14,7 @@ fn main` → (12, 14).
fn hunk_starts(header: &str) -> Option<(usize, usize)> {
    let mut ranges = header.strip_prefix("@@ ")?.split(' ');
    let old = ranges.next()?.strip_prefix('-')?;
    let new = ranges.next()?.strip_prefix('+')?;
    let start = |range: &str| range.split(',').next()?.parse().ok();
    Some((start(old)?, start(new)?))
}

/// Context lines fill both sides; each run of removed lines is paired row by
/// row with the added lines that follow it.
fn split_rows(lines: &[DiffLine]) -> Vec<SplitRow> {
    fn flush(rows: &mut Vec<SplitRow>, removed: &mut Vec<usize>, added: &mut Vec<usize>) {
        for row in 0..removed.len().max(added.len()) {
            rows.push(SplitRow::Pair(
                removed.get(row).copied(),
                added.get(row).copied(),
            ));
        }
        removed.clear();
        added.clear();
    }
    let mut rows = Vec::new();
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        match line.kind {
            LineKind::Removed => {
                if !added.is_empty() {
                    flush(&mut rows, &mut removed, &mut added);
                }
                removed.push(index);
            }
            LineKind::Added => added.push(index),
            LineKind::Hunk => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(SplitRow::Hunk(index));
            }
            LineKind::Context => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(SplitRow::Pair(Some(index), Some(index)));
            }
        }
    }
    flush(&mut rows, &mut removed, &mut added);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: LineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn numbers_lines_from_hunk_headers() {
        let lines = vec![
            line(LineKind::Hunk, "@@ -10,3 +20,3 @@ fn main() {"),
            line(LineKind::Context, "keep"),
            line(LineKind::Removed, "old"),
            line(LineKind::Added, "new"),
            line(LineKind::Context, "keep"),
        ];
        assert_eq!(
            line_numbers(&lines),
            vec![
                (None, None),
                (Some(10), Some(20)),
                (Some(11), None),
                (None, Some(21)),
                (Some(12), Some(22)),
            ]
        );
    }

    #[test]
    fn pairs_removed_lines_with_the_added_lines_after_them() {
        let lines = vec![
            line(LineKind::Hunk, "@@ -1,4 +1,3 @@"),
            line(LineKind::Removed, "a"),
            line(LineKind::Removed, "b"),
            line(LineKind::Added, "c"),
            line(LineKind::Context, "d"),
            line(LineKind::Added, "e"),
        ];
        let rows: Vec<String> = split_rows(&lines)
            .into_iter()
            .map(|row| match row {
                SplitRow::Hunk(index) => format!("hunk {index}"),
                SplitRow::Pair(old, new) => format!("{old:?} {new:?}"),
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                "hunk 0",
                "Some(1) Some(3)",
                "Some(2) None",
                "Some(4) Some(4)",
                "None Some(5)",
            ]
        );
    }
}
