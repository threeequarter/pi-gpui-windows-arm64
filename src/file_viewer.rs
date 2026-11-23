//! A read-only view of one project file in its own window, opened from the
//! Files and Changes panels: markdown rendered, other text with syntax colors
//! and line numbers, and common raster images shown as previews. A file changed
//! against HEAD can also be shown as an inline or a side-by-side diff. The view
//! follows changes made to the file on disk.

use std::collections::HashMap;
use std::io::Read as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::channel::mpsc;
use gpui::{
    AnyElement, ClipboardItem, Context, Corners, Entity, FocusHandle, FontWeight, Hsla, KeyBinding,
    ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, ScrollHandle,
    ScrollWheelEvent, SharedString, Subscription, Task, Window, WindowControlArea, actions, div,
    list, point, px,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::diff;
use crate::hub::{TITLE_BAR_HEIGHT, TRAFFIC_LIGHT_PADDING, render_window_controls};
use crate::markdown::{self, Node};
use crate::platform;
use crate::project_watcher;
use crate::selectable::{SelectableText, SpanStyle, TextSelection};
use crate::settings::Settings;
use crate::ui::prelude::*;
use crate::ui::raster_preview::RasterPreview;
use crate::ui::scrollbar::{PreviewScroll, ScrollDrag, thumb_height};
use crate::ui::zoom::{self, DiagramView, DiagramZoom, Zoom};
use crate::ui::{SyntaxColors, ThemeColors, Tooltip};

use diff_view::{DiffView, RowStyle};

mod diff_view;
mod image_preview;
mod mermaid;
mod pdf;

actions!(file_viewer, [Close, CopySelection]);

const KEY_CONTEXT: &str = "FileViewer";
/// Larger files show their beginning.
const MAX_BYTES: usize = 2 << 20;
/// Markdown is laid out at once, not per visible line; larger files show
/// as text.
const MAX_MARKDOWN_BYTES: usize = 300 << 10;
/// A NUL byte in this many leading bytes marks a binary file.
const BINARY_PROBE_BYTES: usize = 8 << 10;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    File,
    InlineDiff,
    SplitDiff,
}

impl ViewMode {
    fn is_diff(self) -> bool {
        self != ViewMode::File
    }
}

/// The file's change against HEAD.
enum DiffState {
    Loading,
    /// Unchanged, ignored or outside a git work tree.
    Unchanged,
    Failed(String),
    Changed(DiffView),
}

enum Content {
    Loading,
    Binary,
    Image(RasterPreview),
    Failed(String),
    Markdown {
        nodes: Vec<Node>,
        diagrams: markdown::Diagrams,
    },
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

pub struct FileViewer {
    path: PathBuf,
    title: String,
    mode: ViewMode,
    content: Content,
    diff: DiffState,
    cut: bool,
    markdown_as_text: bool,
    size: u64,
    list: ListState,
    markdown_scroll: ScrollHandle,
    image_scroll: PreviewScroll,
    image_zoom: Option<f32>,
    diagram_views: HashMap<String, DiagramView>,
    scroll_drag: Option<ScrollDrag>,
    title_bar_drag_pending: bool,
    selection: Entity<TextSelection>,
    focus: FocusHandle,
    focused: bool,
    exporting_pdf: bool,
    _selection_subscription: Subscription,
    _load: Task<()>,
    _diff_load: Task<()>,
    _watcher: Option<RecommendedWatcher>,
    _watch: Task<()>,
}

impl FileViewer {
    /// `title` is the path shown, relative to the project. A diff mode falls
    /// back to the file when the file turns out unchanged.
    pub fn new(path: PathBuf, title: String, mode: ViewMode, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let selection = cx.new(|_| Self::selection_for(&path, focus.clone()));
        let selection_subscription = cx.observe(&selection, |_, _, cx| cx.notify());
        let mut this = Self {
            path,
            title,
            mode,
            content: Content::Loading,
            diff: DiffState::Loading,
            cut: false,
            markdown_as_text: false,
            size: 0,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            markdown_scroll: ScrollHandle::new(),
            image_scroll: PreviewScroll::default(),
            image_zoom: None,
            diagram_views: HashMap::new(),
            scroll_drag: None,
            title_bar_drag_pending: false,
            selection,
            focus,
            focused: false,
            exporting_pdf: false,
            _selection_subscription: selection_subscription,
            _load: Task::ready(()),
            _diff_load: Task::ready(()),
            _watcher: None,
            _watch: Task::ready(()),
        };
        this.start_load(cx);
        this.watch_file(cx);
        this
    }

    /// Loads another file into the same window.
    pub fn open(&mut self, path: PathBuf, title: String, mode: ViewMode, cx: &mut Context<Self>) {
        self.path = path;
        self.title = title;
        self.mode = mode;
        self.list = ListState::new(0, ListAlignment::Top, px(400.));
        self.markdown_scroll = ScrollHandle::new();
        self.image_scroll = PreviewScroll::default();
        self.image_zoom = None;
        self.diagram_views.clear();
        self.scroll_drag = None;
        // A fresh selection also clears the old one and re-points links.
        self.selection = cx.new(|_| Self::selection_for(&self.path, self.focus.clone()));
        self._selection_subscription = cx.observe(&self.selection, |_, _, cx| cx.notify());
        self.start_load(cx);
        self.watch_file(cx);
        cx.notify();
    }

    /// Reads the file again after it changes on disk. The folder is watched,
    /// not the file: editors often save by renaming a new file over the old
    /// one, which ends a watch on the file itself.
    fn watch_file(&mut self, cx: &mut Context<Self>) {
        self._watcher = None;
        self._watch = Task::ready(());
        let (Some(folder), Some(name)) = (self.path.parent(), self.path.file_name()) else {
            return;
        };
        let name = name.to_os_string();
        let (sender, receiver) = mpsc::unbounded::<()>();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else {
                return;
            };
            // Our own reads arrive as access events.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            if event
                .paths
                .iter()
                .any(|path| path.file_name() == Some(name.as_os_str()))
            {
                sender.unbounded_send(()).ok();
            }
        })
        .and_then(|mut watcher| {
            watcher
                .watch(folder, RecursiveMode::NonRecursive)
                .map(|()| watcher)
        });
        match watcher {
            Ok(watcher) => {
                self._watcher = Some(watcher);
                self._watch = project_watcher::when_settled(receiver, Self::reload, cx);
            }
            Err(error) => {
                eprintln!("pi-gpui: cannot watch {}: {error}", self.path.display());
            }
        }
    }

    fn selection_for(path: &Path, focus: FocusHandle) -> TextSelection {
        let mut selection = TextSelection::new(focus).with_copy_on_select();
        if let Some(parent) = path.parent() {
            selection = selection.with_link_base(parent.to_path_buf());
        }
        selection
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        self.set_content(Content::Loading, cx);
        self.cut = false;
        self.markdown_as_text = false;
        self.size = 0;
        self.diff = DiffState::Loading;
        self.read(false, cx);
    }

    fn set_content(&mut self, content: Content, cx: &mut Context<Self>) {
        if let Content::Markdown { diagrams, .. } = &content {
            self.diagram_views.retain(|key, _| diagrams.contains_key(key));
            for (key, diagram) in diagrams {
                if diagram.is_ok() {
                    self.diagram_views.entry(key.clone()).or_default();
                }
            }
        }
        let previews = match std::mem::replace(&mut self.content, content) {
            Content::Image(image) => vec![image],
            Content::Markdown { diagrams, .. } => {
                diagrams.into_values().filter_map(Result::ok).collect()
            }
            _ => Vec::new(),
        };
        if !previews.is_empty() {
            cx.defer(move |cx| {
                for image in previews.iter().flat_map(RasterPreview::images) {
                    cx.drop_image(image, None);
                }
            });
        }
    }

    /// Keeps the shown content and the scroll position until the new content
    /// arrives.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.read(true, cx);
    }

    fn read(&mut self, reload: bool, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let syntax = theme.colors.syntax;
        let colors = theme.colors.clone();
        let dark = theme.is_dark();
        let font_family = Settings::get(cx).ui_font_family.clone();
        let read_path = self.path.clone();
        let read =
            cx.background_spawn(async move { load(&read_path, &colors, dark, &font_family) });
        self._load = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(loaded) => {
                        this.cut = loaded.cut;
                        this.size = loaded.size;
                        this.markdown_as_text = loaded.markdown_as_text;
                        this.set_content(loaded.content, cx);
                    }
                    Err(error) => this.set_content(Content::Failed(error), cx),
                }
                if reload {
                    // The selected offsets point into the old text.
                    this.selection
                        .update(cx, |selection, cx| selection.clear(cx));
                }
                this.update_list(reload, cx);
                cx.notify();
            })
            .ok();
        });
        let diff_path = self.path.clone();
        let read_diff = cx.background_spawn(async move {
            diff::load_file_diff(&diff_path)
                .map(|file| file.map(|file| DiffView::new(file, &syntax)))
        });
        self._diff_load = cx.spawn(async move |this, cx| {
            let result = read_diff.await;
            this.update(cx, |this, cx| {
                this.diff = match result {
                    Ok(Some(view)) => DiffState::Changed(view),
                    Ok(None) => DiffState::Unchanged,
                    Err(error) => {
                        eprintln!(
                            "pi-gpui: git diff of {} failed: {error}",
                            this.path.display()
                        );
                        DiffState::Failed(error)
                    }
                };
                if this.mode.is_diff() && !matches!(this.diff, DiffState::Changed(_)) {
                    this.mode = ViewMode::File;
                }
                this.update_list(reload, cx);
                cx.notify();
            })
            .ok();
        });
    }

    fn set_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.scroll_drag = None;
        self.reset_list();
        self.sync_selection_blocks(cx);
        cx.notify();
    }

    /// Rows of the virtual list in the current mode; `None` when the body is
    /// not a list (markdown, messages).
    fn list_rows(&self) -> Option<usize> {
        match (self.mode, &self.content, &self.diff) {
            (ViewMode::File, Content::Text { lines, .. }, _) => Some(lines.len()),
            (ViewMode::InlineDiff, _, DiffState::Changed(view)) => Some(view.inline_row_count()),
            (ViewMode::SplitDiff, _, DiffState::Changed(view)) => Some(view.split_row_count()),
            _ => None,
        }
    }

    fn reset_list(&mut self) {
        self.list.reset(self.list_rows().unwrap_or(0));
    }

    /// Resets the list for new rows; a reload keeps the scroll position.
    fn update_list(&mut self, reload: bool, cx: &mut Context<Self>) {
        let top = self.list.logical_scroll_top();
        self.reset_list();
        if reload {
            self.list.scroll_to(top);
        }
        self.sync_selection_blocks(cx);
    }

    /// Registers the current mode's selectable lines, so a drag can cross
    /// them; markdown keeps selection inside one block.
    fn sync_selection_blocks(&mut self, cx: &mut Context<Self>) {
        let groups = match (self.mode, &self.content, &self.diff) {
            (ViewMode::File, Content::Text { lines, .. }, _) => vec![
                lines
                    .iter()
                    .enumerate()
                    .map(|(index, line)| {
                        (SharedString::from(format!("file:{index}")), line.clone())
                    })
                    .collect(),
            ],
            (ViewMode::InlineDiff, _, DiffState::Changed(view)) => vec![view.inline_blocks()],
            (ViewMode::SplitDiff, _, DiffState::Changed(view)) => view.split_blocks(),
            _ => Vec::new(),
        };
        self.selection
            .update(cx, |selection, cx| selection.set_blocks(groups, cx));
    }

    fn shows_markdown(&self) -> bool {
        self.mode == ViewMode::File && matches!(self.content, Content::Markdown { .. })
    }

    fn image_zoom_factors(&self, image: &RasterPreview) -> (f32, f32) {
        let viewport = self.image_scroll.handle.bounds().size;
        let fit = if viewport.width > px(0.) && viewport.height > px(0.) {
            image.fit_scale(viewport)
        } else {
            1.
        };
        let factor = self.image_zoom.unwrap_or(fit);
        (factor, zoom::MIN_FACTOR.min(fit).min(factor))
    }

    fn zoom_image(&mut self, direction: Zoom, cx: &mut Context<Self>) {
        let Content::Image(image) = &self.content else {
            return;
        };
        let (factor, minimum) = self.image_zoom_factors(image);
        let next = zoom::step(factor, direction, minimum);
        if next != factor {
            self.image_zoom = Some(next);
            cx.notify();
        }
    }

    fn zoom_diagram(&mut self, key: String, direction: Zoom, cx: &mut Context<Self>) {
        if let Some(view) = self.diagram_views.get_mut(&key) {
            view.factor = zoom::step(view.factor, direction, zoom::MIN_FACTOR);
            cx.notify();
        }
    }

    fn export_pdf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exporting_pdf || !is_markdown(&self.path) {
            return;
        }
        let source = self.path.clone();
        let Some(directory) = source.parent() else {
            return;
        };
        let default_path = source.with_extension("pdf");
        let suggested_name = default_path.file_name().map(|name| name.to_string_lossy());
        let picker = cx.prompt_for_new_path(directory, suggested_name.as_deref());
        let executor = cx.background_executor().clone();
        self.exporting_pdf = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match picker.await {
                Ok(Ok(Some(destination))) if extension(&destination).as_deref() == Some("pdf") => {
                    executor
                        .spawn(async move { pdf::export(&source, &destination) })
                        .await
                }
                Ok(Ok(Some(_))) => Err("Choose a filename ending in .pdf.".into()),
                Ok(Ok(None)) => Ok(()),
                Ok(Err(error)) => Err(format!("Could not open the save dialog: {error}")),
                Err(_) => Err("The save dialog closed without a result.".into()),
            };
            this.update_in(cx, |this, window, cx| {
                this.exporting_pdf = false;
                if let Err(error) = result {
                    drop(window.prompt(
                        gpui::PromptLevel::Critical,
                        "Could not export PDF",
                        Some(&error),
                        &["OK"],
                        cx,
                    ));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selection.read(cx).selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// "+12 −4" while a diff is shown.
    fn diff_facts(&self) -> Option<String> {
        match (self.mode.is_diff(), &self.diff) {
            (true, DiffState::Changed(view)) => {
                Some(format!("+{} −{}", view.additions, view.deletions))
            }
            _ => None,
        }
    }

    /// "45 lines · 1.7 KB".
    fn file_facts(&self) -> String {
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
        let (track, max, offset) = if self.list_rows().is_some() {
            (
                self.list.viewport_bounds().size.height,
                self.list.max_offset_for_scrollbar().height,
                -self.list.scroll_px_offset_for_scrollbar().y,
            )
        } else if self.shows_markdown() {
            (
                self.markdown_scroll.bounds().size.height,
                self.markdown_scroll.max_offset().height,
                -self.markdown_scroll.offset().y,
            )
        } else {
            return None;
        };
        if track <= px(0.) || max <= px(0.) {
            return None;
        }
        Some((track, track + max, offset.clamp(px(0.), max)))
    }

    fn apply_scroll(&self, desired: Pixels) {
        let desired = desired.max(px(0.));
        if self.list_rows().is_some() {
            self.list.set_offset_from_scrollbar(point(px(0.), -desired));
        } else if self.shows_markdown() {
            let max = self.markdown_scroll.max_offset().height;
            self.markdown_scroll
                .set_offset(point(px(0.), -desired.min(max)));
        }
    }

    fn start_drag(&mut self, mouse_y: Pixels, cx: &mut Context<Self>) {
        let Some((track, content, offset)) = self.scroll_metrics() else {
            return;
        };
        if self.list_rows().is_some() {
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
        // Released outside the window: the move arrives without the button.
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
        let top = if self.list_rows().is_some() {
            self.list.viewport_bounds().origin.y
        } else {
            self.markdown_scroll.bounds().origin.y
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
                    let offset = if this.list_rows().is_some() {
                        -this.list.scroll_px_offset_for_scrollbar().y
                    } else if this.shows_markdown() {
                        -this.markdown_scroll.offset().y
                    } else {
                        return;
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
        if self.mode.is_diff() {
            return self.render_diff(cx);
        }
        match &self.content {
            Content::Loading => render_message("Loading…".to_string()),
            Content::Binary => div().flex_1().into_any_element(),
            Content::Image(image) => {
                let (factor, minimum) = self.image_zoom_factors(image);
                let entity = cx.entity();
                let sink: zoom::ZoomSink = Rc::new(move |direction, _, cx| {
                    entity.update(cx, |this, cx| this.zoom_image(direction, cx));
                });
                let preview = image.render(self.image_zoom);
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .p(px(12.))
                    .relative()
                    .child(
                        div()
                            .relative()
                            .size_full()
                            .child(
                                div()
                                    .id("file-viewer-image")
                                    .size_full()
                                    .overflow_scroll()
                                    .track_scroll(&self.image_scroll.handle)
                                    .child(
                                        div()
                                            .min_w_full()
                                            .min_h_full()
                                            .map(|this| match self.image_zoom {
                                                Some(scale) => this
                                                    .w(image.size.width * scale)
                                                    .h(image.size.height * scale),
                                                None => this.size_full(),
                                            })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .on_scroll_wheel(zoom::wheel(sink.clone()))
                                            .on_scroll_wheel(self.image_scroll.shift_wheel())
                                            .child(preview),
                                    ),
                            )
                            .child(self.image_scroll.horizontal_bar(cx)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(16.))
                            .left(px(16.))
                            .child(zoom::controls("file-image", factor, minimum, sink, cx)),
                    )
                    .into_any_element()
            }
            Content::Failed(error) => render_message(format!("Could not read the file: {error}")),
            Content::Text { lines, .. } if lines.is_empty() => {
                render_message("Empty file.".to_string())
            }
            Content::Markdown { nodes, diagrams } => {
                let settings = Settings::get(cx);
                let entity = cx.entity();
                let zoom = DiagramZoom::new(
                    &self.diagram_views,
                    Rc::new(move |key, direction, _, cx| {
                        entity.update(cx, |this, cx| this.zoom_diagram(key, direction, cx));
                    }),
                );
                let mut scroller = div()
                    .id("file-viewer-markdown")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.markdown_scroll)
                    .px(px(20.))
                    .py(px(14.))
                    .text_size(px(settings.agent_font_size))
                    .line_height(px(settings.agent_line_height()))
                    .child(markdown::render_with_diagrams(
                        "file",
                        nodes,
                        &self.selection,
                        diagrams,
                        Some(&zoom),
                        window,
                        cx,
                    ));
                scroller.style().restrict_scroll_to_axis = Some(true);
                scroller.into_any_element()
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

    fn render_diff(&self, cx: &mut Context<Self>) -> AnyElement {
        let DiffState::Changed(view) = &self.diff else {
            return render_message("Loading…".to_string());
        };
        if view.binary {
            return div().flex_1().into_any_element();
        }
        if view.is_empty() {
            return render_message("No text changes.".to_string());
        }
        let settings = Settings::get(cx);
        let style = RowStyle {
            font_size: px(settings.buffer_font_size),
            line_height: px(settings.buffer_line_height()),
            // Wide enough for the longest number in the code font.
            gutter: px(settings.buffer_font_size * 0.62 * view.number_digits() as f32 + 16.),
        };
        let mode = self.mode;
        list(
            self.list.clone(),
            cx.processor(move |this, index: usize, _, cx| {
                let DiffState::Changed(view) = &this.diff else {
                    return div().into_any_element();
                };
                match mode {
                    ViewMode::SplitDiff => view.render_split_row(index, &this.selection, style, cx),
                    _ => view.render_inline_row(index, &this.selection, style, cx),
                }
            }),
        )
        .font_family(settings.buffer_font_family.clone())
        .flex_1()
        .py(px(8.))
        .into_any_element()
    }

    fn render_mode_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let diff_ready = matches!(self.diff, DiffState::Changed(_));
        let unavailable = match &self.diff {
            DiffState::Loading => "reading git diff…".to_string(),
            DiffState::Failed(error) => format!("git diff failed: {error}"),
            _ => "no changes against HEAD".to_string(),
        };
        let button = |id: &'static str, icon: IconName, mode: ViewMode, tooltip: &str| {
            let available = mode == ViewMode::File || diff_ready;
            IconButton::new(id, icon)
                .size(ButtonSize::Compact)
                .icon_size(IconSize::XSmall)
                .toggle_state(self.mode == mode)
                .disabled(!available)
                .tooltip(Tooltip::text(if available {
                    tooltip.to_string()
                } else {
                    format!("{tooltip}: {unavailable}")
                }))
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
        };
        h_flex()
            .flex_none()
            .gap(px(2.))
            .child(button("view-file", IconName::File, ViewMode::File, "File"))
            .child(button(
                "view-inline-diff",
                IconName::Diff,
                ViewMode::InlineDiff,
                "Inline diff",
            ))
            .child(button(
                "view-split-diff",
                IconName::DiffSplit,
                ViewMode::SplitDiff,
                "Side-by-side diff",
            ))
    }

    /// The file name strip doubles as the window's title bar, laid out like
    /// the hub's: native caption drag on Windows, our own drag elsewhere.
    fn render_title_bar(
        &self,
        window: &mut Window,
        corners: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let file_facts = self.file_facts();
        let diff_facts = self.diff_facts();
        let path = self.path.clone();
        h_flex()
            .id("file-viewer-title-bar")
            .flex_none()
            .w_full()
            .h(px(TITLE_BAR_HEIGHT))
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .bg(if window.is_window_active() {
                colors.title_bar_background
            } else {
                colors.title_bar_inactive_background
            })
            .border_b_1()
            .border_color(colors.border_variant)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, _| {
                    this.title_bar_drag_pending =
                        event.click_count == 1 && !window.default_prevented();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.title_bar_drag_pending = false),
            )
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, _| {
                if std::mem::take(&mut this.title_bar_drag_pending) {
                    platform::start_window_drag(window);
                }
            }))
            // Double-click follows the user's macOS setting (zoom or minimize);
            // on Linux it maximizes. On Windows the native caption consumes the
            // double click, so it is handled on the second press below.
            .on_click(|event, window, _| {
                if event.click_count() >= 2 && !cfg!(windows) {
                    if cfg!(target_os = "macos") {
                        window.titlebar_double_click();
                    } else {
                        platform::toggle_maximized(window);
                    }
                }
            })
            .when(!cfg!(any(target_os = "macos", windows)), |this| {
                this.on_mouse_down(MouseButton::Right, |event, window, _| {
                    window.show_window_menu(event.position)
                })
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    // The name starts right after the traffic lights, which
                    // are hidden in full screen (and absent on Linux).
                    .pl(px(
                        if window.is_fullscreen() || !cfg!(target_os = "macos") {
                            12.
                        } else {
                            TRAFFIC_LIGHT_PADDING
                        },
                    ))
                    .pr(px(10.))
                    .gap(px(8.))
                    .child({
                        let drag_area = h_flex()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(8.))
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
                            );
                        if cfg!(windows) {
                            // Scoped to the name area: the buttons are its
                            // siblings, and a double click on them must not
                            // maximize the window.
                            drag_area.on_mouse_down(MouseButton::Left, |event, window, cx| {
                                if event.click_count == 2 {
                                    platform::toggle_maximized(window);
                                    cx.stop_propagation();
                                }
                            })
                        } else {
                            drag_area
                        }
                    })
                    .child(self.render_mode_buttons(cx))
                    .child(render_facts(file_facts, diff_facts))
                    .child(
                        IconButton::new("file-viewer-open", IconName::Pencil)
                            .size(ButtonSize::Compact)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Open in editor"))
                            .on_click(move |_, _, _| crate::hub::open_in_editor(&path)),
                    )
                    .when(is_markdown(&self.path), |this| {
                        this.child(
                            Button::new("file-viewer-export-pdf", "Export to PDF")
                                .size(ButtonSize::Compact)
                                .loading(self.exporting_pdf)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.export_pdf(window, cx);
                                })),
                        )
                    })
                    .children(render_window_controls(window, |window, _| {
                        window.remove_window()
                    })),
            )
            .into_any_element()
    }
}

impl Render for FileViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !std::mem::replace(&mut self.focused, true) {
            window.focus(&self.focus);
        }
        let (ui_scale, ui_font_family) = {
            let settings = Settings::get(cx);
            (settings.ui_scale(), settings.ui_font_family.clone())
        };
        let colors = cx.theme().colors.clone();
        let corners = crate::window_frame::content_corners(window);
        let note = self.note();
        let body = self.render_body(window, cx);
        let scrollbar = self.render_scrollbar(cx);
        let content = v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Close, window, _| window.remove_window()))
            .on_action(cx.listener(Self::copy_selection))
            // On the root, so scrollbar drags keep going when the mouse
            // leaves the body.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_scroll(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_drag(cx)),
            )
            .size_full()
            .font_family(ui_font_family)
            .bg(colors.editor_background)
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .text_color(colors.text)
            .text_size(px(14.) * ui_scale)
            .line_height((px(21.) * ui_scale).round())
            .child(self.render_title_bar(window, corners, cx))
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
                    .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted))
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
            );
        crate::window_frame::decorate(content, window, cx)
    }
}

/// The file facts keep their width in the diff modes too, so switching
/// modes never moves the buttons beside them. Both labels are laid out in
/// one column, which is as wide as the wider; the hidden one has no height.
fn render_facts(file_facts: String, diff_facts: Option<String>) -> impl IntoElement {
    let showing_diff = diff_facts.is_some();
    let label = |text: String, visible: bool| {
        div()
            .flex()
            .justify_end()
            .when(!visible, |this| {
                this.h(px(0.)).overflow_hidden().opacity(0.)
            })
            .child(
                Label::new(text)
                    .size(LabelSize::XSmall)
                    .color(Color::Placeholder),
            )
    };
    v_flex()
        .flex_none()
        .child(label(file_facts, !showing_diff))
        .children(diff_facts.map(|facts| label(facts, true)))
}

fn render_message(text: String) -> AnyElement {
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
}

fn is_markdown(path: &Path) -> bool {
    is_markdown_extension(extension(path).as_deref())
}

fn is_markdown_extension(extension: Option<&str>) -> bool {
    matches!(extension, Some("md" | "markdown" | "mdx"))
}

fn is_mermaid_extension(extension: Option<&str>) -> bool {
    matches!(extension, Some("mmd" | "mermaid"))
}

fn extension(path: &Path) -> Option<String> {
    path.file_name()?
        .to_string_lossy()
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
}

fn load(
    path: &Path,
    colors: &ThemeColors,
    dark: bool,
    font_family: &str,
) -> Result<Loaded, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let size = file.metadata().map_or(0, |metadata| metadata.len());
    let extension = extension(path);
    if let Some(format) = extension.as_deref().and_then(image_preview::format) {
        return Ok(Loaded {
            content: Content::Image(image_preview::load(file, format)?),
            cut: false,
            size,
            markdown_as_text: false,
        });
    }
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

    let is_markdown = is_markdown_extension(extension.as_deref());
    let is_mermaid = is_mermaid_extension(extension.as_deref());
    if (is_markdown || is_mermaid) && text.len() <= MAX_MARKDOWN_BYTES {
        // A standalone diagram file is one mermaid block.
        let nodes = if is_mermaid {
            vec![Node::Code {
                language: Some("mermaid".to_string()),
                text: text.trim_end_matches('\n').to_string(),
            }]
        } else {
            markdown::parse(&text)
        };
        let mut diagrams = markdown::Diagrams::new();
        for (key, source) in markdown::mermaid_sources("file", &nodes) {
            diagrams.insert(key, mermaid::render(&source, colors, dark, font_family));
        }
        return Ok(Loaded {
            content: Content::Markdown { nodes, diagrams },
            cut,
            size,
            markdown_as_text: false,
        });
    }
    let language = extension.unwrap_or(name);
    let lines = split_lines(&text);
    let highlights = line_highlights(&text, &lines, &language, &colors.syntax);
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
