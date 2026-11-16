//! The commit box of the Changes panel: the commit matrix as one `CommitPlan`,
//! the Commit and Generate actions, and the footer they render in.

use gpui::{Focusable, MouseButton};

use super::*;
use crate::git::CommitMode;

/// The message box's hint when no message is suggested.
pub(super) const COMMIT_PLACEHOLDER: &str = "Commit message";

/// What a commit would do right now, from the commit matrix. The single place the
/// git command, the Commit button and the Generate input are decided.
struct CommitPlan {
    mode: CommitMode,
    /// The mode has something to take, which is also what Generate describes.
    has_input: bool,
    conflicted: bool,
    /// Used when the message box is left empty (Zed's "Update a.rs").
    suggested_message: Option<String>,
}

impl CommitPlan {
    fn new(status: &RepoStatus, amend: bool) -> Self {
        // An unborn branch has nothing to amend; its checkbox is disabled.
        let mode = status.commit_mode(amend && !status.is_unborn());
        let has_input = match mode {
            CommitMode::Staged => status.has_staged(),
            CommitMode::AllTracked => status.has_unstaged_tracked(),
            // A message-only amend is a valid commit too.
            CommitMode::Amend => true,
        };
        Self {
            mode,
            has_input,
            conflicted: status.has_conflicts(),
            suggested_message: status.suggested_commit_message(mode),
        }
    }

    fn label(&self) -> &'static str {
        match self.mode {
            CommitMode::Staged => "Commit",
            CommitMode::AllTracked => "Commit Tracked",
            CommitMode::Amend => "Amend",
        }
    }

    fn description(&self) -> &'static str {
        match self.mode {
            CommitMode::Staged => "Commit the staged changes",
            CommitMode::AllTracked => {
                "Nothing is staged: commit every change to tracked files (untracked files stay out)"
            }
            CommitMode::Amend => {
                "Replace the last commit with the staged changes; an empty message keeps its message"
            }
        }
    }
}

impl DiffPanel {
    /// Why Commit cannot run now, or `None` when it can.
    fn commit_blocker(&self, plan: &CommitPlan, message_empty: bool) -> Option<&'static str> {
        if let Some(action) = self.busy {
            Some(action.progress())
        } else if plan.conflicted {
            Some("Resolve the conflicts before committing")
        } else if !plan.has_input {
            Some(
                "Nothing to commit: stage files or change tracked ones \
                 (untracked files are never committed automatically)",
            )
        } else if message_empty
            && plan.mode != CommitMode::Amend
            && plan.suggested_message.is_none()
        {
            Some("Write a commit message first")
        } else {
            None
        }
    }

    /// Why Generate cannot run now, or `None` when it can.
    fn generate_blocker(&self, plan: &CommitPlan) -> Option<&'static str> {
        if let Some(action) = self.busy {
            return Some(action.progress());
        }
        if self.installation.is_none() {
            return Some("pi was not found, so it cannot write a commit message");
        }
        match self.print_mode {
            None => Some("Checking whether this pi can write commit messages…"),
            Some(false) => Some("This pi has no print mode (-p); update pi to generate messages"),
            Some(true) if !plan.has_input => Some(
                "Nothing to describe: stage files or change tracked ones \
                 (untracked files are never included)",
            ),
            Some(true) => None,
        }
    }

    /// Commits per the commit matrix: from the Commit button and from ⌘Enter.
    fn commit(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.status() else {
            return;
        };
        let plan = CommitPlan::new(status, self.amend);
        let mut message = self.commit_input.read(cx).text().to_string();
        if let Some(reason) = self.commit_blocker(&plan, message.trim().is_empty()) {
            // ⌘Enter has no disabled state to look at, so say why.
            if self.busy.is_none() {
                self.action_error = Some(reason.to_string());
                cx.notify();
            }
            return;
        }
        if message.trim().is_empty()
            && let Some(suggested) = plan.suggested_message
        {
            message = suggested;
        }
        let mode = plan.mode;
        self.run_git(
            GitAction::Commit,
            move |root| {
                git::commit(root, mode, &message)
                    .map(|_| ())
                    .map_err(GitError::message)
            },
            cx,
        );
    }

    /// Asks pi for a message describing what Commit would take; the answer
    /// replaces the box (the user clicked for it) and the box gets focus.
    fn generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(workspace), State::Loaded(changes, status)) =
            (self.workspace.clone(), &self.state)
        else {
            return;
        };
        let plan = CommitPlan::new(status, self.amend);
        if self.generate_blocker(&plan).is_some() {
            return;
        }
        let Some(installation) = self.installation.clone() else {
            return;
        };
        let root = changes.root.clone();
        let mode = plan.mode;
        let settings = Settings::get(cx);
        let instructions = settings.commit_message_instructions.to_string();
        let model = settings
            .commit_message_model
            .as_ref()
            .map(|model| model.to_string());
        self.busy = Some(GitAction::Generate);
        self.action_error = None;
        let work = cx.background_spawn(async move {
            let diff = git::generation_diff(&root, mode).map_err(GitError::message)?;
            if diff.trim().is_empty() {
                return Err("there are no changes to describe".to_string());
            }
            pi::generate_commit_message(
                &installation,
                &root,
                &instructions,
                &diff,
                model.as_deref(),
            )
        });
        self._action_task = cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            this.update_in(cx, |this, window, cx| {
                if this.workspace.as_ref() != Some(&workspace) {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(message) => {
                        this.commit_input
                            .update(cx, |input, cx| input.set_text(message, cx));
                        window.focus(&this.commit_input.read(cx).focus_handle(cx));
                    }
                    Err(error) => this.show_error(GitAction::Generate, error),
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// A git or pi failure, until it is dismissed or the next action starts.
    fn render_action_error(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        h_flex()
            .items_start()
            .gap(px(6.))
            .pl(px(8.))
            .pr(px(2.))
            .py(px(2.))
            .rounded(px(6.))
            .bg(colors.error.opacity(0.1))
            .border_1()
            .border_color(colors.error.opacity(0.4))
            .child(
                div().pt(px(5.)).child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                ),
            )
            .child(
                div()
                    .id("git-error")
                    .flex_1()
                    .min_w(px(0.))
                    .max_h(px(120.))
                    .overflow_y_scroll()
                    .py(px(3.))
                    .child(
                        Label::new(error.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
            )
            .child(
                IconButton::new("dismiss-git-error", IconName::Close)
                    .size(ButtonSize::Compact)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Dismiss"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.action_error = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The message box, with Amend, Generate and Commit under it.
    pub(super) fn render_commit_footer(
        &self,
        status: &RepoStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let plan = CommitPlan::new(status, self.amend);
        let placeholder = plan
            .suggested_message
            .clone()
            .unwrap_or_else(|| COMMIT_PLACEHOLDER.to_string());
        if *self.commit_input.read(cx).placeholder() != placeholder {
            self.commit_input
                .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        let message_empty = self.commit_input.read(cx).text().trim().is_empty();
        let commit_blocker = self.commit_blocker(&plan, message_empty);
        let generate_blocker = self.generate_blocker(&plan);
        let unborn = status.is_unborn();
        let error = self
            .action_error
            .as_deref()
            .map(|error| self.render_action_error(error, cx));

        let message_box = div()
            .id("commit-message")
            .key_context(COMMIT_CONTEXT)
            // Enter is bound to Submit inside the field itself and cannot be
            // rebound from out here, so a bubbled Submit becomes a newline.
            .on_action(cx.listener(|this, _: &crate::chat::Submit, _, cx| {
                this.commit_input
                    .update(cx, |input, cx| input.insert_text("\n", false, false, cx));
            }))
            .on_action(cx.listener(|this, _: &CommitNow, _, cx| this.commit(cx)))
            // The padding around the field focuses it too.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.commit_input.read(cx).focus_handle(cx));
                }),
            )
            .px(px(8.))
            .py(px(6.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .child(self.commit_input.clone());

        let amend = h_flex()
            .gap(px(2.))
            .child(
                Checkbox::new(
                    "amend",
                    if self.amend && !unborn {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .disabled(unborn || self.busy.is_some())
                .tooltip(Tooltip::text(if unborn {
                    "There is no commit to amend yet"
                } else {
                    "Replace the last commit instead of adding a new one"
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.amend = !this.amend;
                    cx.notify();
                })),
            )
            .child(Label::new("Amend").size(LabelSize::Small).color(if unborn {
                Color::Disabled
            } else {
                Color::Muted
            }));

        let generating = self.busy == Some(GitAction::Generate);
        let generate = IconButton::new("generate-commit-message", IconName::Sparkle)
            .size(ButtonSize::Compact)
            .icon_size(IconSize::XSmall)
            .loading(generating)
            .disabled(generate_blocker.is_some() && !generating)
            .tooltip(Tooltip::text(generate_blocker.unwrap_or(
                "Let pi write the message from the changes this commit takes",
            )))
            .on_click(cx.listener(|this, _, window, cx| this.generate(window, cx)));

        let committing = self.busy == Some(GitAction::Commit);
        let commit_tooltip = match commit_blocker {
            Some(reason) => Tooltip::with_optional_keys(reason, None),
            None => Tooltip::with_optional_keys(
                plan.description(),
                Some(crate::keys::label("⌘↩").into()),
            ),
        };
        let commit = Button::new("commit", plan.label())
            .style(ButtonStyle::Filled)
            .size(ButtonSize::Compact)
            .loading(committing)
            .disabled(commit_blocker.is_some() && !committing)
            .tooltip(commit_tooltip)
            .on_click(cx.listener(|this, _, _, cx| this.commit(cx)));

        v_flex()
            .flex_none()
            .gap(px(6.))
            .p(px(8.))
            .border_t_1()
            .border_color(colors.border_variant)
            .children(error)
            .child(message_box)
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(amend)
                    .child(div().flex_1())
                    .child(generate)
                    .child(commit),
            )
            .into_any_element()
    }
}
