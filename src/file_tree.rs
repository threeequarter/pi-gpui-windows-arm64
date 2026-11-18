//! The Files panel: the selected project as a tree, colored by git
//! status like Zed's project panel, each file by the innermost repository
//! holding it. Folders are read lazily when expanded; a chain of folders
//! that each hold only the next one shows as a single row, as Zed folds them.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, DragMoveEvent, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable as _, KeyBinding, MouseButton, MouseDownEvent, Pixels, Point,
    ScrollStrategy, SharedString, Task, UniformListScrollHandle, Window, actions, anchored, canvas,
    deferred, div, px, uniform_list,
};

use crate::diff::FileStatus;
use crate::git;
use crate::platform::{self, ConsoleWindow as _, FileClipboard};
use crate::settings::ui_px;
use crate::text_input::TextInput;
use crate::ui::prelude::*;

actions!(
    files,
    [
        SelectPrevious,
        SelectNext,
        ExtendSelectionPrevious,
        ExtendSelectionNext,
        CollapseOrParent,
        ExpandOrChild,
        OpenSelected,
        ConfirmEdit,
        CancelEdit
    ]
);

const KEY_CONTEXT: &str = "FileTree";
const EDIT_KEY_CONTEXT: &str = "FileTreeEdit";
const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 14.;
/// Not worth showing: git's own data and Finder's folder metadata.
const SKIPPED_NAMES: [&str; 2] = [".git", ".DS_Store"];
/// Longest chain of folders one row stands for; a link back up a chain
/// would otherwise never end.
const MAX_FOLDED: usize = 64;

pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some(KEY_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-up", ExtendSelectionPrevious, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-down", ExtendSelectionNext, Some(KEY_CONTEXT)),
        KeyBinding::new("left", CollapseOrParent, Some(KEY_CONTEXT)),
        KeyBinding::new("right", ExpandOrChild, Some(KEY_CONTEXT)),
        KeyBinding::new("enter", OpenSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("enter", ConfirmEdit, Some(EDIT_KEY_CONTEXT)),
        KeyBinding::new("escape", CancelEdit, Some(EDIT_KEY_CONTEXT)),
    ]);
}

pub enum FilesEvent {
    /// A file clicked: the path, and the same relative to the project.
    OpenPath(PathBuf, String),
}

#[derive(Clone)]
struct Entry {
    name: SharedString,
    path: PathBuf,
    is_dir: bool,
    /// The folders a folded row stands for above `path`, outermost first;
    /// each holds nothing but the next.
    folded: Vec<PathBuf>,
}

impl Entry {
    /// The folder listing this entry.
    fn parent(&self) -> Option<&Path> {
        self.folded.first().unwrap_or(&self.path).parent()
    }
}

#[derive(Clone)]
struct VisibleRow {
    entry: Entry,
    depth: usize,
    editing: bool,
}

#[derive(Default)]
struct GitState {
    /// The project's repository first, then the nested ones.
    repositories: Vec<PathBuf>,
    status: HashMap<PathBuf, FileStatus>,
    /// Untracked folders git reports as a whole (`?? dir/`).
    untracked_dirs: Vec<PathBuf>,
    ignored: Vec<PathBuf>,
    /// Folders holding a changed file, colored like the change.
    changed_dirs: HashMap<PathBuf, FileStatus>,
    /// Lines added and removed per changed text file.
    line_changes: HashMap<PathBuf, (usize, usize)>,
}

impl GitState {
    fn status(&self, path: &Path) -> Option<FileStatus> {
        if let Some(status) = self.status.get(path) {
            return Some(*status);
        }
        if let Some(status) = self.changed_dirs.get(path) {
            return Some(*status);
        }
        self.untracked_dirs
            .iter()
            .any(|dir| self.covers(dir, path))
            .then_some(FileStatus::Untracked)
    }

    fn is_ignored(&self, path: &Path) -> bool {
        self.ignored
            .iter()
            .any(|ignored| self.covers(ignored, path))
    }

    /// Whether a folder one repository reported answers for `path`: a
    /// repository nested in between answers for its own files.
    fn covers(&self, folder: &Path, path: &Path) -> bool {
        path.starts_with(folder)
            && !self
                .repositories
                .iter()
                .any(|root| root.starts_with(folder) && path.starts_with(root))
    }
}

struct FileMenu {
    path: PathBuf,
    /// The selection when the menu opened: what Cut, Copy and Delete act on.
    paths: Vec<PathBuf>,
    position: Point<Pixels>,
    /// Read once when the menu opens, so Paste knows what it would paste.
    clipboard: Option<FileClipboard>,
}

enum EditKind {
    NewFile,
    NewFolder,
    Rename(PathBuf),
}

/// An input row in the tree: a new file or folder in `directory`, or the
/// rename of one entry.
struct Editing {
    directory: PathBuf,
    kind: EditKind,
    input: Entity<TextInput>,
}

/// A project shown before, put back at once on coming back while the
/// refresh reads the disk and git again.
struct Remembered {
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<Entry>>,
    git: GitState,
}

pub struct FilesPanel {
    workspace: Option<PathBuf>,
    children: HashMap<PathBuf, Vec<Entry>>,
    expanded: HashSet<PathBuf>,
    remembered: HashMap<PathBuf, Remembered>,
    /// The cursor row: what the keyboard moves and Enter opens. Ctrl-click
    /// can toggle it out of the selection while the cursor stays on it.
    selected: Option<PathBuf>,
    /// Every selected row.
    selection: HashSet<PathBuf>,
    /// The fixed end of a shift range.
    anchor: Option<PathBuf>,
    git: GitState,
    error: Option<String>,
    rows: Vec<VisibleRow>,
    refreshing: bool,
    /// A refresh was asked for while one was in flight; it runs when that
    /// one lands, so a change on disk is never skipped.
    refresh_pending: bool,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    menu: Option<FileMenu>,
    editing: Option<Editing>,
    /// An inline edit was open when the mouse pressed on the tree, so the
    /// click that commits it must not also clear the selection.
    pressed_while_editing: bool,
    edit_anchor: Option<Bounds<Pixels>>,
    drop_target: Option<PathBuf>,
    action_error: Option<String>,
    _refresh_task: Task<()>,
}

impl EventEmitter<FilesEvent> for FilesPanel {}

impl FilesPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            workspace: None,
            children: HashMap::new(),
            expanded: HashSet::new(),
            remembered: HashMap::new(),
            selected: None,
            selection: HashSet::new(),
            anchor: None,
            git: GitState::default(),
            error: None,
            rows: Vec::new(),
            refreshing: false,
            refresh_pending: false,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            menu: None,
            editing: None,
            pressed_while_editing: false,
            edit_anchor: None,
            drop_target: None,
            action_error: None,
            _refresh_task: Task::ready(()),
        }
    }

    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    /// Follows the selected project.
    pub fn set_workspace(&mut self, workspace: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.workspace == workspace {
            return;
        }
        if let Some(previous) = self.workspace.take() {
            self.remembered.insert(
                previous,
                Remembered {
                    expanded: std::mem::take(&mut self.expanded),
                    children: std::mem::take(&mut self.children),
                    git: std::mem::take(&mut self.git),
                },
            );
        }
        let remembered = workspace
            .as_ref()
            .and_then(|path| self.remembered.remove(path));
        (self.expanded, self.children, self.git) = match remembered {
            Some(remembered) => (remembered.expanded, remembered.children, remembered.git),
            None => Default::default(),
        };
        self.workspace = workspace;
        self.selected = None;
        self.selection.clear();
        self.anchor = None;
        self.error = None;
        self.menu = None;
        self.editing = None;
        self.edit_anchor = None;
        self.drop_target = None;
        self.action_error = None;
        self.rebuild_rows();
        self.refreshing = false;
        self.refresh_pending = false;
        self._refresh_task = Task::ready(());
        cx.notify();
    }

    /// Re-reads the root, every expanded folder and git status. The tree
    /// shows as soon as the folders are read; git colors it when it answers.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        if self.refreshing {
            self.refresh_pending = true;
            return;
        }
        self.refreshing = true;
        let mut directories = vec![root.clone()];
        directories.extend(self.expanded.iter().cloned());
        let load = cx.background_spawn(async move {
            directories
                .into_iter()
                .map(|directory| {
                    let listing = read_directory(&directory);
                    (directory, listing)
                })
                .collect::<Vec<(PathBuf, Result<Vec<Entry>, String>)>>()
        });
        let git = cx.background_spawn(async move { git_state(&root) });
        self._refresh_task = cx.spawn(async move |this, cx| {
            let listings = load.await;
            let listed = this.update(cx, |this, cx| {
                this.children.clear();
                this.error = None;
                for (directory, listing) in listings {
                    match listing {
                        Ok(entries) => {
                            this.children.insert(directory, entries);
                        }
                        Err(error) if Some(&directory) == this.workspace.as_ref() => {
                            this.error = Some(error);
                        }
                        // Gone since it was expanded.
                        Err(_) => {
                            this.expanded.remove(&directory);
                        }
                    }
                }
                this.rebuild_rows();
                cx.notify();
            });
            if listed.is_err() {
                return;
            }
            let git = git.await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                this.git = git;
                if std::mem::take(&mut this.refresh_pending) {
                    this.refresh(cx);
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(root) = &self.workspace {
            self.push_rows(root, 0, &mut rows);
        }
        self.rows = rows;
    }

    fn push_rows(&self, directory: &Path, depth: usize, rows: &mut Vec<VisibleRow>) {
        if let Some(editing) = &self.editing
            && !matches!(editing.kind, EditKind::Rename(_))
            && editing.directory == directory
        {
            rows.push(VisibleRow {
                entry: Entry {
                    name: "".into(),
                    path: directory.to_path_buf(),
                    is_dir: matches!(editing.kind, EditKind::NewFolder),
                    folded: Vec::new(),
                },
                depth,
                editing: true,
            });
        }
        let Some(entries) = self.children.get(directory) else {
            return;
        };
        for entry in entries {
            rows.push(VisibleRow {
                entry: entry.clone(),
                depth,
                editing: false,
            });
            if entry.is_dir && self.expanded.contains(&entry.path) {
                self.push_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    /// A folded row opens and closes its whole chain, so the folders stay
    /// open when a new file breaks the chain up.
    fn set_expanded(&mut self, entry: &Entry, expanded: bool, cx: &mut Context<Self>) {
        let directory = entry.path.clone();
        if !expanded {
            self.expanded.remove(&directory);
            for folder in &entry.folded {
                self.expanded.remove(folder);
            }
            self.rebuild_rows();
            cx.notify();
            return;
        }
        self.expanded.extend(entry.folded.iter().cloned());
        if !self.expanded.insert(directory.clone()) {
            return;
        }
        if self.children.contains_key(&directory) {
            self.rebuild_rows();
            cx.notify();
            return;
        }
        let load = {
            let directory = directory.clone();
            cx.background_spawn(async move { read_directory(&directory) })
        };
        cx.spawn(async move |this, cx| {
            let listing = load.await;
            this.update(cx, |this, cx| {
                match listing {
                    Ok(entries) => {
                        this.children.insert(directory, entries);
                    }
                    Err(error) => {
                        eprintln!("pi-gpui: could not read {}: {error}", directory.display());
                        this.expanded.remove(&directory);
                    }
                }
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn row_index(&self, path: &Path) -> Option<usize> {
        self.rows.iter().position(|row| row.entry.path == path)
    }

    fn selected_index(&self) -> Option<usize> {
        self.row_index(self.selected.as_deref()?)
    }

    fn select_only(&mut self, path: PathBuf) {
        self.selected = Some(path.clone());
        self.anchor = Some(path.clone());
        self.selection.clear();
        self.selection.insert(path);
    }

    fn clear_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.selected = None;
        self.selection.clear();
        self.anchor = None;
        cx.notify();
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(index) {
            self.select_only(row.entry.path.clone());
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Selects every row between the anchor and `index`, keeping the
    /// anchor so further shift steps grow or shrink the same range.
    fn extend_selection_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(path) = self.rows.get(index).map(|row| row.entry.path.clone()) else {
            return;
        };
        let Some(anchor) = self
            .anchor
            .as_deref()
            .and_then(|anchor| self.row_index(anchor))
        else {
            self.select_index(index, cx);
            return;
        };
        let range = anchor.min(index)..=anchor.max(index);
        self.selection = self.rows[range]
            .iter()
            .map(|row| row.entry.path.clone())
            .collect();
        self.selected = Some(path);
        self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        cx.notify();
    }

    /// The selected rows' paths in tree order, without ones a selected
    /// folder already holds; paths gone from the tree are left out.
    fn selected_paths(&self) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self
            .rows
            .iter()
            .filter(|row| !row.editing && self.selection.contains(&row.entry.path))
            .map(|row| row.entry.path.clone())
            .collect();
        let mut roots = paths.clone();
        roots.retain(|path| {
            !paths
                .iter()
                .any(|other| path != other && path.starts_with(other))
        });
        roots
    }

    fn previous_index(&self) -> usize {
        match self.selected_index() {
            Some(index) => index.saturating_sub(1),
            None => self.rows.len().saturating_sub(1),
        }
    }

    fn next_index(&self) -> usize {
        match self.selected_index() {
            Some(index) => (index + 1).min(self.rows.len().saturating_sub(1)),
            None => 0,
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        self.select_index(self.previous_index(), cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        self.select_index(self.next_index(), cx);
    }

    fn extend_selection_previous(
        &mut self,
        _: &ExtendSelectionPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_some() {
            return;
        }
        self.extend_selection_to(self.previous_index(), cx);
    }

    fn extend_selection_next(
        &mut self,
        _: &ExtendSelectionNext,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_some() {
            return;
        }
        self.extend_selection_to(self.next_index(), cx);
    }

    fn collapse_or_parent(&mut self, _: &CollapseOrParent, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let Some(index) = self.selected_index() else {
            return;
        };
        let row = self.rows[index].clone();
        if row.entry.is_dir && self.expanded.contains(&row.entry.path) {
            self.set_expanded(&row.entry, false, cx);
            return;
        }
        if let Some(parent) = row.entry.parent()
            && let Some(parent_index) = self.rows.iter().position(|row| row.entry.path == parent)
        {
            self.select_index(parent_index, cx);
        }
    }

    fn expand_or_child(&mut self, _: &ExpandOrChild, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let Some(index) = self.selected_index() else {
            return;
        };
        let entry = self.rows[index].entry.clone();
        if !entry.is_dir {
            return;
        }
        if self.expanded.contains(&entry.path) {
            if self
                .rows
                .get(index + 1)
                .is_some_and(|row| row.entry.path.parent() == Some(entry.path.as_path()))
            {
                self.select_index(index + 1, cx);
            }
        } else {
            self.set_expanded(&entry, true, cx);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let Some(index) = self.selected_index() else {
            return;
        };
        let entry = self.rows[index].entry.clone();
        if entry.is_dir {
            let expanded = self.expanded.contains(&entry.path);
            self.set_expanded(&entry, !expanded, cx);
        } else {
            let relative = self.relative_path(&entry.path);
            cx.emit(FilesEvent::OpenPath(entry.path, relative));
        }
    }

    fn click_row(
        &mut self,
        entry: &Entry,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let modifiers = event.modifiers();
        if modifiers.secondary() {
            self.selected = Some(entry.path.clone());
            self.anchor = Some(entry.path.clone());
            if !self.selection.remove(&entry.path) {
                self.selection.insert(entry.path.clone());
            }
        } else if modifiers.shift {
            if let Some(index) = self.row_index(&entry.path) {
                self.extend_selection_to(index, cx);
            }
        } else {
            self.select_only(entry.path.clone());
            if entry.is_dir {
                let expanded = self.expanded.contains(&entry.path);
                self.set_expanded(entry, !expanded, cx);
            }
        }
        if !entry.is_dir && event.click_count() >= 2 {
            let relative = self.relative_path(&entry.path);
            cx.emit(FilesEvent::OpenPath(entry.path.clone(), relative));
        }
        cx.notify();
    }

    fn start_edit(
        &mut self,
        kind: EditKind,
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match &kind {
            EditKind::NewFile => "File name",
            EditKind::NewFolder => "Folder name",
            EditKind::Rename(_) => "Name",
        };
        let initial = match &kind {
            EditKind::Rename(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let input = cx.new(|cx| {
            let mut input = TextInput::single_line(placeholder, cx).compact();
            input.set_text(initial, cx);
            input
        });
        window.focus(&input.read(cx).focus_handle(cx));
        if matches!(kind, EditKind::Rename(_)) {
            input.update(cx, |input, cx| input.select_all_text(cx));
        } else {
            self.expanded.insert(directory.clone());
            self.refresh(cx);
        }
        self.editing = Some(Editing {
            directory,
            kind,
            input,
        });
        cx.notify();
    }

    fn confirm_edit(&mut self, _: &ConfirmEdit, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.commit_edit(window, cx);
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if self.editing.take().is_some() {
            self.action_error = None;
            self.edit_anchor = None;
            self.rebuild_rows();
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let name = editing.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.edit_anchor = None;
            self.rebuild_rows();
            window.focus(&self.focus_handle);
            cx.notify();
            return;
        }
        let destination = editing.directory.join(&name);
        if let EditKind::Rename(source) = &editing.kind
            && *source == destination
        {
            window.focus(&self.focus_handle);
            cx.notify();
            return;
        }
        if !valid_name(&name) {
            let input = editing.input.clone();
            self.editing = Some(editing);
            window.focus(&input.read(cx).focus_handle(cx));
            self.show_error(format!("\"{name}\" is not a valid name"));
            cx.notify();
            return;
        }
        if destination.exists() {
            let input = editing.input.clone();
            self.editing = Some(editing);
            window.focus(&input.read(cx).focus_handle(cx));
            self.show_error(format!("{} already exists", destination.display()));
            cx.notify();
            return;
        }
        window.focus(&self.focus_handle);
        self.edit_anchor = None;
        self.rebuild_rows();
        self.select_only(destination.clone());
        match editing.kind {
            EditKind::Rename(source) => self.run_file_job(
                move || {
                    fs::rename(&source, &destination)
                        .map_err(|error| format!("could not rename {}: {error}", source.display()))
                },
                cx,
            ),
            EditKind::NewFile => self.run_file_job(
                move || {
                    fs::write(&destination, "").map_err(|error| {
                        format!("could not create {}: {error}", destination.display())
                    })
                },
                cx,
            ),
            EditKind::NewFolder => self.run_file_job(
                move || {
                    fs::create_dir_all(&destination).map_err(|error| {
                        format!("could not create {}: {error}", destination.display())
                    })
                },
                cx,
            ),
        }
    }

    fn show_error(&mut self, message: String) {
        eprintln!("pi-gpui: {message}");
        self.action_error = Some(message);
    }

    fn run_file_job(
        &mut self,
        job: impl FnOnce() -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.action_error = None;
        let work = cx.background_spawn(async move { job() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.show_error(error);
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn move_into(&mut self, sources: Vec<PathBuf>, directory: PathBuf, cx: &mut Context<Self>) {
        let mut moves = Vec::new();
        let mut taken = HashSet::new();
        for source in sources {
            if directory.starts_with(&source) || source.parent() == Some(directory.as_path()) {
                continue;
            }
            let Some(name) = source.file_name() else {
                continue;
            };
            let destination = unique_destination(&directory, name, &taken);
            taken.insert(destination.clone());
            moves.push((source, destination));
        }
        let Some((_, first)) = moves.first() else {
            return;
        };
        self.selected = Some(first.clone());
        self.anchor = Some(first.clone());
        self.selection = moves
            .iter()
            .map(|(_, destination)| destination.clone())
            .collect();
        self.run_file_job(
            move || {
                for (source, destination) in &moves {
                    move_path(source, destination)?;
                }
                Ok(())
            },
            cx,
        );
    }

    fn paste_into(
        &mut self,
        sources: Vec<PathBuf>,
        directory: PathBuf,
        cut: bool,
        cx: &mut Context<Self>,
    ) {
        self.run_file_job(
            move || {
                let mut taken = HashSet::new();
                for source in &sources {
                    if directory.starts_with(source) {
                        return Err(format!("cannot paste {} into itself", source.display()));
                    }
                    if cut && source.parent() == Some(directory.as_path()) {
                        continue;
                    }
                    let Some(name) = source.file_name() else {
                        continue;
                    };
                    let destination = unique_destination(&directory, name, &taken);
                    taken.insert(destination.clone());
                    if cut {
                        move_path(source, &destination)?;
                    } else {
                        copy_path(source, &destination)?;
                    }
                }
                if cut {
                    platform::write_file_clipboard(&[], false);
                }
                Ok(())
            },
            cx,
        );
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn relative_path(&self, path: &Path) -> String {
        self.workspace
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// The label area of the edited row, in window coordinates. The input
    /// overlays it from a deferred root: uniform_list lays rows out outside
    /// the normal flex tree, which collapses a TextInput placed in a row.
    fn edit_anchor_canvas(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        canvas(
            move |bounds, _, cx| {
                entity.update(cx, |this, cx| {
                    if this.edit_anchor != Some(bounds) {
                        this.edit_anchor = Some(bounds);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .into_any_element()
    }

    fn render_edit_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editing = self.editing.as_ref()?;
        let anchor = self.edit_anchor?;
        Some(
            deferred(
                anchored().position(anchor.origin).child(
                    div()
                        .w(anchor.size.width)
                        .occlude()
                        .key_context(EDIT_KEY_CONTEXT)
                        .on_action(cx.listener(Self::confirm_edit))
                        .on_action(cx.listener(Self::cancel_edit))
                        .on_mouse_down_out(
                            cx.listener(|this, _, window, cx| this.commit_edit(window, cx)),
                        )
                        .child(editing.input.clone()),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_edit_row(&self, depth: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(editing) = &self.editing else {
            return div().into_any_element();
        };
        let icon = if matches!(editing.kind, EditKind::NewFolder) {
            IconName::Folder
        } else {
            IconName::File
        };
        h_flex()
            .w_full()
            .h(ui_px(cx, ROW_HEIGHT))
            .pl(px(8. + depth as f32 * INDENT))
            .pr(px(8.))
            .gap(px(4.))
            .child(div().w(px(12.)))
            .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            .child(self.edit_anchor_canvas(cx))
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = cx.theme().colors.clone();
        let selected_paths = self.selected_paths();
        range
            .filter_map(|index| {
                let row = self.rows.get(index)?.clone();
                if row.editing {
                    return Some(self.render_edit_row(row.depth, cx));
                }
                let entry = row.entry;
                let name = entry.name.clone();
                let selected = self.selection.contains(&entry.path);
                let expanded = entry.is_dir && self.expanded.contains(&entry.path);
                let color = if self.git.is_ignored(&entry.path) {
                    Color::Placeholder
                } else {
                    match self.git.status(&entry.path) {
                        Some(status) => status.color(),
                        None => Color::Default,
                    }
                };
                let icon = match (entry.is_dir, expanded) {
                    (true, true) => IconName::FolderOpen,
                    (true, false) => IconName::Folder,
                    (false, _) => IconName::File,
                };
                let disclosure = if entry.is_dir {
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element()
                } else {
                    div().w(px(12.)).into_any_element()
                };
                let (additions, deletions) = if entry.is_dir {
                    (0, 0)
                } else {
                    self.git
                        .line_changes
                        .get(&entry.path)
                        .copied()
                        .unwrap_or_default()
                };
                let menu_path = entry.path.clone();
                let drag_paths = if self.selection.contains(&entry.path) {
                    selected_paths.clone()
                } else {
                    vec![entry.path.clone()]
                };
                let dragged_row = if drag_paths.len() == 1 {
                    self.rows.iter().find(|row| row.entry.path == drag_paths[0])
                } else {
                    None
                };
                let drag_name = match dragged_row {
                    Some(row) => row.entry.name.clone(),
                    None if drag_paths.len() > 1 => format!("{} items", drag_paths.len()).into(),
                    None => entry.name.clone(),
                };
                let drag_is_dir = dragged_row.is_some_and(|row| row.entry.is_dir);
                let drop_directory = if entry.is_dir {
                    Some(entry.path.clone())
                } else {
                    entry.path.parent().map(Path::to_path_buf)
                };
                let rename_input = match &self.editing {
                    Some(editing) => match &editing.kind {
                        EditKind::Rename(path) if *path == entry.path => {
                            Some(editing.input.clone())
                        }
                        _ => None,
                    },
                    None => None,
                };
                let mut element = h_flex()
                    .id(("file-row", index))
                    .w_full()
                    .h(ui_px(cx, ROW_HEIGHT))
                    .pl(px(8. + row.depth as f32 * INDENT))
                    .pr(px(8.))
                    .gap(px(4.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.ghost_element_selected))
                    .when(
                        entry.is_dir && self.drop_target.as_ref() == Some(&entry.path),
                        |this| this.bg(colors.ghost_element_selected),
                    )
                    .hover(|style| style.bg(colors.ghost_element_hover));
                if rename_input.is_none() {
                    element = element
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.click_row(&entry, event, window, cx);
                            cx.stop_propagation();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                window.focus(&this.focus_handle);
                                this.selected = Some(menu_path.clone());
                                if !this.selection.contains(&menu_path) {
                                    this.select_only(menu_path.clone());
                                }
                                this.menu = Some(FileMenu {
                                    path: menu_path.clone(),
                                    paths: this.selected_paths(),
                                    position: event.position,
                                    clipboard: platform::read_file_clipboard(),
                                });
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .on_drag(drag_paths, move |_, offset, _, cx| {
                            cx.new(|_| DraggedFile {
                                name: drag_name.clone(),
                                is_dir: drag_is_dir,
                                offset,
                            })
                        })
                        .on_drag_move::<Vec<PathBuf>>(cx.listener({
                            let directory = drop_directory.clone();
                            move |this, event: &DragMoveEvent<Vec<PathBuf>>, _, cx| {
                                if !event.bounds.contains(&event.event.position) {
                                    return;
                                }
                                let Some(directory) = directory.clone() else {
                                    return;
                                };
                                if event
                                    .drag(cx)
                                    .iter()
                                    .any(|source| directory.starts_with(source))
                                {
                                    return;
                                }
                                this.drop_target = Some(directory);
                                cx.notify();
                            }
                        }))
                        .on_drag_move::<ExternalPaths>(cx.listener({
                            let directory = drop_directory.clone();
                            move |this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                                if !event.bounds.contains(&event.event.position) {
                                    return;
                                }
                                this.drop_target = directory.clone();
                                cx.notify();
                            }
                        }))
                        .on_drop::<Vec<PathBuf>>(cx.listener({
                            let directory = drop_directory.clone();
                            move |this, sources: &Vec<PathBuf>, _, cx| {
                                this.drop_target = None;
                                let Some(directory) = directory.clone() else {
                                    return;
                                };
                                this.move_into(sources.clone(), directory, cx);
                            }
                        }))
                        .on_drop::<ExternalPaths>(cx.listener({
                            let directory = drop_directory.clone();
                            move |this, paths: &ExternalPaths, _, cx| {
                                this.drop_target = None;
                                let Some(directory) = directory.clone() else {
                                    return;
                                };
                                this.paste_into(paths.paths().to_vec(), directory, false, cx);
                            }
                        }));
                }
                Some(
                    element
                        .child(disclosure)
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(match rename_input {
                            Some(_) => self.edit_anchor_canvas(cx),
                            None => div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(
                                    Label::new(name)
                                        .size(LabelSize::Small)
                                        .color(color)
                                        .truncate(),
                                )
                                .into_any_element(),
                        })
                        .when(additions > 0, |this| {
                            this.child(
                                Label::new(format!("+{additions}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Success),
                            )
                        })
                        .when(deletions > 0, |this| {
                            this.child(
                                Label::new(format!("−{deletions}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            )
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_menu(&self, menu: &FileMenu, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let is_dir = fs::metadata(&menu.path).is_ok_and(|metadata| metadata.is_dir());
        let is_root = self.workspace.as_ref() == Some(&menu.path);
        let directory = if is_dir {
            menu.path.clone()
        } else {
            menu.path.parent().unwrap_or(&menu.path).to_path_buf()
        };
        let item = |id: &'static str, icon: IconName, label: &'static str| {
            h_flex()
                .id(id)
                .h(ui_px(cx, 28.))
                .px(px(8.))
                .gap(px(8.))
                .rounded(px(5.))
                .cursor_pointer()
                .hover(|style| style.bg(colors.element_hover))
                .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Default))
                .child(Label::new(label).size(LabelSize::Small))
        };
        let separator = || div().my(px(4.)).h(px(1.)).bg(colors.border_variant);
        let path = menu.path.clone();
        let mut items: Vec<AnyElement> = Vec::new();
        if !is_dir {
            let path = path.clone();
            items.push(
                item(
                    "file-menu-open",
                    IconName::ExternalLink,
                    "Open with default app",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    platform::open_with_default_app(&path);
                    cx.notify();
                }))
                .into_any_element(),
            );
            items.push(separator().into_any_element());
        }
        let new_file_directory = directory.clone();
        items.push(
            item("file-menu-new-file", IconName::FilePlus, "New file")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.menu = None;
                    this.start_edit(EditKind::NewFile, new_file_directory.clone(), window, cx);
                }))
                .into_any_element(),
        );
        let new_folder_directory = directory.clone();
        items.push(
            item("file-menu-new-folder", IconName::FolderPlus, "New folder")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.menu = None;
                    this.start_edit(
                        EditKind::NewFolder,
                        new_folder_directory.clone(),
                        window,
                        cx,
                    );
                }))
                .into_any_element(),
        );
        if !is_root {
            items.push(separator().into_any_element());
            let cut_paths = menu.paths.clone();
            items.push(
                item("file-menu-cut", IconName::Scissors, "Cut")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        platform::write_file_clipboard(&cut_paths, true);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            let copy_paths = menu.paths.clone();
            items.push(
                item("file-menu-copy", IconName::Copy, "Copy")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        platform::write_file_clipboard(&copy_paths, false);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        if menu
            .clipboard
            .as_ref()
            .is_some_and(|clipboard| !clipboard.paths.is_empty())
        {
            let paste_directory = directory.clone();
            items.push(
                item("file-menu-paste", IconName::ClipboardPaste, "Paste")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        if let Some(clipboard) = platform::read_file_clipboard() {
                            this.paste_into(
                                clipboard.paths,
                                paste_directory.clone(),
                                clipboard.cut,
                                cx,
                            );
                        }
                    }))
                    .into_any_element(),
            );
        }
        if !is_root {
            items.push(separator().into_any_element());
            let rename_path = path.clone();
            let rename_directory = menu.path.parent().unwrap_or(&menu.path).to_path_buf();
            items.push(
                item("file-menu-rename", IconName::Pencil, "Rename")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.menu = None;
                        this.start_edit(
                            EditKind::Rename(rename_path.clone()),
                            rename_directory.clone(),
                            window,
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
            let delete_paths = menu.paths.clone();
            items.push(
                item("file-menu-delete", IconName::Trash, "Delete")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        let paths = delete_paths.clone();
                        this.run_file_job(
                            move || {
                                for path in &paths {
                                    platform::move_to_trash(path)?;
                                }
                                Ok(())
                            },
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
            items.push(separator().into_any_element());
        }
        let reveal_path = path.clone();
        items.push(
            item(
                "file-menu-reveal",
                IconName::FolderOpen,
                if cfg!(target_os = "macos") {
                    "Reveal in Finder"
                } else {
                    "Show in File Manager"
                },
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.menu = None;
                platform::reveal_in_file_manager(&reveal_path);
                cx.notify();
            }))
            .into_any_element(),
        );
        if !is_root {
            let relative = self.relative_path(&menu.path);
            items.push(
                item(
                    "file-menu-copy-relative",
                    IconName::Copy,
                    "Copy relative path",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(relative.clone()));
                    cx.notify();
                }))
                .into_any_element(),
            );
        }
        deferred(
            anchored().position(menu.position).child(
                v_flex()
                    .id("file-menu")
                    .w(px(220.))
                    .p(px(4.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(items),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_message(&self, text: String) -> AnyElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .px(px(24.))
            .child(
                div().text_center().child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                ),
            )
            .into_any_element()
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        if self.drop_target.is_some() && !cx.has_active_drag() {
            self.drop_target = None;
            cx.notify();
        }
        let content = if self.workspace.is_none() {
            self.render_message("Select a project to see its files.".into())
        } else if let Some(error) = &self.error {
            self.render_message(format!("Could not read the folder: {error}"))
        } else if self.rows.is_empty() && self.refreshing {
            h_flex()
                .flex_1()
                .justify_center()
                .child(Spinner::new().size(IconSize::Small).color(Color::Muted))
                .into_any_element()
        } else if self.rows.is_empty() {
            self.render_message("This folder is empty.".into())
        } else {
            div()
                .id("file-tree-area")
                .flex_1()
                .on_click(cx.listener(|this, _, window, cx| {
                    if std::mem::take(&mut this.pressed_while_editing) {
                        return;
                    }
                    this.clear_selection(window, cx);
                }))
                .child(
                    uniform_list(
                        "file-tree",
                        self.rows.len(),
                        cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
                    )
                    .track_scroll(self.scroll.clone())
                    .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
                        if event.button == MouseButton::Left {
                            this.pressed_while_editing = this.editing.is_some();
                        }
                    }))
                    .size_full()
                    .px(px(6.))
                    .pb(px(10.)),
                )
                .into_any_element()
        };
        let project = self.workspace.as_ref().map(|workspace| {
            let name = workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| workspace.display().to_string());
            h_flex()
                .flex_none()
                .h(ui_px(cx, 30.))
                .pl(px(12.))
                .pr(px(6.))
                .gap(px(6.))
                .border_b_1()
                .border_color(colors.border_variant)
                .child(
                    div().flex_1().min_w(px(0.)).child(
                        Label::new(name)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
                .child(
                    IconButton::new("open-terminal", IconName::Terminal)
                        .size(ButtonSize::Compact)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Open terminal here"))
                        .on_click({
                            let workspace = workspace.clone();
                            move |_, _, _| crate::platform::open_terminal(&workspace)
                        }),
                )
        });
        let menu = self.menu.as_ref().map(|menu| self.render_menu(menu, cx));
        let action_error = self.action_error.as_ref().map(|error| {
            h_flex()
                .gap(px(6.))
                .px(px(12.))
                .py(px(4.))
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                )
                .child(
                    div().flex_1().min_w(px(0.)).child(
                        Label::new(error.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
                )
        });
        v_flex()
            .id("files-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::extend_selection_previous))
            .on_action(cx.listener(Self::extend_selection_next))
            .on_action(cx.listener(Self::collapse_or_parent))
            .on_action(cx.listener(Self::expand_or_child))
            .on_action(cx.listener(Self::open_selected))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let Some(workspace) = this.workspace.clone() else {
                        return;
                    };
                    window.focus(&this.focus_handle);
                    this.menu = Some(FileMenu {
                        paths: vec![workspace.clone()],
                        path: workspace,
                        position: event.position,
                        clipboard: platform::read_file_clipboard(),
                    });
                    cx.notify();
                }),
            )
            .on_drag_move::<Vec<PathBuf>>(cx.listener(
                |this, event: &DragMoveEvent<Vec<PathBuf>>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        if this.drop_target.take().is_some() {
                            cx.notify();
                        }
                        return;
                    }
                    let Some(workspace) = this.workspace.clone() else {
                        return;
                    };
                    if event
                        .drag(cx)
                        .iter()
                        .any(|source| workspace.starts_with(source))
                    {
                        return;
                    }
                    this.drop_target = Some(workspace);
                    cx.notify();
                },
            ))
            .on_drag_move::<ExternalPaths>(cx.listener(
                |this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        if this.drop_target.take().is_some() {
                            cx.notify();
                        }
                        return;
                    }
                    this.drop_target = this.workspace.clone();
                    cx.notify();
                },
            ))
            .on_drop::<Vec<PathBuf>>(cx.listener(|this, sources: &Vec<PathBuf>, _, cx| {
                this.drop_target = None;
                let Some(workspace) = this.workspace.clone() else {
                    return;
                };
                this.move_into(sources.clone(), workspace, cx);
            }))
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.drop_target = None;
                let Some(workspace) = this.workspace.clone() else {
                    return;
                };
                this.paste_into(paths.paths().to_vec(), workspace, false, cx);
            }))
            .size_full()
            .children(project)
            .children(action_error)
            .child(content)
            .children(self.render_edit_overlay(cx))
            .children(menu)
    }
}

/// The ghost following the cursor while a tree entry is dragged. The
/// padding puts the pill under the cursor: gpui anchors the ghost at the
/// mouse minus the press point inside the row.
struct DraggedFile {
    name: SharedString,
    is_dir: bool,
    offset: Point<Pixels>,
}

impl Render for DraggedFile {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .pl(px(f32::from(self.offset.x)))
            .pt(px(f32::from(self.offset.y)))
            .child(
                h_flex()
                    .gap(px(4.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .shadow_lg()
                    .child(
                        Icon::new(if self.is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new(self.name.clone()).size(LabelSize::Small)),
            )
    }
}

/// A name one path component can hold.
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return false;
    }
    #[cfg(windows)]
    {
        // Windows strips trailing dots and spaces silently and reserves
        // device names even with an extension.
        if name.ends_with('.') || name.ends_with(' ') {
            return false;
        }
        if name.chars().any(|character| {
            character < ' ' || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        }) {
            return false;
        }
        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem.as_str()) {
            return false;
        }
    }
    true
}

/// `name` in `directory`, or `name (2)`, `name (3)`… when taken on disk or
/// already planned in `taken`, keeping the extension on files.
fn unique_destination(directory: &Path, name: &OsStr, taken: &HashSet<PathBuf>) -> PathBuf {
    let destination = directory.join(name);
    if !destination.exists() && !taken.contains(&destination) {
        return destination;
    }
    let base = Path::new(name);
    let stem = base.file_stem().unwrap_or_default().to_string_lossy();
    let extension = base
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()));
    let mut counter = 2;
    loop {
        let candidate = match &extension {
            Some(extension) => format!("{stem} ({counter}){extension}"),
            None => format!("{stem} ({counter})"),
        };
        let destination = directory.join(candidate);
        if !destination.exists() && !taken.contains(&destination) {
            return destination;
        }
        counter += 1;
    }
}

/// Recursive copy; symlinks are recreated as links so a linked folder
/// cannot loop the walk.
fn copy_path(source: &Path, destination: &Path) -> Result<(), String> {
    copy_inner(source, destination, &mut Vec::new())
}

fn copy_inner(source: &Path, destination: &Path, stack: &mut Vec<PathBuf>) -> Result<(), String> {
    let fail = |error: std::io::Error| format!("could not copy {}: {error}", source.display());
    let metadata = fs::symlink_metadata(source).map_err(fail)?;
    if metadata.is_symlink() {
        let target = fs::read_link(source).map_err(fail)?;
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, destination);
        #[cfg(windows)]
        let linked = if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
            std::os::windows::fs::symlink_dir(&target, destination)
        } else {
            std::os::windows::fs::symlink_file(&target, destination)
        };
        if linked.is_ok() {
            return Ok(());
        }
        // Windows usually refuses link creation without a privilege;
        // fall back to copying the target, like Explorer.
    }
    let metadata = fs::metadata(source).map_err(fail)?;
    if metadata.is_dir() {
        let canonical = fs::canonicalize(source).map_err(fail)?;
        if stack.contains(&canonical) {
            return Err(format!(
                "could not copy {}: symlink cycle",
                source.display()
            ));
        }
        stack.push(canonical);
        fs::create_dir_all(destination).map_err(fail)?;
        for entry in fs::read_dir(source).map_err(fail)?.flatten() {
            copy_inner(&entry.path(), &destination.join(entry.file_name()), stack)?;
        }
        stack.pop();
        return Ok(());
    }
    fs::copy(source, destination).map_err(fail)?;
    Ok(())
}

/// Rename, or copy and remove the source when rename fails across volumes.
/// The source only goes once the copy fully succeeded.
fn move_path(source: &Path, destination: &Path) -> Result<(), String> {
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }
    copy_path(source, destination)?;
    let fail = |error: std::io::Error| format!("could not move {}: {error}", source.display());
    if fs::symlink_metadata(source).map_err(fail)?.is_dir() {
        fs::remove_dir_all(source).map_err(fail)?;
    } else {
        fs::remove_file(source).map_err(fail)?;
    }
    Ok(())
}

/// Folders first, then files, by name ignoring case.
fn read_directory(directory: &Path) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if SKIPPED_NAMES.contains(&name.as_str()) {
                return None;
            }
            let path = entry.path();
            // Follows symlinks, so a linked folder can be opened.
            let is_dir = fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
            if !is_dir {
                return Some(Entry {
                    name: name.into(),
                    path,
                    is_dir,
                    folded: Vec::new(),
                });
            }
            Some(folded_entry(name, path))
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

/// A folder, folded down through every folder that holds only the next.
fn folded_entry(name: String, path: PathBuf) -> Entry {
    let mut name = name;
    let mut path = path;
    let mut folded = Vec::new();
    while folded.len() < MAX_FOLDED
        && let Some(only) = only_subfolder(&path)
    {
        name.push('/');
        name.push_str(&only.file_name().unwrap_or_default().to_string_lossy());
        folded.push(std::mem::replace(&mut path, only));
    }
    Entry {
        name: name.into(),
        path,
        is_dir: true,
        folded,
    }
}

/// The folder `folder` holds when it holds nothing else.
fn only_subfolder(folder: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(folder)
        .ok()?
        .flatten()
        .filter(|entry| !SKIPPED_NAMES.contains(&entry.file_name().to_string_lossy().as_ref()));
    let only = entries.next()?.path();
    if entries.next().is_some() {
        return None;
    }
    fs::metadata(&only)
        .is_ok_and(|metadata| metadata.is_dir())
        .then_some(only)
}

/// Empty outside a git work tree or when git is missing.
fn git_state(workspace: &Path) -> GitState {
    let mut state = GitState {
        repositories: git::repositories(workspace).unwrap_or_default(),
        ..GitState::default()
    };
    let repositories = state.repositories.clone();
    let outputs = git::for_each_repository(&repositories, read_repository);
    for (root, (status, numstat)) in repositories.iter().zip(outputs) {
        let Some(status) = status else {
            continue;
        };
        parse_status(
            &String::from_utf8_lossy(&status),
            root,
            workspace,
            &mut state,
        );
        if let Some(numstat) = numstat {
            state
                .line_changes
                .extend(parse_numstat(&String::from_utf8_lossy(&numstat), root));
        }
    }
    for (path, status) in &state.status {
        if *status == FileStatus::Untracked
            && let Some(lines) = count_lines(path)
        {
            state.line_changes.insert(path.clone(), (lines, 0));
        }
    }
    state
}

/// `git status` and the line counts of the repository at `root`, both at once.
fn read_repository(root: &Path) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let run = |arguments: &[&str]| -> Option<Vec<u8>> {
        let output = Command::new("git")
            .without_console_window()
            // A status that rewrote the index would wake the project watcher.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    };
    std::thread::scope(|scope| {
        let numstat = scope.spawn(|| run(&["diff", "HEAD", "--numstat", "-z", "--no-renames"]));
        let status = run(&["status", "--porcelain=v1", "-z", "--ignored"]);
        (status, git::joined(numstat))
    })
}

/// `git diff --numstat -z` lines: `added\tdeleted\tpath\0`. Binary files
/// report `-` and are skipped.
fn parse_numstat(output: &str, root: &Path) -> HashMap<PathBuf, (usize, usize)> {
    output
        .split('\0')
        .filter_map(|record| {
            let mut fields = record.splitn(3, '\t');
            let additions = fields.next()?.trim().parse().ok()?;
            let deletions = fields.next()?.parse().ok()?;
            let path = fields.next().filter(|path| !path.is_empty())?;
            Some((root.join(path), (additions, deletions)))
        })
        .filter(|(_, (additions, deletions))| additions + deletions > 0)
        .collect()
}

/// Line count of a small untracked text file, all of it new.
fn count_lines(path: &Path) -> Option<usize> {
    const MAX_BYTES: u64 = 1024 * 1024;
    if fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count();
    Some(newlines + usize::from(!bytes.ends_with(b"\n")))
}

/// Adds one repository's `git status --porcelain=v1 -z` to `state`. A nested
/// repository's folder is left to that repository; changes color the folders
/// above them up to the project, across repositories.
fn parse_status(output: &str, root: &Path, workspace: &Path, state: &mut GitState) {
    let mut tokens = output.split('\0').filter(|token| !token.is_empty());
    while let Some(token) = tokens.next() {
        if token.len() < 4 {
            continue;
        }
        let (code, path) = token.split_at(3);
        let code = code.trim_end();
        let is_dir = path.ends_with('/');
        let path = root.join(path.trim_end_matches('/'));
        if state.repositories.contains(&path) {
            continue;
        }
        let status = match code {
            "!!" => {
                state.ignored.push(path);
                continue;
            }
            "??" => FileStatus::Untracked,
            _ if code.contains('R') || code.contains('C') => {
                // The original path follows as its own token.
                tokens.next();
                FileStatus::Renamed
            }
            _ if code.contains('A') => FileStatus::Added,
            _ if code.contains('D') => FileStatus::Deleted,
            _ => FileStatus::Modified,
        };
        if is_dir {
            state.untracked_dirs.push(path.clone());
        } else {
            state.status.insert(path.clone(), status);
        }
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory == workspace || !directory.starts_with(workspace) {
                break;
            }
            let entry = state
                .changed_dirs
                .entry(directory.to_path_buf())
                .or_insert(status);
            // A modification outweighs additions in the folder's color.
            if status == FileStatus::Modified {
                *entry = FileStatus::Modified;
            }
            parent = directory.parent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_marks_files_folders_and_ignored_paths() {
        let root = Path::new("/repo");
        let output = " M src/main.rs\0?? notes/\0R  new.rs\0old.rs\0!! target/\0A  src/ui/new.rs\0";
        let mut state = GitState::default();
        parse_status(output, root, root, &mut state);
        assert_eq!(
            state.status(Path::new("/repo/src/main.rs")),
            Some(FileStatus::Modified)
        );
        assert_eq!(
            state.status(Path::new("/repo/notes/todo.md")),
            Some(FileStatus::Untracked)
        );
        assert_eq!(
            state.status(Path::new("/repo/new.rs")),
            Some(FileStatus::Renamed)
        );
        assert_eq!(state.status(Path::new("/repo/old.rs")), None);
        assert_eq!(
            state.status(Path::new("/repo/src")),
            Some(FileStatus::Modified)
        );
        assert_eq!(
            state.status(Path::new("/repo/src/ui")),
            Some(FileStatus::Added)
        );
        assert!(state.is_ignored(Path::new("/repo/target/debug/app")));
        assert!(!state.is_ignored(Path::new("/repo/src/main.rs")));
    }

    #[test]
    fn folds_folders_that_hold_only_one_folder() {
        let root = std::env::temp_dir().join(format!("pi-gpui-fold-{}", std::process::id()));
        fs::create_dir_all(root.join("backend/src/main/java/app")).unwrap();
        fs::write(root.join("backend/src/main/java/app/Main.java"), "").unwrap();
        fs::write(root.join("backend/src/main/build.txt"), "").unwrap();
        let entries = read_directory(&root).unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.as_ref(), "backend/src/main");
        assert_eq!(entries[0].path, root.join("backend/src/main"));
        assert_eq!(
            entries[0].folded,
            vec![root.join("backend"), root.join("backend/src")]
        );
        assert_eq!(entries[0].parent(), Some(root.as_path()));
    }

    #[test]
    fn nested_repository_answers_for_its_own_files() {
        let root = Path::new("/repo");
        let nested = Path::new("/repo/vendor/lib");
        let mut state = GitState {
            repositories: vec![root.to_path_buf(), nested.to_path_buf()],
            ..GitState::default()
        };
        parse_status("?? vendor/\0?? vendor/lib/\0", root, root, &mut state);
        parse_status(" M src/a.rs\0!! target/\0", nested, root, &mut state);
        assert_eq!(
            state.status(Path::new("/repo/vendor/notes.md")),
            Some(FileStatus::Untracked)
        );
        assert_eq!(state.status(Path::new("/repo/vendor/lib/README.md")), None);
        assert_eq!(
            state.status(Path::new("/repo/vendor/lib/src/a.rs")),
            Some(FileStatus::Modified)
        );
        assert_eq!(
            state.status(Path::new("/repo/vendor/lib")),
            Some(FileStatus::Modified)
        );
        assert!(state.is_ignored(Path::new("/repo/vendor/lib/target/x")));
    }

    #[test]
    fn numstat_counts_text_files_and_skips_binary() {
        let root = Path::new("/repo");
        let changes = parse_numstat("3\t1\tsrc/main.rs\0-\t-\tlogo.png\0", root);
        assert_eq!(changes.get(Path::new("/repo/src/main.rs")), Some(&(3, 1)));
        assert_eq!(changes.get(Path::new("/repo/logo.png")), None);
    }
}
