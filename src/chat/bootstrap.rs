//! The memory-bootstrap extension's `memory_init` card above the user
//! message whose run triggered it, as pi's TUI shows it.
//!
//! The extension keeps its state in custom session entries, which RPC sends
//! only through `get_entries`: a status entry `{ run }` when the call starts
//! and a result entry `{ run, text }` when it succeeds. It signals both
//! moments with `setStatus("memory-bootstrap-status")`, so each signal
//! refetches the entries. A run whose second signal finds no result failed.

use super::*;

const STATUS_TYPE: &str = "memory-bootstrap-status";
const ENTRY_TYPE: &str = "memory-bootstrap";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Phase {
    Running,
    Done,
    Failed,
}

/// The card shown above a user message.
pub(super) struct Bootstrap {
    run: String,
    phase: Phase,
    /// What was injected into the system prompt; empty until done.
    text: String,
    /// The user's choice; `None` follows the global default.
    pub(super) expanded: Option<bool>,
}

/// One bootstrap run on the current branch.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Found {
    run: String,
    text: Option<String>,
    /// The timestamp of the user message the run came before, once pi has
    /// stored that message.
    user_at: Option<i64>,
}

impl ChatView {
    /// `setStatus` from the extension: a run started or ended.
    pub(super) fn bootstrap_signaled(&mut self, record: &Value) -> bool {
        if record["statusKey"] != STATUS_TYPE {
            return false;
        }
        self.bootstrap_signal = true;
        self.send_all([json!({ "type": "get_entries" })]);
        true
    }

    /// A `get_entries` answer, for the signal or for a transcript load.
    pub(super) fn bootstrap_entries(&mut self, data: &Value, cx: &mut Context<Self>) {
        let live = std::mem::take(&mut self.bootstrap_signal);
        self.bootstraps = find(data);
        self.attach_bootstraps(live, cx);
    }

    /// Puts the known runs on their user rows; `live` when a signal asked.
    pub(super) fn attach_bootstraps(&mut self, live: bool, cx: &mut Context<Self>) {
        let found = std::mem::take(&mut self.bootstraps);
        for run in &found {
            let known = self.bootstrap_live.get(&run.run).copied();
            let phase = match (&run.text, known) {
                (Some(_), _) => Phase::Done,
                // The run's second signal and still no result.
                (None, Some(Phase::Running)) if live => Phase::Failed,
                (None, Some(phase)) => phase,
                (None, None) if live => Phase::Running,
                // Failed or cut off in an earlier process: the TUI shows nothing either.
                (None, None) => continue,
            };
            if live || known.is_some() {
                self.bootstrap_live.insert(run.run.clone(), phase);
            }
            let Some(index) = self.bootstrap_row(run, live) else {
                continue;
            };
            let Some(Row::User { bootstrap, .. }) = self.rows.get_mut(index) else {
                continue;
            };
            let expanded = bootstrap.as_ref().and_then(|card| card.expanded);
            *bootstrap = Some(Bootstrap {
                run: run.run.clone(),
                phase,
                text: run.text.clone().unwrap_or_default(),
                expanded,
            });
            if index < self.list_state.item_count() {
                self.list_state.splice(index..index + 1, 1);
            }
        }
        self.bootstraps = found;
        cx.notify();
    }

    /// The user row a run belongs to: where it already shows, the stored
    /// message it preceded, or, while it runs, the message just sent.
    fn bootstrap_row(&self, run: &Found, live: bool) -> Option<usize> {
        let user_rows = || {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, row)| matches!(row, Row::User { .. }))
        };
        user_rows()
            .find(|(_, row)| {
                matches!(row, Row::User { bootstrap: Some(card), .. } if card.run == run.run)
            })
            .or_else(|| {
                let at = run.user_at?;
                user_rows().find(|(_, row)| matches!(row, Row::User { at: Some(sent), .. } if *sent == at))
            })
            .or_else(|| {
                (live && run.user_at.is_none())
                    .then(|| user_rows().last())
                    .flatten()
            })
            .map(|(index, _)| index)
    }

    pub(super) fn toggle_bootstrap(&mut self, row: usize, cx: &mut Context<Self>) {
        let default = details_expanded(cx);
        if let Some(Row::User {
            bootstrap: Some(card),
            ..
        }) = self.rows.get_mut(row)
        {
            card.expanded = Some(!card.expanded.unwrap_or(default));
            self.list_state.splice(row..row + 1, 1);
            cx.notify();
        }
    }

    /// A tool-style card: status, size, and the injected text on demand.
    pub(super) fn render_bootstrap(
        &self,
        index: usize,
        card: &Bootstrap,
        default_open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (code_size, code_line) = crate::settings::Settings::get(cx).dense_code_size();
        let has_text = !card.text.is_empty();
        let open = card.expanded.unwrap_or(default_open) && has_text;
        let (status_icon, message) = match card.phase {
            Phase::Running => (
                Spinner::new()
                    .size(IconSize::XSmall)
                    .color(Color::Accent)
                    .into_any_element(),
                "Memory bootstrap in progress…".to_string(),
            ),
            Phase::Done => (
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Success)
                    .into_any_element(),
                format!(
                    "Memory bootstrap finished ({:.1} KB)",
                    card.text.len() as f64 / 1024.
                ),
            ),
            Phase::Failed => (
                Icon::new(IconName::Close)
                    .size(IconSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
                "Memory bootstrap failed".to_string(),
            ),
        };
        v_flex()
            .border_1()
            .border_color(colors.border_variant)
            .overflow_hidden()
            .child(
                h_flex()
                    .gap(px(6.))
                    .h(ui_px(cx, 28.))
                    .pl(px(8.))
                    .pr(px(4.))
                    .bg(colors.panel_background)
                    .child(status_icon)
                    .child(
                        Icon::new(IconName::Tool)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("memory_init")
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM),
                    )
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .when(has_text, |this| {
                        this.child(disclosure_button(
                            ("bootstrap-disclosure", index),
                            open,
                            "memory context",
                            cx.listener(move |this, _, _, cx| this.toggle_bootstrap(index, cx)),
                        ))
                    }),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .child(
                            SelectableText::new(
                                SharedString::from(format!("{index}:bootstrap")),
                                SharedString::from(card.text.clone()),
                                &self.selection,
                            )
                            .text_size(px(code_size), px(code_line))
                            .color(Color::Muted)
                            .monospace(),
                        ),
                )
            })
            .into_any_element()
    }
}

/// Bootstrap runs on the current branch of a `get_entries` answer, in order.
pub(super) fn find(data: &Value) -> Vec<Found> {
    let entries = data["entries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| Some((entry["id"].as_str()?, entry)))
        .collect();
    let mut branch = Vec::new();
    let mut next = data["leafId"].as_str();
    while let Some(id) = next {
        let Some(entry) = by_id.get(id) else {
            break;
        };
        branch.push(*entry);
        next = entry["parentId"].as_str();
        // A malformed tree must not loop forever.
        if branch.len() > entries.len() {
            break;
        }
    }
    branch.reverse();

    let mut found: Vec<Found> = Vec::new();
    let mut awaiting_user = Vec::new();
    for entry in branch {
        match entry["type"].as_str() {
            Some("custom") => {
                let Some(run) = entry["data"]["run"].as_str() else {
                    continue;
                };
                match entry["customType"].as_str() {
                    Some(STATUS_TYPE) => {
                        awaiting_user.push(found.len());
                        found.push(Found {
                            run: run.to_string(),
                            text: None,
                            user_at: None,
                        });
                    }
                    Some(ENTRY_TYPE) => {
                        if let Some(item) = found.iter_mut().find(|item| item.run == run) {
                            item.text = entry["data"]["text"].as_str().map(str::to_string);
                        }
                    }
                    _ => {}
                }
            }
            Some("message") if entry["message"]["role"] == "user" => {
                for index in awaiting_user.drain(..) {
                    found[index].user_at = entry["message"]["timestamp"].as_i64();
                }
            }
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_come_from_the_current_branch_with_their_user_message() {
        let data = json!({
            "leafId": "u1",
            "entries": [
                { "id": "s1", "parentId": null, "type": "custom",
                  "customType": STATUS_TYPE, "data": { "run": "a" } },
                { "id": "r1", "parentId": "s1", "type": "custom",
                  "customType": ENTRY_TYPE, "data": { "run": "a", "text": "ctx" } },
                { "id": "u1", "parentId": "r1", "type": "message",
                  "message": { "role": "user", "timestamp": 42 } },
                // Another branch.
                { "id": "s2", "parentId": null, "type": "custom",
                  "customType": STATUS_TYPE, "data": { "run": "b" } },
            ],
        });
        assert_eq!(
            find(&data),
            vec![Found {
                run: "a".to_string(),
                text: Some("ctx".to_string()),
                user_at: Some(42),
            }]
        );
    }

    #[test]
    fn a_run_in_progress_has_no_result_or_message_yet() {
        let data = json!({
            "leafId": "s1",
            "entries": [
                { "id": "s1", "parentId": null, "type": "custom",
                  "customType": STATUS_TYPE, "data": { "run": "a" } },
            ],
        });
        assert_eq!(
            find(&data),
            vec![Found {
                run: "a".to_string(),
                text: None,
                user_at: None,
            }]
        );
    }
}
