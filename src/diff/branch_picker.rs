//! The branch switcher of the Changes panel, after Zed's branch picker: the
//! current branch first, then the most recently committed local and remote
//! branches; typing filters them, and a name no branch has offers to create
//! it. Picking a remote branch checks out a local branch that tracks it.

use std::time::{Duration, UNIX_EPOCH};

use gpui::{Corner, Focusable, ScrollHandle, anchored, deferred};

use super::*;
use crate::git::Branch;

pub(super) const BRANCH_PICKER_CONTEXT: &str = "BranchPicker";

pub(super) struct BranchPicker {
    filter: Entity<TextInput>,
    /// `None` while the list loads.
    branches: Option<Vec<Branch>>,
    error: Option<String>,
    highlighted: usize,
    scroll: ScrollHandle,
    pub(super) scroll_to_highlight: bool,
    _filter_subscription: Subscription,
    _load: Task<()>,
}

#[derive(Clone)]
enum Entry {
    Branch(Branch),
    NewBranch(String),
}

impl DiffPanel {
    pub(super) fn toggle_branch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_picker.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
            return;
        }
        if self.popover_just_closed(Popover::Branches) {
            return;
        }
        let State::Loaded(changes, _) = &self.state else {
            return;
        };
        let root = changes.root.clone();
        let filter = cx.new(|cx| TextInput::single_line("Switch or type to create a branch…", cx));
        let filter_subscription = cx.observe(&filter, |this, _, cx| {
            if let Some(picker) = &mut this.branch_picker {
                picker.highlighted = 0;
                picker.scroll_to_highlight = true;
            }
            cx.notify();
        });
        window.focus(&filter.read(cx).focus_handle(cx));
        let load = cx.background_spawn(async move { git::branches(&root) });
        let load = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                let Some(picker) = &mut this.branch_picker else {
                    return;
                };
                match result {
                    Ok(branches) => picker.branches = Some(branches),
                    Err(error) => {
                        let error = error.message();
                        eprintln!("pi-gpui: listing branches failed: {error}");
                        picker.branches = Some(Vec::new());
                        picker.error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.remote_menu = None;
        self.repository_menu = false;
        self.branch_picker = Some(BranchPicker {
            filter,
            branches: None,
            error: None,
            highlighted: 0,
            scroll: ScrollHandle::new(),
            scroll_to_highlight: true,
            _filter_subscription: filter_subscription,
            _load: load,
        });
        cx.notify();
    }

    pub(super) fn dismiss_branch_picker(
        &mut self,
        _: &DismissBranchPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.branch_picker.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    fn branch_entries(&self, cx: &App) -> Vec<Entry> {
        let Some(picker) = &self.branch_picker else {
            return Vec::new();
        };
        let Some(branches) = &picker.branches else {
            return Vec::new();
        };
        let query = picker.filter.read(cx).text().trim().to_string();
        if query.is_empty() {
            return branches.iter().cloned().map(Entry::Branch).collect();
        }
        let lowercase = query.to_lowercase();
        // Names containing the query come before the ones that only hold its
        // characters in order; each group keeps the list's own order.
        let mut entries = Vec::new();
        let mut scattered = Vec::new();
        for branch in branches {
            let name = branch.name.to_lowercase();
            if name.contains(&lowercase) {
                entries.push(Entry::Branch(branch.clone()));
            } else if is_subsequence(&lowercase, &name) {
                scattered.push(Entry::Branch(branch.clone()));
            }
        }
        entries.extend(scattered);
        let name = git::normalize_branch_name(&query);
        if !branches.iter().any(|branch| branch.name == name) {
            entries.push(Entry::NewBranch(name));
        }
        entries
    }

    fn move_branch_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.branch_entries(cx).len();
        if let Some(picker) = &mut self.branch_picker
            && count > 0
        {
            let next = (picker.highlighted as isize + delta).rem_euclid(count as isize);
            picker.highlighted = next as usize;
            picker.scroll_to_highlight = true;
            cx.notify();
        }
    }

    fn confirm_branch(
        &mut self,
        _: &crate::chat::Submit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &self.branch_picker else {
            return;
        };
        let highlighted = picker.highlighted;
        if let Some(entry) = self.branch_entries(cx).get(highlighted).cloned() {
            self.pick_branch(entry, window, cx);
        }
    }

    fn pick_branch(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        self.branch_picker = None;
        window.focus(&self.focus_handle);
        cx.notify();
        match entry {
            Entry::Branch(branch) if branch.is_head => {}
            Entry::Branch(branch) => self.run_git(
                GitAction::SwitchBranch,
                move |root| git::switch_branch(root, &branch).map_err(GitError::message),
                cx,
            ),
            Entry::NewBranch(name) => self.run_git(
                GitAction::CreateBranch,
                move |root| git::create_branch(root, &name).map_err(GitError::message),
                cx,
            ),
        }
    }

    /// Asks before deleting the local branch `name`; the picker closes, since
    /// the prompt takes the keyboard.
    fn confirm_delete_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.branch_picker = None;
        window.focus(&self.focus_handle);
        cx.notify();
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Delete branch \"{name}\"?"),
            Some("Only the local branch is deleted; a branch of the same name on a remote stays."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.delete_branch(name, false, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// `git branch -d`, or `-D` when `force`. A refused unmerged branch asks
    /// once more before the forced delete.
    fn delete_branch(
        &mut self,
        name: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            self.show_error(
                GitAction::DeleteBranch,
                "another git action is running".into(),
            );
            cx.notify();
            return;
        }
        let (Some(workspace), State::Loaded(changes, _)) = (self.workspace.clone(), &self.state)
        else {
            return;
        };
        let root = changes.root.clone();
        self.busy = Some(GitAction::DeleteBranch);
        self.action_error = None;
        let work = cx.background_spawn({
            let name = name.clone();
            async move { git::delete_branch(&root, &name, force).map_err(GitError::message) }
        });
        self._action_task = cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            this.update_in(cx, |this, window, cx| {
                if this.workspace.as_ref() != Some(&workspace) {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(()) => {}
                    Err(error) if !force && git::is_not_merged(&error) => {
                        this.confirm_force_delete(name, window, cx)
                    }
                    Err(error) => this.show_error(GitAction::DeleteBranch, error),
                }
                this.refresh(cx);
                cx.emit(DiffPanelEvent::RepositoryChanged);
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn confirm_force_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            gpui::PromptLevel::Critical,
            &format!("\"{name}\" is not fully merged"),
            Some(
                "Some of its commits are neither in the current branch nor in its upstream. Deleting it anyway can lose them; the reflog keeps them for a while.",
            ),
            &["Delete Anyway", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.delete_branch(name, true, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// The picker, anchored under the branch name.
    pub(super) fn render_branch_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.branch_picker.as_ref()?;
        let colors = cx.theme().colors.clone();
        let entries = self.branch_entries(cx);
        let highlighted = picker.highlighted.min(entries.len().saturating_sub(1));
        if picker.scroll_to_highlight {
            picker.scroll.scroll_to_item(highlighted);
        }
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| self.render_branch_entry(index, entry, index == highlighted, cx))
            .collect();
        let body = if picker.branches.is_none() {
            h_flex()
                .gap(px(6.))
                .px(px(10.))
                .py(px(10.))
                .child(Spinner::new().size(IconSize::XSmall))
                .child(LoadingLabel::new("Loading branches…").size(LabelSize::Small))
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(10.))
                .py(px(10.))
                .child(
                    Label::new("No branches")
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
                .into_any_element()
        } else {
            div()
                .id("branch-list")
                .max_h(px(320.))
                .overflow_y_scroll()
                .track_scroll(&picker.scroll)
                .p(px(4.))
                .children(rows)
                .into_any_element()
        };
        let card = v_flex()
            .id("branch-picker")
            .key_context(BRANCH_PICKER_CONTEXT)
            .w(px(340.))
            .mt(px(4.))
            .rounded(px(8.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .occlude()
            .on_action(cx.listener(Self::confirm_branch))
            .on_action(cx.listener(Self::dismiss_branch_picker))
            .on_action(
                cx.listener(|this, _: &text_input::Up, _, cx| this.move_branch_highlight(-1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &text_input::Down, _, cx| this.move_branch_highlight(1, cx)),
            )
            .on_mouse_down_out(
                cx.listener(|this, _, _, cx| this.dismiss_popover(Popover::Branches, cx)),
            )
            .child(
                div()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(picker.filter.clone()),
            )
            .children(picker.error.clone().map(|error| {
                div().px(px(10.)).py(px(6.)).child(
                    Label::new(format!("Some branches could not be loaded: {error}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                )
            }))
            .child(body);
        Some(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .child(
                    deferred(
                        anchored()
                            .anchor(Corner::TopLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(card),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    fn render_branch_entry(
        &self,
        index: usize,
        entry: Entry,
        highlighted: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon, icon_color, title, subject, age) = match &entry {
            Entry::Branch(branch) => (
                if branch.is_head {
                    IconName::Check
                } else {
                    IconName::Fork
                },
                if branch.is_head {
                    Color::Accent
                } else {
                    Color::Muted
                },
                branch.name.clone(),
                Some(branch.subject.clone()).filter(|subject| !subject.is_empty()),
                (branch.committed_at > 0).then(|| {
                    sessions::relative_age(UNIX_EPOCH + Duration::from_secs(branch.committed_at))
                }),
            ),
            Entry::NewBranch(name) => (
                IconName::Plus,
                Color::Muted,
                format!("Create branch \"{name}\"…"),
                None,
                None,
            ),
        };
        let is_remote = matches!(&entry, Entry::Branch(branch) if branch.is_remote);
        let local_only = matches!(&entry, Entry::Branch(branch) if branch.local_only);
        // Any local branch but the one checked out can go.
        let deletable = match &entry {
            Entry::Branch(branch) if !branch.is_remote && !branch.is_head => {
                Some(branch.name.clone())
            }
            _ => None,
        };
        let colors = cx.theme().colors.clone();
        ListItem::new(("branch", index))
            .toggle_state(highlighted)
            .start_slot(Icon::new(icon).size(IconSize::XSmall).color(icon_color))
            .child(
                div().min_w(px(0.)).child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .color(if is_remote {
                            Color::Muted
                        } else {
                            Color::Default
                        })
                        .truncate(),
                ),
            )
            .when(local_only, |this| {
                this.child(
                    div()
                        .id(("branch-local", index))
                        .flex_none()
                        .px(px(4.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(colors.border_variant)
                        .tooltip(Tooltip::text(
                            "On no remote: never pushed, or deleted there",
                        ))
                        .child(
                            Label::new("local")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .when_some(deletable, |this, name| {
                this.end_hover_slot(
                    IconButton::new(("branch-delete", index), IconName::Trash)
                        .size(ButtonSize::Compact)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Error)
                        .disabled(self.busy.is_some())
                        .tooltip(Tooltip::text("Delete branch"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_delete_branch(name.clone(), window, cx)
                        })),
                )
            })
            .when_some(age, |this, age| {
                this.end_slot(
                    Label::new(age)
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder),
                )
            })
            .when_some(subject, |this, subject| {
                this.tooltip(Tooltip::text(subject))
            })
            .on_click(
                cx.listener(move |this, _, window, cx| this.pick_branch(entry.clone(), window, cx)),
            )
            .into_any_element()
    }
}

/// Whether the characters of `needle` appear in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut characters = haystack.chars();
    needle
        .chars()
        .all(|wanted| characters.any(|character| character == wanted))
}
