//! A read-only view of one project file over the whole window, opened from
//! the Files panel: markdown rendered, other text with syntax colors and
//! line numbers, binary files only named.

use std::io::Read as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, CursorStyle, Entity, EventEmitter, FocusHandle,
    FontWeight, Hsla, KeyBinding, ListAlignment, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, ScrollHandle, ScrollWheelEvent, SharedString, Size,
    Subscription, Task, Window, actions, canvas, div, list, point, px, relative, size,
};

use crate::markdown::{self, Node};
use crate::selectable::{SelectableText, SpanStyle, TextSelection};
use crate::settings::Settings;
use crate::ui::prelude::*;
use crate::ui::{SyntaxColors, Tooltip};

actions!(file_viewer, [Close, CopySelection]);

const KEY_CONTEXT: &str = "FileViewer";
/// Larger files show their beginning.
const MAX_BYTES: usize = 2 << 20;
/// Markdown is laid out at once, not per visible line; larger files show
/// as text.
const MAX_MARKDOWN_BYTES: usize = 300 << 10;
/// A NUL byte in this many leading bytes marks a binary file.
const BINARY_PROBE_BYTES: usize = 8 << 10;
/// The card cannot be resized below this.
const MIN_WIDTH: Pixels = px(360.);
const MIN_HEIGHT: Pixels = px(160.);

pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("escape", Close, Some(KEY_CONTEXT)),
        KeyBinding::new(
            &crate::keys::binding("cmd-c"),
            CopySelection,
            Some(KEY_CONTEXT),
        ),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([KeyBinding::new(
        "ctrl-shift-c",
        CopySelection,
        Some(KEY_CONTEXT),
    )]);
}

pub enum FileViewerEvent {
    Closed,
}

enum Content {
    Loading,
    Binary,
    Failed(String),
    Markdown(Vec<Node>),
    Text {
        lines: Vec<SharedString>,
        highlights: Vec<Vec<(Range<usize>, Hsla)>>,
    },
}

/// What the background read produced.
struct Loaded {
    /// Bytes past `MAX_BYTES` that were not read.
    cut: bool,
    /// The whole file, in bytes.
    size: u64,
    content: Content,
    /// A markdown file too large to render.
    markdown_as_text: bool,
}

/// A scrollbar thumb drag in progress.
#[derive(Clone, Copy)]
struct ScrollDrag {
    mouse_y: Pixels,
    scroll: Pixels,
    content: Pixels,
    track: Pixels,
}

/// The card edges a frame drag moves; none of them moves the whole card.
#[derive(Clone, Copy, Default)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn cursor(self) -> CursorStyle {
        match (self.left || self.right, self.top || self.bottom) {
            (true, false) => CursorStyle::ResizeLeftRight,
            (false, true) => CursorStyle::ResizeUpDown,
            (true, true) if self.left == self.top => CursorStyle::ResizeUpLeftDownRight,
            (true, true) => CursorStyle::ResizeUpRightDownLeft,
            (false, false) => CursorStyle::Arrow,
        }
    }
}

/// A move or resize of the card in progress.
#[derive(Clone, Copy)]
struct FrameDrag {
    edges: Edges,
    mouse: Point<Pixels>,
    start: Bounds<Pixels>,
}

pub struct FileViewer {
    path: PathBuf,
    title: String,
    content: Content,
    cut: bool,
    markdown_as_text: bool,
    size: u64,
    list: ListState,
    markdown_scroll: ScrollHandle,
    scroll_drag: Option<ScrollDrag>,
    /// The card position and size within the overlay once moved or
    /// resized; centered at its default size before that.
    frame: Option<Bounds<Pixels>>,
    frame_drag: Option<FrameDrag>,
    /// The overlay and the card as last laid out, in window coordinates.
    area: Bounds<Pixels>,
    card: Bounds<Pixels>,
    selection: Entity<TextSelection>,
    focus: FocusHandle,
    focused: bool,
    _selection_subscription: Subscription,
    _load: Task<()>,
}

impl EventEmitter<FileViewerEvent> for FileViewer {}

impl FileViewer {
    /// `title` is the path shown, relative to the project; `frame` is where
    /// the previous viewer was left, if anywhere.
    pub fn new(
        path: PathBuf,
        title: String,
        frame: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let mut selection = TextSelection::new(focus.clone());
        if let Some(parent) = path.parent() {
            selection = selection.with_link_base(parent.to_path_buf());
        }
        let selection = cx.new(|_| selection);
        let selection_subscription = cx.observe(&selection, |_, _, cx| cx.notify());
        let syntax = cx.theme().colors.syntax;
        let read_path = path.clone();
        let read = cx.background_spawn(async move { load(&read_path, &syntax) });
        let load_task = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(loaded) => {
                        if let Content::Text { lines, .. } = &loaded.content {
                            this.list.reset(lines.len());
                        }
                        this.cut = loaded.cut;
                        this.size = loaded.size;
                        this.markdown_as_text = loaded.markdown_as_text;
                        this.content = loaded.content;
                    }
                    Err(error) => this.content = Content::Failed(error),
                }
                cx.notify();
            })
            .ok();
        });
        Self {
            path,
            title,
            content: Content::Loading,
            cut: false,
            markdown_as_text: false,
            size: 0,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            markdown_scroll: ScrollHandle::new(),
            scroll_drag: None,
            frame,
            frame_drag: None,
            area: Bounds::default(),
            card: Bounds::default(),
            selection,
            focus,
            focused: false,
            _selection_subscription: selection_subscription,
            _load: load_task,
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(FileViewerEvent::Closed);
    }

    /// Where the card was moved or resized to, for the next viewer.
    pub fn frame(&self) -> Option<Bounds<Pixels>> {
        self.frame
    }

    fn start_frame_drag(&mut self, edges: Edges, mouse: Point<Pixels>, cx: &mut Context<Self>) {
        let start = match self.frame {
            Some(frame) => fit(frame, self.area.size),
            None if self.card.size.width > px(0.) => {
                Bounds::new(self.card.origin - self.area.origin, self.card.size)
            }
            // Not laid out yet.
            None => return,
        };
        self.frame_drag = Some(FrameDrag {
            edges,
            mouse,
            start,
        });
        cx.notify();
    }

    fn drag_frame(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.frame_drag else {
            return;
        };
        // Released outside the overlay: the move arrives without the button.
        if event.pressed_button != Some(MouseButton::Left) {
            self.frame_drag = None;
            cx.notify();
            return;
        }
        let delta = event.position - drag.mouse;
        let area = self.area.size;
        let start = drag.start;
        let edges = drag.edges;
        let frame = if !(edges.left || edges.right || edges.top || edges.bottom) {
            fit(Bounds::new(start.origin + delta, start.size), area)
        } else {
            let mut left = start.left();
            let mut right = start.right();
            let mut top = start.top();
            let mut bottom = start.bottom();
            if edges.left {
                left = (left + delta.x).clamp(px(0.), (right - MIN_WIDTH).max(px(0.)));
            }
            if edges.right {
                right = (right + delta.x).clamp((left + MIN_WIDTH).min(area.width), area.width);
            }
            if edges.top {
                top = (top + delta.y).clamp(px(0.), (bottom - MIN_HEIGHT).max(px(0.)));
            }
            if edges.bottom {
                bottom = (bottom + delta.y).clamp((top + MIN_HEIGHT).min(area.height), area.height);
            }
            Bounds::new(point(left, top), size(right - left, bottom - top))
        };
        self.frame = Some(frame);
        cx.notify();
    }

    fn end_frame_drag(&mut self, cx: &mut Context<Self>) {
        if self.frame_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Invisible strips over the card edges and corners that resize it.
    fn render_resize_handles(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // Mostly outside the card, so the scrollbar keeps its whole width.
        const OUTSIDE: Pixels = px(4.);
        const THICKNESS: Pixels = px(6.);
        const CORNER: Pixels = px(12.);
        let edges = [
            (false, false, true, false),
            (false, false, false, true),
            (true, false, false, false),
            (false, true, false, false),
            (true, false, true, false),
            (false, true, true, false),
            (true, false, false, true),
            (false, true, false, true),
        ];
        edges
            .into_iter()
            .enumerate()
            .map(|(index, (left, right, top, bottom))| {
                let edges = Edges {
                    left,
                    right,
                    top,
                    bottom,
                };
                let corner = (left || right) && (top || bottom);
                let mut handle = div().id(("file-viewer-resize", index)).absolute();
                handle = match (left, right) {
                    (true, _) => handle
                        .left(-OUTSIDE)
                        .w(if corner { CORNER } else { THICKNESS }),
                    (_, true) => handle
                        .right(-OUTSIDE)
                        .w(if corner { CORNER } else { THICKNESS }),
                    _ => handle.left(CORNER - OUTSIDE).right(CORNER - OUTSIDE),
                };
                handle = match (top, bottom) {
                    (true, _) => handle
                        .top(-OUTSIDE)
                        .h(if corner { CORNER } else { THICKNESS }),
                    (_, true) => handle
                        .bottom(-OUTSIDE)
                        .h(if corner { CORNER } else { THICKNESS }),
                    _ => handle.top(CORNER - OUTSIDE).bottom(CORNER - OUTSIDE),
                };
                handle
                    .cursor(edges.cursor())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.start_frame_drag(edges, event.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selection.read(cx).selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn facts(&self) -> String {
        if matches!(self.content, Content::Loading | Content::Failed(_)) {
            return String::new();
        }
        let size = if self.size < 1024 {
            format!("{} B", self.size)
        } else if self.size < 1 << 20 {
            format!("{:.1} KB", self.size as f64 / 1024.)
        } else {
            format!("{:.1} MB", self.size as f64 / (1 << 20) as f64)
        };
        match &self.content {
            Content::Text { lines, .. } => match lines.len() {
                1 => format!("1 line · {size}"),
                count => format!("{count} lines · {size}"),
            },
            _ => size,
        }
    }

    fn note(&self) -> Option<String> {
        let mut notes = Vec::new();
        if self.cut {
            notes.push("Showing the first 2 MB of the file.");
        }
        if self.markdown_as_text {
            notes.push("Shown as text: markdown larger than 300 KB is not rendered.");
        }
        (!notes.is_empty()).then(|| notes.join(" "))
    }

    /// Track height, whole content height and the current offset, or `None`
    /// when the content fits and needs no scrollbar.
    fn scroll_metrics(&self) -> Option<(Pixels, Pixels, Pixels)> {
        let (track, max, offset) = match &self.content {
            Content::Text { .. } => (
                self.list.viewport_bounds().size.height,
                self.list.max_offset_for_scrollbar().height,
                -self.list.scroll_px_offset_for_scrollbar().y,
            ),
            Content::Markdown(_) => (
                self.markdown_scroll.bounds().size.height,
                self.markdown_scroll.max_offset().height,
                -self.markdown_scroll.offset().y,
            ),
            _ => return None,
        };
        if track <= px(0.) || max <= px(0.) {
            return None;
        }
        Some((track, track + max, offset.clamp(px(0.), max)))
    }

    fn apply_scroll(&self, desired: Pixels) {
        let desired = desired.max(px(0.));
        match &self.content {
            Content::Text { .. } => {
                self.list.set_offset_from_scrollbar(point(px(0.), -desired));
            }
            Content::Markdown(_) => {
                let max = self.markdown_scroll.max_offset().height;
                self.markdown_scroll
                    .set_offset(point(px(0.), -desired.min(max)));
            }
            _ => {}
        }
    }

    fn start_drag(&mut self, mouse_y: Pixels, cx: &mut Context<Self>) {
        let Some((track, content, offset)) = self.scroll_metrics() else {
            return;
        };
        if matches!(self.content, Content::Text { .. }) {
            self.list.scrollbar_drag_started();
        }
        self.scroll_drag = Some(ScrollDrag {
            mouse_y,
            scroll: offset,
            content,
            track,
        });
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.scroll_drag.take().is_some() {
            self.list.scrollbar_drag_ended();
            cx.notify();
        }
    }

    fn drag_scroll(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.scroll_drag else {
            return;
        };
        // Released outside the card: the move arrives without the button.
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_drag(cx);
            return;
        }
        let scale = f32::from(drag.content) / f32::from(drag.track).max(1.);
        self.apply_scroll(drag.scroll + (event.position.y - drag.mouse_y) * scale);
        cx.notify();
    }

    /// A thin draggable scrollbar over the right edge of the body.
    fn render_scrollbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (track, content, offset) = self.scroll_metrics()?;
        let colors = cx.theme().colors.clone();
        let thumb = thumb_height(track, content);
        let thumb_y = (track - thumb) * (f32::from(offset) / f32::from(content - track));
        let top = match &self.content {
            Content::Text { .. } => self.list.viewport_bounds().origin.y,
            _ => self.markdown_scroll.bounds().origin.y,
        };
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(10.))
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    let delta = event.delta.pixel_delta(px(20.));
                    let offset = match &this.content {
                        Content::Text { .. } => -this.list.scroll_px_offset_for_scrollbar().y,
                        Content::Markdown(_) => -this.markdown_scroll.offset().y,
                        _ => return,
                    };
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
                        this.start_drag(event.position.y, cx);
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
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                this.start_drag(event.position.y, cx);
                                cx.stop_propagation();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: String| {
            h_flex()
                .flex_1()
                .justify_center()
                .py(px(40.))
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
                .into_any_element()
        };
        match &self.content {
            Content::Loading => message("Loading…".to_string()),
            Content::Binary => message("Binary file: open it in an editor.".to_string()),
            Content::Failed(error) => message(format!("Could not read the file: {error}")),
            Content::Text { lines, .. } if lines.is_empty() => message("Empty file.".to_string()),
            Content::Markdown(nodes) => {
                let settings = Settings::get(cx);
                div()
                    .id("file-viewer-markdown")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.markdown_scroll)
                    .px(px(20.))
                    .py(px(14.))
                    .text_size(px(settings.agent_font_size))
                    .line_height(px(settings.agent_line_height()))
                    .child(markdown::render("file", nodes, &self.selection, window, cx))
                    .into_any_element()
            }
            Content::Text { lines, .. } => {
                let settings = Settings::get(cx);
                let font_size = px(settings.buffer_font_size);
                let line_height = px(settings.buffer_line_height());
                let digits = lines.len().to_string().len();
                // Wide enough for the longest number in the code font.
                let gutter = px(settings.buffer_font_size * 0.62 * digits as f32 + 16.);
                list(
                    self.list.clone(),
                    cx.processor(move |this, index: usize, _, cx| {
                        let Content::Text { lines, highlights } = &this.content else {
                            return div().into_any_element();
                        };
                        let Some(line) = lines.get(index) else {
                            return div().into_any_element();
                        };
                        let spans: Vec<(Range<usize>, SpanStyle)> = highlights
                            .get(index)
                            .into_iter()
                            .flatten()
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
                        // A list item is laid out as a root node, and a flex
                        // root shrinks to its content: the text would then
                        // get its own rounded width and wrap its last glyph.
                        h_flex()
                            .w_full()
                            .items_start()
                            .pr(px(14.))
                            .child(
                                div()
                                    .flex_none()
                                    .w(gutter)
                                    .pr(px(10.))
                                    .flex()
                                    .justify_end()
                                    .text_size(font_size)
                                    .line_height(line_height)
                                    .text_color(Color::Placeholder.text(cx))
                                    .child((index + 1).to_string()),
                            )
                            .child(
                                div().flex_1().min_w(px(0.)).child(
                                    SelectableText::new(
                                        SharedString::from(format!("file:{index}")),
                                        if line.is_empty() {
                                            SharedString::new_static(" ")
                                        } else {
                                            line.clone()
                                        },
                                        &this.selection,
                                    )
                                    .spans(spans)
                                    .text_size(font_size, line_height)
                                    .monospace(),
                                ),
                            )
                            .into_any_element()
                    }),
                )
                .font_family(settings.buffer_font_family.clone())
                .flex_1()
                .py(px(8.))
                .into_any_element()
            }
        }
    }
}

impl Render for FileViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !std::mem::replace(&mut self.focused, true) {
            window.focus(&self.focus);
        }
        let colors = cx.theme().colors.clone();
        let note = self.note();
        let facts = self.facts();
        let body = self.render_body(window, cx);
        let scrollbar = self.render_scrollbar(cx);
        let resize_handles = self.render_resize_handles(cx);
        let path = self.path.clone();
        let frame = self.frame.map(|frame| fit(frame, self.area.size));
        let drag_cursor = self
            .frame_drag
            .map(|drag| drag.edges.cursor())
            .filter(|&cursor| cursor != CursorStyle::Arrow);
        let entity = cx.entity();
        let card_entity = entity.clone();
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.45))
            .occlude()
            .when_some(drag_cursor, |this, cursor| this.cursor(cursor))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close(cx)),
            )
            // On the overlay, so drags keep going when the mouse leaves the card.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_scroll(event, cx);
                this.drag_frame(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_drag(cx);
                    this.end_frame_drag(cx);
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| entity.update(cx, |this, _| this.area = bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .key_context(KEY_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &Close, _, cx| this.close(cx)))
                    .on_action(cx.listener(Self::copy_selection))
                    .map(|this| match frame {
                        Some(frame) => this
                            .absolute()
                            .left(frame.origin.x)
                            .top(frame.origin.y)
                            .w(frame.size.width)
                            .h(frame.size.height),
                        None => this
                            .relative()
                            .w(px(1100.))
                            .max_w(relative(0.94))
                            .h(relative(0.88)),
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                card_entity.update(cx, |this, _| this.card = bounds)
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        v_flex()
                            .size_full()
                            .rounded(px(10.))
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .shadow_lg()
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap(px(8.))
                                    .px(px(14.))
                                    .py(px(8.))
                                    // overflow_hidden clips to a rectangle, so the header rounds itself.
                                    .rounded_t(px(9.))
                                    .border_b_1()
                                    .border_color(colors.border_variant)
                                    .bg(colors.panel_background)
                                    .cursor(CursorStyle::Arrow)
                                    // Drags the card; buttons prevent default on press.
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                            if !window.default_prevented() {
                                                this.start_frame_drag(
                                                    Edges::default(),
                                                    event.position,
                                                    cx,
                                                );
                                            }
                                        }),
                                    )
                                    .child(
                                        Icon::new(IconName::File)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        div().flex_1().min_w(px(0.)).child(
                                            Label::new(self.title.clone())
                                                .size(LabelSize::Small)
                                                .weight(FontWeight::MEDIUM)
                                                .monospace()
                                                .truncate(),
                                        ),
                                    )
                                    .child(
                                        Label::new(facts)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Placeholder),
                                    )
                                    .child(
                                        IconButton::new("file-viewer-open", IconName::FolderOpen)
                                            .size(ButtonSize::Compact)
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Open in editor"))
                                            .on_click(move |_, _, _| {
                                                crate::hub::open_in_editor(&path)
                                            }),
                                    )
                                    .child(
                                        IconButton::new("file-viewer-close", IconName::Close)
                                            .size(ButtonSize::Compact)
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::with_keys("Close", "Esc"))
                                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                                    ),
                            )
                            .children(note.map(|note| {
                                h_flex()
                                    .flex_none()
                                    .gap(px(6.))
                                    .px(px(14.))
                                    .py(px(5.))
                                    .border_b_1()
                                    .border_color(colors.border_variant)
                                    .child(
                                        Icon::new(IconName::Info)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(note)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                            }))
                            .child(
                                // Flex column: the body's flex_1 needs a flex parent
                                // to get the viewport height.
                                v_flex()
                                    .relative()
                                    .flex_1()
                                    .min_h(px(0.))
                                    .child(body)
                                    .children(scrollbar),
                            ),
                    )
                    .children(resize_handles),
            )
    }
}

/// `frame` shrunk and shifted to lie within an area of `area` size.
fn fit(frame: Bounds<Pixels>, area: Size<Pixels>) -> Bounds<Pixels> {
    if area.width <= px(0.) || area.height <= px(0.) {
        return frame;
    }
    let width = frame.size.width.min(area.width);
    let height = frame.size.height.min(area.height);
    Bounds::new(
        point(
            frame.origin.x.clamp(px(0.), area.width - width),
            frame.origin.y.clamp(px(0.), area.height - height),
        ),
        size(width, height),
    )
}

fn thumb_height(track: Pixels, content: Pixels) -> Pixels {
    (track * (f32::from(track) / f32::from(content))).max(px(24.))
}

fn load(path: &Path, syntax: &SyntaxColors) -> Result<Loaded, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let size = file.metadata().map_or(0, |metadata| metadata.len());
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
    if probe.contains(&0) {
        return Ok(Loaded {
            cut: false,
            size,
            content: Content::Binary,
            markdown_as_text: false,
        });
    }
    let cut = bytes.len() > MAX_BYTES;
    bytes.truncate(MAX_BYTES);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    // End at a whole line when the tail was cut.
    if cut && let Some(newline) = text.rfind('\n') {
        text.truncate(newline + 1);
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    let is_markdown = matches!(extension.as_deref(), Some("md" | "markdown" | "mdx"));
    if is_markdown && text.len() <= MAX_MARKDOWN_BYTES {
        return Ok(Loaded {
            content: Content::Markdown(markdown::parse(&text)),
            cut,
            size,
            markdown_as_text: false,
        });
    }
    let language = extension.unwrap_or_else(|| name.clone());
    let lines = split_lines(&text);
    let highlights = line_highlights(&text, &lines, &language, syntax);
    let lines = lines
        .into_iter()
        .map(|(_, line)| SharedString::from(line.to_string()))
        .collect();
    Ok(Loaded {
        content: Content::Text { lines, highlights },
        cut,
        size,
        markdown_as_text: is_markdown,
    })
}

/// Lines without their `\n` or `\r\n`, with where each starts in `text`.
fn split_lines(text: &str) -> Vec<(usize, &str)> {
    let mut start = 0;
    text.split_inclusive('\n')
        .map(|chunk| {
            let line = chunk.strip_suffix('\n').unwrap_or(chunk);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let item = (start, line);
            start += chunk.len();
            item
        })
        .collect()
}

/// Syntax colors per line, as ranges within the line; empty for unknown
/// languages and files too large to highlight.
fn line_highlights(
    text: &str,
    lines: &[(usize, &str)],
    language: &str,
    syntax: &SyntaxColors,
) -> Vec<Vec<(Range<usize>, Hsla)>> {
    let Some(spans) = crate::highlight::highlight_once(text, language, syntax) else {
        return Vec::new();
    };
    lines
        .iter()
        .map(|&(start, line)| {
            let end = start + line.len();
            let first = spans.partition_point(|(range, _)| range.end <= start);
            spans[first..]
                .iter()
                .take_while(|(range, _)| range.start < end)
                .map(|(range, color)| {
                    (
                        range.start.max(start) - start..range.end.min(end) - start,
                        *color,
                    )
                })
                .filter(|(range, _)| !range.is_empty())
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_their_offsets_without_line_endings() {
        assert_eq!(
            split_lines("a\r\nbc\n\nd"),
            vec![(0, "a"), (3, "bc"), (6, ""), (7, "d")]
        );
        assert!(split_lines("").is_empty());
    }

    #[test]
    fn frames_fit_inside_the_overlay() {
        let area = size(px(800.), px(600.));
        let frame =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        assert_eq!(
            fit(frame(700., -50., 400., 300.), area),
            frame(400., 0., 400., 300.)
        );
        assert_eq!(
            fit(frame(10., 10., 1000., 900.), area),
            frame(0., 0., 800., 600.)
        );
        assert_eq!(
            fit(frame(10., 20., 300., 200.), area),
            frame(10., 20., 300., 200.)
        );
    }

    #[test]
    fn highlights_stay_inside_their_lines() {
        let color = Hsla {
            h: 0.,
            s: 0.,
            l: 0.5,
            a: 1.,
        };
        let syntax = SyntaxColors {
            comment: color,
            keyword: color,
            string: color,
            number: color,
            function: color,
            r#type: color,
            constant: color,
            operator: color,
            punctuation: Some(color),
            property: color,
            tag: color,
            attribute: color,
        };
        let text = "//! A comment\r\n#[cfg(windows)]\r\nfn main() {}\r\n";
        let lines = split_lines(text);
        let highlights = line_highlights(text, &lines, "rs", &syntax);
        assert_eq!(highlights.len(), lines.len());
        assert!(!highlights[1].is_empty());
        for ((_, line), spans) in lines.iter().zip(&highlights) {
            assert!(spans.iter().all(|(range, _)| range.end <= line.len()));
        }
    }
}
