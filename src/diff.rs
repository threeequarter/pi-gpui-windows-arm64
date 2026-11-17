//! The Changes panel: `git diff` of the selected project against
//! HEAD, untracked files included, with the git client around it: staging
//! from the index, discarding a file's changes, commit and amend, branch
//! switching, fetch/pull/push and a commit message written by pi. A project
//! holding nested repositories shows one at a time, picked as in Zed's git
//! panel. A file's diff opens in the file viewer. Git is run directly with
//! separate arguments, never through a shell; every git and pi call runs in
//! the background.

use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Corner, ElementId, Entity, EventEmitter, FocusHandle, FontWeight,
    Hsla, KeyBinding, Subscription, Task, Window, actions, anchored, deferred, div, px,
};

use crate::git::{self, Discard, GitError, RepoStatus, StatusEntry, git};
use crate::pi::{self, Installation};
use crate::sessions;
use crate::settings::ui_px;
use crate::text_input::{self, TextInput};
use crate::ui::prelude::*;
use crate::ui::{AnimationTicker, SyntaxColors};

use branch_picker::{BRANCH_PICKER_CONTEXT, BranchPicker};

mod branch_picker;
mod commit;

actions!(diff, [CommitNow, DismissBranchPicker]);

/// The commit message box. Enter stays a newline there, so committing
/// from the keyboard is ⌘Enter.
const COMMIT_CONTEXT: &str = "GitCommit";
/// A click on the button that opened a popover first closes it as a click
/// outside; that same click must not open it again.
const POPOVER_REOPEN_GUARD: Duration = Duration::from_millis(250);
/// Untracked files larger than this are listed without content.
const MAX_UNTRACKED_BYTES: u64 = 512 * 1024;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            &crate::keys::binding("cmd-enter"),
            CommitNow,
            Some(COMMIT_CONTEXT),
        ),
        KeyBinding::new("escape", DismissBranchPicker, Some(BRANCH_PICKER_CONTEXT)),
    ]);
}

pub enum DiffPanelEvent {
    /// A git action ran: the index, HEAD or the remote refs may have changed.
    RepositoryChanged,
    /// Show this file's diff: its full path and the path shown as its title.
    OpenDiff(PathBuf, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

impl FileStatus {
    fn letter(self) -> &'static str {
        match self {
            FileStatus::Modified => "M",
            FileStatus::Added => "A",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Untracked => "U",
        }
    }

    pub fn color(self) -> Color {
        match self {
            FileStatus::Modified | FileStatus::Renamed => Color::Warning,
            FileStatus::Added | FileStatus::Untracked => Color::Success,
            FileStatus::Deleted => Color::Error,
        }
    }

    fn description(self) -> &'static str {
        match self {
            FileStatus::Modified => "Modified",
            FileStatus::Added => "Added",
            FileStatus::Deleted => "Deleted",
            FileStatus::Renamed => "Renamed",
            FileStatus::Untracked => "Untracked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Hunk,
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub additions: usize,
    pub deletions: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
pub struct Changes {
    pub root: PathBuf,
    pub files: Vec<FileDiff>,
}

enum State {
    /// No session is open, so there is no project to look at.
    NoProject,
    Loading,
    NotARepository,
    Failed(String),
    Loaded(Changes, RepoStatus),
}

/// The git or pi work in flight. One at a time: two git processes would
/// fight over `index.lock`, and a second click must not commit twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitAction {
    ToggleFile,
    StageAll,
    UnstageAll,
    Discard,
    SwitchBranch,
    CreateBranch,
    Commit,
    Generate,
    Fetch,
    Pull,
    Push,
}

impl GitAction {
    /// Shown while the action runs, e.g. as the reason other buttons wait.
    fn progress(self) -> &'static str {
        match self {
            GitAction::ToggleFile | GitAction::StageAll => "Staging…",
            GitAction::UnstageAll => "Unstaging…",
            GitAction::Discard => "Discarding…",
            GitAction::SwitchBranch => "Switching branch…",
            GitAction::CreateBranch => "Creating branch…",
            GitAction::Commit => "Committing…",
            GitAction::Generate => "pi is writing the commit message…",
            GitAction::Fetch => "Fetching…",
            GitAction::Pull => "Pulling…",
            GitAction::Push => "Pushing…",
        }
    }

    fn failure(self) -> &'static str {
        match self {
            GitAction::ToggleFile => "Staging failed",
            GitAction::StageAll => "Stage All failed",
            GitAction::UnstageAll => "Unstage All failed",
            GitAction::Discard => "Discarding changes failed",
            GitAction::SwitchBranch => "Switching branch failed",
            GitAction::CreateBranch => "Creating the branch failed",
            GitAction::Commit => "Commit failed",
            GitAction::Generate => "Generating the commit message failed",
            GitAction::Fetch => "Fetch failed",
            GitAction::Pull => "Pull failed",
            GitAction::Push => "Push failed",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Popover {
    Repositories,
    Branches,
    Remotes(GitAction),
}

/// The checkbox of a file: the index holds all, part or none of its changes.
fn stage_state(entry: &StatusEntry) -> ToggleState {
    match (entry.staged, entry.unstaged) {
        (true, false) => ToggleState::Selected,
        (true, true) => ToggleState::Indeterminate,
        _ => ToggleState::Unselected,
    }
}

pub struct DiffPanel {
    workspace: Option<PathBuf>,
    /// The project's repository first, then the nested ones.
    repositories: Vec<PathBuf>,
    /// The repository shown; `None` until the first load picks one.
    repository: Option<PathBuf>,
    /// The repository shown in each other project.
    chosen_repositories: HashMap<PathBuf, PathBuf>,
    /// The repositories shown before, by root, as last read.
    stashed: HashMap<PathBuf, (Changes, RepoStatus)>,
    repository_menu: bool,
    state: State,
    /// A refresh is in flight while the previous result stays on screen.
    refreshing: bool,
    /// A refresh was asked for while one was in flight; it runs when that
    /// one lands, so the state after a git action is never skipped.
    refresh_pending: bool,
    branch_picker: Option<BranchPicker>,
    /// Pull or Push waiting for the user to pick a remote.
    remote_menu: Option<GitAction>,
    /// The popover a click outside closed last, and when.
    closed_popover: Option<(Popover, Instant)>,
    commit_input: Entity<TextInput>,
    /// Unsent commit messages of the other repositories, by root.
    drafts: HashMap<PathBuf, String>,
    amend: bool,
    busy: Option<GitAction>,
    action_error: Option<String>,
    installation: Option<Installation>,
    /// Whether this pi has the print mode Generate needs; `None` while the
    /// probe runs.
    print_mode: Option<bool>,
    _task: Task<()>,
    _action_task: Task<()>,
    _probe_task: Task<()>,
    _subscriptions: Vec<Subscription>,
    /// Takes the keyboard back when the branch picker closes.
    focus_handle: FocusHandle,
    animation: AnimationTicker,
}

impl EventEmitter<DiffPanelEvent> for DiffPanel {}

impl DiffPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let commit_input = cx.new(|cx| TextInput::new(commit::COMMIT_PLACEHOLDER, cx).compact());
        // The Commit button depends on whether the message is empty.
        let subscriptions = vec![cx.observe(&commit_input, |_, _, cx| cx.notify())];
        Self {
            workspace: None,
            repositories: Vec::new(),
            repository: None,
            chosen_repositories: HashMap::new(),
            stashed: HashMap::new(),
            repository_menu: false,
            state: State::NoProject,
            refreshing: false,
            refresh_pending: false,
            branch_picker: None,
            remote_menu: None,
            closed_popover: None,
            commit_input,
            drafts: HashMap::new(),
            amend: false,
            busy: None,
            action_error: None,
            installation: None,
            print_mode: None,
            _task: Task::ready(()),
            _action_task: Task::ready(()),
            _probe_task: Task::ready(()),
            _subscriptions: subscriptions,
            focus_handle: cx.focus_handle(),
            animation: AnimationTicker::default(),
        }
    }

    /// Follows the selected project. Returns true when it changed.
    pub fn set_workspace(&mut self, workspace: Option<PathBuf>, cx: &mut Context<Self>) -> bool {
        if self.workspace == workspace {
            return false;
        }
        self.save_draft(cx);
        self.stash_loaded();
        if let Some(previous) = self.workspace.take()
            && let Some(repository) = self.repository.take()
        {
            self.chosen_repositories.insert(previous, repository);
        }
        self.workspace = workspace;
        self.repository = self
            .workspace
            .as_ref()
            .and_then(|workspace| self.chosen_repositories.get(workspace).cloned());
        self.restore_draft(cx);
        self.repositories = self
            .workspace
            .as_deref()
            .map(git::known_repositories)
            .unwrap_or_default();
        self.state = if self.workspace.is_some() {
            self.unstash_loaded()
        } else {
            State::NoProject
        };
        self.drop_repository_work();
        cx.notify();
        true
    }

    /// Shows another repository of the project.
    fn select_repository(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.repository_menu = false;
        if self.repository.as_ref() == Some(&root) {
            cx.notify();
            return;
        }
        self.save_draft(cx);
        self.stash_loaded();
        self.repository = Some(root);
        self.restore_draft(cx);
        self.state = self.unstash_loaded();
        self.drop_repository_work();
        self.refresh(cx);
    }

    /// Keeps the repository shown as it was last read, to show it at once
    /// on coming back while the refresh reads it again.
    fn stash_loaded(&mut self) {
        if let State::Loaded(changes, status) = std::mem::replace(&mut self.state, State::Loading) {
            self.stashed.insert(changes.root.clone(), (changes, status));
        }
    }

    /// The stashed state of the repository picked now, or `Loading`.
    fn unstash_loaded(&mut self) -> State {
        self.repository
            .as_ref()
            .and_then(|repository| self.stashed.remove(repository))
            .map_or(State::Loading, |(changes, status)| {
                State::Loaded(changes, status)
            })
    }

    /// Forgets the popovers, the load and the action in flight: they belong
    /// to the repository shown before, and their results are dropped.
    fn drop_repository_work(&mut self) {
        self.branch_picker = None;
        self.remote_menu = None;
        self.repository_menu = false;
        self._task = Task::ready(());
        self.refreshing = false;
        self.refresh_pending = false;
        self._action_task = Task::ready(());
        self.busy = None;
        self.action_error = None;
        self.amend = false;
    }

    /// Keeps the unsent commit message of the repository shown.
    fn save_draft(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository.clone() else {
            return;
        };
        let draft = self.commit_input.read(cx).text().to_string();
        if draft.trim().is_empty() {
            self.drafts.remove(&repository);
        } else {
            self.drafts.insert(repository, draft);
        }
    }

    fn restore_draft(&mut self, cx: &mut Context<Self>) {
        let draft = self
            .repository
            .as_ref()
            .and_then(|repository| self.drafts.remove(repository))
            .unwrap_or_default();
        self.commit_input
            .update(cx, |input, cx| input.set_text(draft, cx));
    }

    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    /// The pi that writes commit messages. A new one is probed once for
    /// print mode in the background; the answer is kept until it changes.
    pub fn set_installation(&mut self, installation: Option<Installation>, cx: &mut Context<Self>) {
        let unchanged = match (&self.installation, &installation) {
            (Some(current), Some(new)) => {
                current.executable == new.executable && current.version == new.version
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        self.installation = installation.clone();
        self.print_mode = None;
        self._probe_task = match installation {
            Some(installation) => {
                let probe = cx.background_spawn(async move {
                    let supported = pi::supports_print_mode(&installation);
                    if !supported {
                        eprintln!(
                            "pi-gpui: pi {} has no print mode; commit messages cannot be generated",
                            installation.version
                        );
                    }
                    supported
                });
                cx.spawn(async move |this, cx| {
                    let supported = probe.await;
                    this.update(cx, |this, cx| {
                        this.print_mode = Some(supported);
                        cx.notify();
                    })
                    .ok();
                })
            }
            None => Task::ready(()),
        };
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        if self.refreshing {
            self.refresh_pending = true;
            return;
        }
        self.refreshing = true;
        let preferred = self.repository.clone();
        let load =
            cx.background_spawn(async move { load_repository(&workspace, preferred.as_deref()) });
        self._task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                this.state = match result {
                    Ok(Some((repositories, changes, status))) => {
                        this.repositories = repositories;
                        // The first load, or the one shown before is gone.
                        if this.repository.as_ref() != Some(&changes.root) {
                            this.save_draft(cx);
                            this.repository = Some(changes.root.clone());
                            this.restore_draft(cx);
                        }
                        State::Loaded(changes, status)
                    }
                    Ok(None) => {
                        this.repositories.clear();
                        State::NotARepository
                    }
                    Err(error) => {
                        eprintln!("pi-gpui: git diff failed: {error}");
                        State::Failed(error)
                    }
                };
                if std::mem::take(&mut this.refresh_pending) {
                    this.refresh(cx);
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// "3 files  +12 −4" for the panel header.
    pub fn summary(&self) -> Option<String> {
        let State::Loaded(changes, _) = &self.state else {
            return None;
        };
        if changes.files.is_empty() {
            return None;
        }
        let additions: usize = changes.files.iter().map(|file| file.additions).sum();
        let deletions: usize = changes.files.iter().map(|file| file.deletions).sum();
        let files = match changes.files.len() {
            1 => "1 file".to_string(),
            count => format!("{count} files"),
        };
        Some(format!("{files}  +{additions} −{deletions}"))
    }

    fn status(&self) -> Option<&RepoStatus> {
        match &self.state {
            State::Loaded(_, status) => Some(status),
            _ => None,
        }
    }

    /// Runs `job` against the repository root in the background, then shows
    /// its error, refreshes and tells the hub the repository changed.
    fn run_git(
        &mut self,
        action: GitAction,
        job: impl FnOnce(&Path) -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let (Some(workspace), State::Loaded(changes, _)) = (self.workspace.clone(), &self.state)
        else {
            return;
        };
        let root = changes.root.clone();
        self.busy = Some(action);
        self.action_error = None;
        let work = cx.background_spawn(async move { job(&root) });
        self._action_task = cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                if this.workspace.as_ref() != Some(&workspace) {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(()) if action == GitAction::Commit => {
                        this.commit_input.update(cx, |input, cx| input.clear(cx));
                        this.amend = false;
                    }
                    Ok(()) => {}
                    Err(error) => this.show_error(action, error),
                }
                this.refresh(cx);
                cx.emit(DiffPanelEvent::RepositoryChanged);
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn show_error(&mut self, action: GitAction, error: String) {
        let message = format!("{}: {error}", action.failure());
        eprintln!("pi-gpui: {message}");
        self.action_error = Some(message);
    }

    /// Unstaged or partly staged → stage it fully; staged → unstage it.
    fn toggle_staged(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(entry) = self
            .status()
            .and_then(|status| status.entries.iter().find(|entry| entry.path == path))
            .cloned()
        else {
            return;
        };
        let stage = stage_state(&entry) != ToggleState::Selected;
        self.run_git(
            GitAction::ToggleFile,
            move |root| {
                let entries = [&entry];
                let result = if stage {
                    git::stage(root, &entries)
                } else {
                    git::unstage(root, &entries)
                };
                result.map_err(GitError::message)
            },
            cx,
        );
    }

    fn stage_all(&mut self, cx: &mut Context<Self>) {
        self.run_git(
            GitAction::StageAll,
            {
                let nested = self
                    .status()
                    .map(|status| status.nested.clone())
                    .unwrap_or_default();
                move |root| git::stage_all(root, &nested).map_err(GitError::message)
            },
            cx,
        );
    }

    fn unstage_all(&mut self, cx: &mut Context<Self>) {
        self.run_git(
            GitAction::UnstageAll,
            |root| git::unstage_all(root).map_err(GitError::message),
            cx,
        );
    }

    /// Pull and Push ask for the remote first when there are several.
    fn start_network(&mut self, action: GitAction, cx: &mut Context<Self>) {
        let has_choice = action != GitAction::Fetch
            && self
                .status()
                .is_some_and(|status| status.remotes.len() > 1 && status.branch.is_some());
        if has_choice {
            if !self.popover_just_closed(Popover::Remotes(action)) {
                self.branch_picker = None;
                self.repository_menu = false;
                self.remote_menu = Some(action);
                cx.notify();
            }
            return;
        }
        self.run_network(action, cx);
    }

    fn popover_just_closed(&self, popover: Popover) -> bool {
        self.closed_popover
            .is_some_and(|(closed, at)| closed == popover && at.elapsed() < POPOVER_REOPEN_GUARD)
    }

    /// Closes the open popover after a click outside it.
    fn dismiss_popover(&mut self, popover: Popover, cx: &mut Context<Self>) {
        self.branch_picker = None;
        self.remote_menu = None;
        self.repository_menu = false;
        self.closed_popover = Some((popover, Instant::now()));
        cx.notify();
    }

    fn run_network(&mut self, action: GitAction, cx: &mut Context<Self>) {
        let operation: fn(&Path) -> Result<String, String> = match action {
            GitAction::Fetch => git::fetch,
            GitAction::Pull => git::pull,
            GitAction::Push => git::push,
            _ => return,
        };
        self.run_git(action, move |root| operation(root).map(|_| ()), cx);
    }

    fn run_network_with(&mut self, action: GitAction, remote: String, cx: &mut Context<Self>) {
        self.remote_menu = None;
        let Some(target) = self
            .status()
            .and_then(|status| git::RemoteTarget::new(status, remote))
        else {
            return;
        };
        let operation: fn(&Path, &git::RemoteTarget) -> Result<String, String> = match action {
            GitAction::Pull => git::pull_from,
            GitAction::Push => git::push_to,
            _ => return,
        };
        self.run_git(action, move |root| operation(root, &target).map(|_| ()), cx);
    }

    fn confirm_discard(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = self.status() else {
            return;
        };
        let Some(entry) = status
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };
        let steps = Discard::plan(&entry, status.is_unborn());
        let deletes = steps
            .iter()
            .any(|step| matches!(step, Discard::Remove { .. }));
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Discard all changes to \"{name}\"?"),
            Some(if deletes {
                "The file is new since the last commit: it moves to the Trash."
            } else {
                "The file goes back to its last committed version. This cannot be undone."
            }),
            &["Discard", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update(cx, |this, cx| {
                if this.busy.is_some() {
                    this.show_error(GitAction::Discard, "another git action is running".into());
                    cx.notify();
                    return;
                }
                this.run_git(
                    GitAction::Discard,
                    move |root| discard(root, &entry, &steps),
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    fn render_message(&self, message: String) -> AnyElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .px(px(24.))
            .child(
                div().text_center().child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                ),
            )
            .into_any_element()
    }

    /// Branch, distance to the upstream, and the network actions.
    fn render_branch_bar(&self, status: &RepoStatus, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let branch = match (&status.branch, status.upstream_remote()) {
            (Some(branch), Some(remote)) => format!("{remote} / {branch}"),
            (Some(branch), None) => branch.clone(),
            (None, _) => "detached HEAD".to_string(),
        };
        let distance = status.upstream.as_ref().map(|upstream| {
            (
                [("↑", status.ahead), ("↓", status.behind)],
                format!(
                    "{} ahead of and {} behind {upstream}",
                    status.ahead, status.behind
                ),
            )
        });
        h_flex()
            .flex_none()
            .h(ui_px(cx, 30.))
            .pl(px(12.))
            .pr(px(6.))
            .gap(px(6.))
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                Icon::new(IconName::Fork)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(6.))
                    .children(self.render_repository_switcher(cx))
                    // The picker is a sibling of the name, not its child, so
                    // clicks inside it never reach the name's toggle.
                    .child(
                        div()
                            .relative()
                            .min_w(px(0.))
                            .child(
                                h_flex()
                                    .id("branch-name")
                                    .min_w(px(0.))
                                    .h(ui_px(cx, 22.))
                                    .px(px(4.))
                                    .gap(px(2.))
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .when(self.branch_picker.is_some(), |this| {
                                        this.bg(colors.ghost_element_selected)
                                    })
                                    .hover(|style| style.bg(colors.ghost_element_hover))
                                    .tooltip(Tooltip::text("Switch or create a branch"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_branch_picker(window, cx)
                                    }))
                                    .child(
                                        div().min_w(px(0.)).child(
                                            Label::new(branch)
                                                .size(LabelSize::XSmall)
                                                .weight(FontWeight::MEDIUM)
                                                .truncate_start(),
                                        ),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(Color::Placeholder),
                                    ),
                            )
                            .children(self.render_branch_picker(cx)),
                    )
                    .when_some(distance, |this, (distance, tooltip)| {
                        this.child(
                            div()
                                .id("branch-distance")
                                .flex_none()
                                .tooltip(Tooltip::text(tooltip))
                                .flex()
                                .gap(px(6.))
                                .children(distance.map(|(arrow, count)| {
                                    h_flex()
                                        .gap(px(2.))
                                        .child(
                                            Label::new(arrow)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Label::new(count.to_string())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                })),
                        )
                    }),
            )
            .child(self.render_network_button(
                "git-fetch",
                IconName::Refresh,
                GitAction::Fetch,
                "Fetch",
                cx,
            ))
            .child(self.render_network_button(
                "git-pull",
                IconName::ArrowDown,
                GitAction::Pull,
                "Pull (fast-forward only)",
                cx,
            ))
            .child(self.render_network_button(
                "git-push",
                IconName::ArrowUp,
                GitAction::Push,
                "Push",
                cx,
            ))
            .into_any_element()
    }

    /// The repository shown and its menu, with a slash before the branch;
    /// `None` when the project has only one repository.
    fn render_repository_switcher(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.repositories.len() < 2 {
            return None;
        }
        let colors = cx.theme().colors.clone();
        let name = self
            .repository
            .as_deref()
            .map(|root| self.repository_name(root))
            .unwrap_or_default();
        Some(
            h_flex()
                .flex_shrink()
                .min_w(px(0.))
                .gap(px(4.))
                .child(
                    div()
                        .relative()
                        .min_w(px(0.))
                        .child(
                            h_flex()
                                .id("repository-name")
                                .min_w(px(0.))
                                .h(ui_px(cx, 22.))
                                .px(px(4.))
                                .gap(px(2.))
                                .rounded(px(4.))
                                .cursor_pointer()
                                .when(self.repository_menu, |this| {
                                    this.bg(colors.ghost_element_selected)
                                })
                                .hover(|style| style.bg(colors.ghost_element_hover))
                                .tooltip(Tooltip::text("Switch repository"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.toggle_repository_menu(cx)),
                                )
                                .child(
                                    div().min_w(px(0.)).child(
                                        Label::new(name)
                                            .size(LabelSize::XSmall)
                                            .weight(FontWeight::MEDIUM)
                                            .truncate_start(),
                                    ),
                                )
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Placeholder),
                                ),
                        )
                        .children(
                            self.repository_menu
                                .then(|| self.render_repository_menu(cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn toggle_repository_menu(&mut self, cx: &mut Context<Self>) {
        if self.popover_just_closed(Popover::Repositories) {
            return;
        }
        self.branch_picker = None;
        self.remote_menu = None;
        self.repository_menu = true;
        cx.notify();
    }

    /// A repository as the project sees it: its folder relative to the
    /// project, or its own name for the one holding the project.
    fn repository_name(&self, root: &Path) -> String {
        self.workspace
            .as_deref()
            .and_then(|workspace| root.strip_prefix(workspace).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| root.display().to_string())
    }

    /// The project's repositories, under the repository name.
    fn render_repository_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let rows = self.repositories.iter().enumerate().map(|(index, root)| {
            let is_current = self.repository.as_ref() == Some(root);
            let root = root.clone();
            ListItem::new(("repository", index))
                .start_slot(
                    div()
                        .w(px(14.))
                        .flex()
                        .justify_center()
                        .when(is_current, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                        }),
                )
                .child(
                    Label::new(self.repository_name(&root))
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_repository(root.clone(), cx)),
                )
        });
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .child(
                deferred(
                    anchored()
                        .anchor(Corner::TopLeft)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            v_flex()
                                .id("repository-menu")
                                .w(px(260.))
                                .max_h(px(320.))
                                .overflow_y_scroll()
                                .mt(px(4.))
                                .p(px(4.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(colors.border)
                                .bg(colors.elevated_surface_background)
                                .shadow_lg()
                                .occlude()
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.dismiss_popover(Popover::Repositories, cx)
                                }))
                                .children(rows),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    fn render_network_button(
        &self,
        id: &'static str,
        icon: IconName,
        action: GitAction,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = IconButton::new(id, icon)
            .size(ButtonSize::Compact)
            .icon_size(IconSize::XSmall)
            .loading(self.busy == Some(action))
            .disabled(self.busy.is_some())
            .toggle_state(self.remote_menu == Some(action))
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |this, _, _, cx| this.start_network(action, cx)));
        div()
            .relative()
            .flex_none()
            .child(button)
            .children(
                (self.remote_menu == Some(action)).then(|| self.render_remote_menu(action, cx)),
            )
            .into_any_element()
    }

    /// The remotes to pull from or push to, under the clicked button.
    fn render_remote_menu(&self, action: GitAction, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (remotes, current) = match self.status() {
            Some(status) => (
                status.remotes.clone(),
                status.upstream_remote().map(str::to_string),
            ),
            None => (Vec::new(), None),
        };
        let verb = if action == GitAction::Pull {
            "Pull from"
        } else {
            "Push to"
        };
        let rows = remotes.into_iter().enumerate().map(|(index, remote)| {
            let is_current = current.as_deref() == Some(remote.as_str());
            ListItem::new(("remote", index))
                .start_slot(
                    div()
                        .w(px(14.))
                        .flex()
                        .justify_center()
                        .when(is_current, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                        }),
                )
                .child(Label::new(remote.clone()).size(LabelSize::Small).truncate())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_network_with(action, remote.clone(), cx)
                }))
        });
        div()
            .absolute()
            .bottom_0()
            .right_0()
            .child(
                deferred(
                    anchored()
                        .anchor(Corner::TopRight)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            v_flex()
                                .id("remote-menu")
                                .w(px(200.))
                                .mt(px(4.))
                                .p(px(4.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(colors.border)
                                .bg(colors.elevated_surface_background)
                                .shadow_lg()
                                .occlude()
                                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                                    this.dismiss_popover(Popover::Remotes(action), cx)
                                }))
                                .child(
                                    div().px(px(8.)).pt(px(2.)).pb(px(4.)).child(
                                        Label::new(verb)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Placeholder),
                                    ),
                                )
                                .children(rows),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// The diff summary and the bulk staging buttons.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let staging = self.status().map(|status| {
            let can_stage = status.entries.iter().any(|entry| entry.unstaged);
            (can_stage, status.has_staged())
        });
        h_flex()
            .flex_none()
            .h(ui_px(cx, 30.))
            .pl(px(12.))
            .pr(px(6.))
            .gap(px(6.))
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .children(self.summary().map(|summary| {
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .when_some(staging, |this, (can_stage, can_unstage)| {
                this.child(
                    IconButton::new("stage-all", IconName::Plus)
                        .size(ButtonSize::Compact)
                        .icon_size(IconSize::XSmall)
                        .loading(self.busy == Some(GitAction::StageAll))
                        .disabled(self.busy.is_some() || !can_stage)
                        .tooltip(Tooltip::text(
                            "Stage All: every change, untracked files included",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.stage_all(cx))),
                )
                .child(
                    IconButton::new("unstage-all", IconName::Dash)
                        .size(ButtonSize::Compact)
                        .icon_size(IconSize::XSmall)
                        .loading(self.busy == Some(GitAction::UnstageAll))
                        .disabled(self.busy.is_some() || !can_unstage)
                        .tooltip(Tooltip::text(
                            "Unstage All: take everything out of the index",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.unstage_all(cx))),
                )
            })
            .into_any_element()
    }

    fn render_stage_checkbox(
        &self,
        file: &FileDiff,
        entry: Option<&StatusEntry>,
        cx: &mut Context<Self>,
    ) -> Checkbox {
        let id = ElementId::Name(format!("stage:{}", file.path).into());
        let Some(entry) = entry else {
            // git status has not caught up with the diff yet.
            return Checkbox::new(id, ToggleState::Unselected).disabled(true);
        };
        let state = stage_state(entry);
        let tooltip = match state {
            ToggleState::Selected => "Unstage",
            ToggleState::Indeterminate if entry.conflicted => "Stage (marks the conflict resolved)",
            ToggleState::Indeterminate => "Stage the rest of this file",
            ToggleState::Unselected => "Stage",
        };
        let path = file.path.clone();
        Checkbox::new(id, state)
            .disabled(self.busy.is_some())
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_staged(&path, cx)))
    }

    fn render_file(
        &self,
        file: &FileDiff,
        root: &Path,
        entry: Option<&StatusEntry>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (directory, name) = match file.path.rsplit_once('/') {
            Some((directory, name)) => (format!("{directory}/"), name.to_string()),
            None => (String::new(), file.path.clone()),
        };
        let full_path = root.join(&file.path);
        let tooltip = match &file.old_path {
            Some(old_path) => format!("{} from {old_path}", file.status.description()),
            None => format!(
                "{}: {}",
                file.status.description(),
                sessions::display_path(&full_path)
            ),
        };
        let checkbox = self.render_stage_checkbox(file, entry, cx);
        let group: gpui::SharedString = format!("diff-file:{}", file.path).into();
        let title = file.path.clone();
        let discard_path = file.path.clone();
        let discard = IconButton::new(
            ElementId::Name(format!("discard:{}", file.path).into()),
            IconName::Undo,
        )
        .size(ButtonSize::Compact)
        .icon_size(IconSize::XSmall)
        .disabled(self.busy.is_some() || entry.is_none_or(|entry| entry.conflicted))
        .tooltip(Tooltip::text(
            if entry.is_some_and(|entry| entry.conflicted) {
                "Resolve the conflict first"
            } else {
                "Discard changes"
            },
        ))
        .on_click(
            cx.listener(move |this, _, window, cx| this.confirm_discard(&discard_path, window, cx)),
        );

        h_flex()
            .id(ElementId::Name(group.clone()))
            .group(group.clone())
            .h(ui_px(cx, 28.))
            .pl(px(4.))
            .pr(px(4.))
            .gap(px(6.))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(DiffPanelEvent::OpenDiff(full_path.clone(), title.clone()))
            }))
            .child(checkbox)
            .child(
                div().flex_none().w(px(12.)).child(
                    Label::new(file.status.letter())
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::MEDIUM)
                        .color(file.status.color()),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(4.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .child(Label::new(name).size(LabelSize::Small)),
                    )
                    .child(
                        div().min_w(px(0.)).child(
                            Label::new(directory)
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder)
                                .truncate(),
                        ),
                    ),
            )
            .when(file.additions > 0, |this| {
                this.child(
                    Label::new(format!("+{}", file.additions))
                        .size(LabelSize::XSmall)
                        .color(Color::Success),
                )
            })
            .when(file.deletions > 0, |this| {
                this.child(
                    Label::new(format!("−{}", file.deletions))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            // Shown on hover; transparent rather than absent keeps the row's
            // layout stable (gpui 0.2.2 has no `invisible()`).
            .child(
                div()
                    .flex_none()
                    .opacity(0.)
                    .group_hover(group, |style| style.opacity(1.))
                    .child(discard),
            )
            .into_any_element()
    }
}

impl Render for DiffPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let loading = matches!(self.state, State::Loading);
        self.animation
            .set_active(loading || self.refreshing || self.busy.is_some(), cx);
        let content = match &self.state {
            State::NoProject => self.render_message("Select a project to see its changes.".into()),
            State::Loading => h_flex()
                .flex_1()
                .justify_center()
                .gap(px(8.))
                .child(Spinner::new().size(IconSize::Small).color(Color::Muted))
                .child(LoadingLabel::new("Reading git diff…").size(LabelSize::Small))
                .into_any_element(),
            State::NotARepository => {
                self.render_message("This project is not a git repository.".into())
            }
            State::Failed(error) => self.render_message(format!("git diff failed: {error}")),
            State::Loaded(changes, _) if changes.files.is_empty() => {
                self.render_message("No changes against HEAD.".into())
            }
            State::Loaded(changes, status) => {
                let root = changes.root.clone();
                let files: Vec<AnyElement> = changes
                    .files
                    .iter()
                    .map(|file| {
                        let entry = status.entries.iter().find(|entry| entry.path == file.path);
                        self.render_file(file, &root, entry, cx)
                    })
                    .collect();
                div()
                    .id("diff-files")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .px(px(6.))
                    .pb(px(10.))
                    .children(files)
                    .into_any_element()
            }
        };
        let (branch_bar, footer) = match &self.state {
            State::Loaded(_, status) => (
                Some(self.render_branch_bar(status, cx)),
                Some(self.render_commit_footer(status, cx)),
            ),
            _ => (None, None),
        };
        let toolbar = self.workspace.is_some().then(|| self.render_toolbar(cx));
        // Scrolled to the highlight once; the wheel owns the list after that.
        if let Some(picker) = &mut self.branch_picker {
            picker.scroll_to_highlight = false;
        }
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .children(branch_bar)
            .children(toolbar)
            .child(content)
            .children(footer)
    }
}

/// The project's repositories, then the diff and the status of the
/// `preferred` one (the first when it is gone), read in one background job.
/// `Ok(None)` when the project holds no git work tree.
fn load_repository(
    workspace: &Path,
    preferred: Option<&Path>,
) -> Result<Option<(Vec<PathBuf>, Changes, RepoStatus)>, String> {
    let repositories = git::repositories(workspace).map_err(GitError::message)?;
    let Some(root) = preferred
        .and_then(|preferred| repositories.iter().find(|root| *root == preferred))
        .or(repositories.first())
        .cloned()
    else {
        return Ok(None);
    };
    // Every git run here is independent, so they all run at once.
    let (changes, status, remotes) = std::thread::scope(|scope| {
        let status = scope.spawn(|| git::status(&root));
        let remotes = scope.spawn(|| git::remotes(&root));
        let changes = load_changes(root.clone());
        (changes, git::joined(status), git::joined(remotes))
    });
    let changes = changes?;
    let mut status = status.map_err(GitError::message)?;
    // The remotes only feed the remote picker; the panel works without them.
    status.remotes = remotes.unwrap_or_default();
    Ok(Some((repositories, changes, status)))
}

fn load_changes(root: PathBuf) -> Result<Changes, String> {
    let (diff, untracked) = std::thread::scope(|scope| {
        let untracked =
            scope.spawn(|| git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]));
        let diff = diff_base(&root).and_then(|base| {
            git(
                &root,
                &[
                    "-c",
                    "core.quotePath=false",
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--find-renames",
                    &base,
                ],
            )
            .map_err(GitError::message)
        });
        (diff, git::joined(untracked))
    });
    let mut files = parse_diff(&diff?);
    let untracked = untracked.map_err(GitError::message)?;
    // A folder is a nested repository, which keeps its own changes.
    for path in untracked
        .split('\0')
        .filter(|path| !path.is_empty() && !path.ends_with('/'))
    {
        files.push(untracked_file(&root, path));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Changes { root, files })
}

/// HEAD, or the empty tree in a fresh repository that has no HEAD yet.
fn diff_base(directory: &Path) -> Result<String, String> {
    match git(directory, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
        Ok(_) => Ok("HEAD".to_string()),
        Err(_) => Ok(git(directory, &["hash-object", "-t", "tree", "/dev/null"])
            .map_err(GitError::message)?
            .trim()
            .to_string()),
    }
}

/// The change of one file against HEAD, as the Changes panel shows it.
/// `Ok(None)` when the file is unchanged, ignored or outside a git work tree.
pub fn load_file_diff(path: &Path) -> Result<Option<FileDiff>, String> {
    // A deleted file may have taken its folders with it; git runs from the
    // nearest folder that still exists.
    let Some(directory) = path
        .ancestors()
        .skip(1)
        .find(|directory| directory.is_dir())
    else {
        return Ok(None);
    };
    let Ok(relative) = path.strip_prefix(directory) else {
        return Ok(None);
    };
    let relative = relative.to_string_lossy().into_owned();
    let base = match diff_base(directory) {
        Ok(base) => base,
        Err(_) => return Ok(None),
    };
    let diff = match git(
        directory,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            &base,
            "--",
            &relative,
        ],
    ) {
        Ok(diff) => diff,
        Err(GitError::Failed(_)) => return Ok(None),
        Err(GitError::Spawn(error)) => return Err(error),
    };
    if let Some(file) = parse_diff(&diff).into_iter().next() {
        return Ok(Some(file));
    }
    let untracked = git(
        directory,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            &relative,
        ],
    )
    .map_err(GitError::message)?;
    if untracked.split('\0').all(str::is_empty) {
        return Ok(None);
    }
    Ok(Some(untracked_diff(path, relative)))
}

/// Runs the steps of a discard in order; a file new since HEAD goes to the
/// Trash, so it can still be brought back. The steps were planned when the
/// user was asked; if the file's status moved on since (the agent committed
/// or renamed it), nothing runs.
fn discard(root: &Path, planned_for: &StatusEntry, steps: &[Discard]) -> Result<(), String> {
    let status = git::status(root).map_err(GitError::message)?;
    let current = status
        .entries
        .iter()
        .find(|entry| entry.path == planned_for.path);
    if current != Some(planned_for) {
        return Err("the file changed since you asked; nothing was discarded".to_string());
    }
    for step in steps {
        match step {
            Discard::Restore(path) => {
                git::restore_from_head(root, path).map_err(GitError::message)?;
            }
            Discard::Remove { path, staged } => {
                if *staged {
                    git::remove_from_index(root, path).map_err(GitError::message)?;
                }
                let full_path = root.join(path);
                if full_path.exists() {
                    crate::platform::move_to_trash(&full_path)?;
                }
            }
        }
    }
    Ok(())
}

fn untracked_file(root: &Path, path: &str) -> FileDiff {
    untracked_diff(&root.join(path), path.to_string())
}

/// An untracked file as a diff that adds all of it; `path` is the one shown.
fn untracked_diff(full_path: &Path, path: String) -> FileDiff {
    let mut file = FileDiff {
        path,
        old_path: None,
        status: FileStatus::Untracked,
        binary: false,
        additions: 0,
        deletions: 0,
        lines: Vec::new(),
    };
    let too_big =
        fs::metadata(full_path).is_ok_and(|metadata| metadata.len() > MAX_UNTRACKED_BYTES);
    match fs::read(full_path) {
        Ok(bytes) if !too_big && !bytes.contains(&0) => {
            let text = String::from_utf8_lossy(&bytes);
            file.lines = text
                .lines()
                .map(|line| DiffLine {
                    kind: LineKind::Added,
                    text: line.to_string(),
                })
                .collect();
            file.additions = file.lines.len();
        }
        _ => file.binary = true,
    }
    file
}

/// Parses `git diff` output (unified format, no color).
pub fn parse_diff(output: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let path = rest
                .split_once(" b/")
                .map(|(_, path)| path)
                .unwrap_or(rest)
                .to_string();
            files.push(FileDiff {
                path,
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
            file.lines.push(DiffLine {
                kind: LineKind::Hunk,
                text: line.to_string(),
            });
            continue;
        }
        if !in_hunk {
            if line.starts_with("new file mode") {
                file.status = FileStatus::Added;
            } else if line.starts_with("deleted file mode") {
                file.status = FileStatus::Deleted;
            } else if let Some(from) = line.strip_prefix("rename from ") {
                file.status = FileStatus::Renamed;
                file.old_path = Some(from.to_string());
            } else if let Some(to) = line.strip_prefix("rename to ") {
                file.path = to.to_string();
            } else if line.starts_with("Binary files ") {
                file.binary = true;
            } else if let Some(path) = line.strip_prefix("+++ b/") {
                file.path = path.to_string();
            }
            continue;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // "\ No newline at end of file" and anything unexpected.
            _ => continue,
        };
        match kind {
            LineKind::Added => file.additions += 1,
            LineKind::Removed => file.deletions += 1,
            _ => {}
        }
        file.lines.push(DiffLine {
            kind,
            text: text.to_string(),
        });
    }
    files
}

/// Syntax colors for each diff line, as ranges within the line's text. The
/// new side (context and added lines) and the old side (context and removed
/// lines) are highlighted as two continuous texts, so strings and comments
/// that span lines keep their colors. Hunk headers stay plain.
pub(crate) fn line_highlights(
    path: &str,
    lines: &[DiffLine],
    texts: &[String],
    colors: &SyntaxColors,
) -> Vec<Vec<(Range<usize>, Hsla)>> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let language = name
        .rsplit_once('.')
        .map_or(name, |(_, extension)| extension);
    let mut new_side = String::new();
    let mut old_side = String::new();
    // For each line: which side it is read from, and where it starts there.
    let mut places = Vec::with_capacity(lines.len());
    for (line, text) in lines.iter().zip(texts) {
        let place = match line.kind {
            LineKind::Hunk => None,
            LineKind::Context => {
                old_side.push_str(text);
                old_side.push('\n');
                Some((true, push_line(&mut new_side, text)))
            }
            LineKind::Added => Some((true, push_line(&mut new_side, text))),
            LineKind::Removed => Some((false, push_line(&mut old_side, text))),
        };
        places.push(place);
    }
    let new_spans = crate::highlight::highlight(&new_side, Some(language), colors);
    let old_spans = crate::highlight::highlight(&old_side, Some(language), colors);
    places
        .into_iter()
        .zip(texts)
        .map(|(place, text)| {
            let Some((new, start)) = place else {
                return Vec::new();
            };
            let Some(spans) = (if new { &new_spans } else { &old_spans }) else {
                return Vec::new();
            };
            let end = start + text.len();
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

/// Appends `text` as a line and returns where it starts.
fn push_line(side: &mut String, text: &str) -> usize {
    let start = side.len();
    side.push_str(text);
    side.push('\n');
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_each_side_of_a_rust_diff() {
        let lines = vec![
            DiffLine {
                kind: LineKind::Hunk,
                text: "@@ -1,2 +1,2 @@".into(),
            },
            DiffLine {
                kind: LineKind::Context,
                text: "fn main() {".into(),
            },
            DiffLine {
                kind: LineKind::Removed,
                text: "    let a = 1;".into(),
            },
            DiffLine {
                kind: LineKind::Added,
                text: "    let b = \"two\";".into(),
            },
        ];
        let texts: Vec<String> = lines.iter().map(|line| line.text.clone()).collect();
        let colors = crate::ui::Theme::ayu(true).colors.syntax;
        let spans = line_highlights("src/main.rs", &lines, &texts, &colors);
        assert!(spans[0].is_empty(), "hunk headers stay plain");
        for (spans, text) in spans.iter().zip(&texts).skip(1) {
            assert!(!spans.is_empty(), "no colors in {text:?}");
            for (range, _) in spans {
                assert!(range.end <= text.len());
            }
        }
        let keyword = |index: usize, word: &str| {
            let start = texts[index].find(word).unwrap();
            spans[index].iter().any(|(range, _)| range.contains(&start))
        };
        assert!(keyword(1, "fn"));
        assert!(keyword(2, "let"));
        assert!(keyword(3, "\"two\""));
    }

    #[test]
    fn parses_modified_added_deleted_and_renamed_files() {
        let output = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@ fn main() {
 keep
-old
+new
\\ No newline at end of file
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/a b.txt b/c d.txt
similarity index 100%
rename from a b.txt
rename to c d.txt
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = parse_diff(output);
        assert_eq!(files.len(), 5);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        assert_eq!(files[0].lines.len(), 4);
        assert_eq!(files[0].lines[0].kind, LineKind::Hunk);
        assert_eq!(files[1].status, FileStatus::Added);
        assert_eq!(files[1].additions, 1);
        assert_eq!(files[2].status, FileStatus::Deleted);
        assert_eq!(files[2].path, "gone.txt");
        assert_eq!(files[3].status, FileStatus::Renamed);
        assert_eq!(files[3].path, "c d.txt");
        assert_eq!(files[3].old_path.as_deref(), Some("a b.txt"));
        assert!(files[4].binary);
    }
}
