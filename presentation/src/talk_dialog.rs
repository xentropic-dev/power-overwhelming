use crate::{
    keymap::{Command, Keymap, Scope},
    picker::{Kind, Picker},
};
use anyhow::{Context as _, Result};
use application::talk::LogFilter;
use domain::talk::{Message, MessageId, Recipient, Sender, envelope};
use gpui::{prelude::*, *};
use infrastructure::{client::talk, theme::Colors};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Agent { id: String, label: String },
    User,
}

impl Target {
    fn agent(&self) -> Option<&str> {
        match self {
            Self::Agent { id, .. } => Some(id),
            Self::User => None,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Query {
    id: u64,
    box_id: String,
    target: Target,
    expanded: Option<MessageId>,
}

impl Query {
    pub fn load(&self) -> Result<Vec<Message>> {
        let caller = caller(self.box_id.clone());
        if let Some(id) = &self.expanded {
            talk::show(caller, id.clone(), true)
        } else if self.target == Target::User {
            talk::desktop_inbox(self.box_id.clone(), now_ms())
        } else {
            let messages = talk::log(
                caller,
                LogFilter {
                    agent: self.target.agent().map(str::to_owned),
                    ..LogFilter::default()
                },
            )?;
            let mut threads = std::collections::HashSet::new();
            Ok(messages
                .into_iter()
                .filter(|message| threads.insert(message.thread.clone()))
                .collect())
        }
    }
}

pub struct Draft {
    pub picker: Picker,
    reply_to: Option<MessageId>,
}

pub struct Send {
    box_id: String,
    agent: Option<String>,
    reply_to: Option<MessageId>,
    body: String,
}

impl Send {
    pub fn execute(self) -> Result<()> {
        let caller = caller(self.box_id);
        if let Some(reply_to) = self.reply_to {
            talk::reply(caller, reply_to, self.body, self.agent)?;
        } else {
            let id = self.agent.context("Select an agent to message")?;
            talk::send(caller, Some(id), None, self.body, false)?;
        }
        Ok(())
    }
}

pub fn caller(box_id: String) -> talk::Caller {
    talk::Caller::desktop(box_id)
}

pub struct TalkDialog {
    id: u64,
    box_id: String,
    target: Target,
    focus: FocusHandle,
    pub draft: Option<Draft>,
    threads: Vec<Message>,
    expanded: Option<MessageId>,
    messages: Vec<Message>,
    selected_thread: usize,
    selected_message: usize,
    error: Option<String>,
    loading: bool,
    scroll: ScrollHandle,
}

impl TalkDialog {
    pub fn new(id: u64, box_id: String, target: Target, focus: FocusHandle) -> Self {
        Self {
            id,
            box_id,
            target,
            focus,
            draft: None,
            threads: Vec::new(),
            expanded: None,
            messages: Vec::new(),
            selected_thread: 0,
            selected_message: 0,
            error: None,
            loading: true,
            scroll: ScrollHandle::new(),
        }
    }

    pub fn query(&self) -> Query {
        Query {
            id: self.id,
            box_id: self.box_id.clone(),
            target: self.target.clone(),
            expanded: self.expanded.clone(),
        }
    }

    pub fn apply(&mut self, query: &Query, result: Result<Vec<Message>>) {
        if self.query() != *query {
            return;
        }
        self.loading = false;
        match result {
            Ok(rows) => {
                self.error = None;
                if self.expanded.is_some() {
                    let previous = self
                        .messages
                        .get(self.selected_message)
                        .map(|message| &message.id);
                    self.selected_message = previous
                        .and_then(|id| rows.iter().position(|message| &message.id == id))
                        .unwrap_or_else(|| rows.len().saturating_sub(1));
                    self.messages = rows;
                    self.scroll.scroll_to_item(self.selected_message);
                } else {
                    let previous = self
                        .threads
                        .get(self.selected_thread)
                        .map(|message| &message.thread);
                    self.selected_thread = previous
                        .and_then(|id| rows.iter().position(|message| &message.thread == id))
                        .unwrap_or_else(|| self.selected_thread.min(rows.len().saturating_sub(1)));
                    self.threads = rows;
                    self.scroll.scroll_to_item(self.selected_thread);
                }
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }

    pub fn focus(&self, window: &mut Window) {
        if let Some(draft) = &self.draft {
            draft.picker.focus.focus(window);
        } else {
            self.focus.focus(window);
        }
    }

    pub fn step(&mut self, up: bool) {
        let (rows, selected) = if self.expanded.is_some() {
            (&self.messages, &mut self.selected_message)
        } else {
            (&self.threads, &mut self.selected_thread)
        };
        *selected = if up {
            selected.saturating_sub(1)
        } else {
            (*selected + 1).min(rows.len().saturating_sub(1))
        };
        self.scroll.scroll_to_item(*selected);
    }

    pub fn expand(&mut self) -> bool {
        if self.expanded.is_some() {
            return false;
        }
        if let Some(message) = self.threads.get(self.selected_thread) {
            self.expanded = Some(message.id.clone());
            self.loading = true;
            self.error = None;
            self.scroll.set_offset(point(px(0.), px(0.)));
            return true;
        }
        false
    }

    /// Closes the top-level dialog if there is no draft or expanded thread.
    pub fn back(&mut self) -> bool {
        if self.draft.take().is_some() {
            return false;
        }
        if self.expanded.take().is_some() {
            self.messages.clear();
            self.selected_message = 0;
            self.loading = false;
            self.error = None;
            self.scroll.scroll_to_item(self.selected_thread);
            return false;
        }
        true
    }

    /// The open decision request under the selection in the thread list, if any.
    fn selected_decision(&self) -> Option<&Message> {
        if self.expanded.is_some() || self.draft.is_some() {
            return None;
        }
        self.threads
            .get(self.selected_thread)
            .filter(|message| answerable(message, now_ms()))
    }

    /// When the next open request in the list expires, so the list refreshes then.
    pub fn next_expiry(&self) -> Option<u64> {
        let now = now_ms();
        self.threads
            .iter()
            .filter(|message| answerable(message, now))
            .filter_map(|message| message.decision.as_ref()?.expires_at)
            .min()
    }

    /// Answers the selected decision request with its `index`th choice.
    pub fn answer(&mut self, index: usize) -> Option<Send> {
        let message = self.selected_decision()?;
        let choice = message.decision.as_ref()?.choices.get(index)?.clone();
        Some(Send {
            box_id: self.box_id.clone(),
            agent: None,
            reply_to: Some(message.id.clone()),
            body: choice,
        })
    }

    pub fn answered(&mut self, result: Result<()>) {
        self.error = result.err().map(|error| format!("{error:#}"));
    }

    pub fn compose(&mut self, reply: bool, id: u64, focus: FocusHandle) -> bool {
        if reply {
            let participant = |message: &&Message| {
                self.target
                    .agent()
                    .is_none_or(|id| message.involves_agent(id))
            };
            let message = if self.expanded.is_some() {
                self.messages
                    .get(self.selected_message)
                    .filter(participant)
                    .or_else(|| self.messages.iter().rev().find(participant))
            } else {
                self.threads.get(self.selected_thread)
            };
            let Some(message) = message else { return false };
            let picker = Picker::new(
                id,
                Kind::TalkDraft {
                    title: "Reply to thread".into(),
                },
                focus,
                Vec::new(),
            );
            self.draft = Some(Draft {
                picker,
                reply_to: Some(message.id.clone()),
            });
        } else if let Target::Agent { label, .. } = &self.target {
            let picker = Picker::new(
                id,
                Kind::TalkDraft {
                    title: format!("Message {label}"),
                },
                focus,
                Vec::new(),
            );
            self.draft = Some(Draft {
                picker,
                reply_to: None,
            });
        } else {
            return false;
        }
        true
    }

    pub fn prepare_send(&mut self) -> Option<(u64, Send)> {
        let draft = self.draft.as_mut()?;
        let body = draft.picker.text();
        if draft.picker.waiting || body.trim().is_empty() {
            return None;
        }
        let body = body.to_owned();
        draft.picker.waiting = true;
        draft.picker.message = Some("Sending…".into());
        Some((
            draft.picker.id,
            Send {
                box_id: self.box_id.clone(),
                agent: self.target.agent().map(str::to_owned),
                reply_to: draft.reply_to.clone(),
                body,
            },
        ))
    }

    pub fn sent(&mut self, id: u64, result: Result<()>) {
        let Some(draft) = &mut self.draft else { return };
        if draft.picker.id != id {
            return;
        }
        match result {
            Ok(()) => self.draft = None,
            Err(error) => {
                draft.picker.waiting = false;
                draft.picker.message = Some(format!("{error:#}"));
            }
        }
    }

    pub fn render(&self, colors: &'static Colors, keymap: &Keymap) -> AnyElement {
        if let Some(draft) = &self.draft {
            return draft.picker.render(colors, |_, _, _| {}).into_any_element();
        }
        let expanded = self.expanded.is_some();
        let (messages, selected) = if expanded {
            (&self.messages, self.selected_message)
        } else {
            (&self.threads, self.selected_thread)
        };
        let title = match &self.target {
            Target::Agent { label, .. } => format!("Talk with {label}"),
            Target::User => "Your inbox".into(),
        };
        let now = now_ms();
        let mut actions = Vec::new();
        let mut action = |scope, command, description| {
            if !keymap.keys(scope, command).is_empty() {
                let key = keymap.first(scope, command);
                actions.push(format!("{key} {description}"));
            }
        };
        if !expanded && !messages.is_empty() {
            action(Scope::Picker, Command::PickerSelect, "open thread");
        }
        if self.target.agent().is_some() {
            action(Scope::Talk, Command::TalkCompose, "compose");
        }
        if !messages.is_empty() {
            action(Scope::Talk, Command::TalkReply, "reply");
        }
        action(
            Scope::Picker,
            Command::PickerCancel,
            if expanded { "back" } else { "close" },
        );
        if let Some(decision) = self.selected_decision().and_then(|m| m.decision.as_ref()) {
            for (index, choice) in decision.choices.iter().enumerate().rev() {
                actions.insert(0, format!("{} {choice}", index + 1));
            }
        }
        let status = self.error.as_deref().or({
            if self.loading {
                Some("Loading messages…")
            } else if messages.is_empty() {
                Some("No messages.")
            } else {
                None
            }
        });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba((colors.background << 8) | 0x99))
            .flex()
            .justify_center()
            .items_center()
            .p_4()
            .child(
                div()
                    .id("talk-dialog")
                    .track_focus(&self.focus)
                    .w_full()
                    .max_w(px(720.))
                    .h(relative(0.76))
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .bg(rgb(colors.surface))
                    .border_1()
                    .border_color(rgb(colors.border))
                    .shadow_lg()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(rgb(colors.border))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when_some(status, |panel, text| {
                        panel.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_color(rgb(colors.muted))
                                .child(text.to_owned()),
                        )
                    })
                    .child(
                        div()
                            .id("talk-messages")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(messages.iter().enumerate().map(|(index, message)| {
                                let open = !expanded && answerable(message, now);
                                let section = (!expanded)
                                    .then(|| {
                                        let previous = index
                                            .checked_sub(1)
                                            .and_then(|i| messages.get(i))
                                            .is_some_and(|m| answerable(m, now));
                                        if open && index == 0 {
                                            Some("Open decisions")
                                        } else if !open && previous {
                                            Some("History")
                                        } else {
                                            None
                                        }
                                    })
                                    .flatten();
                                div()
                                    .px_3()
                                    .py_2()
                                    .when_some(section, |row, label| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(colors.muted))
                                                .child(label),
                                        )
                                    })
                                    .when(index == selected, |row| row.bg(rgb(colors.raised)))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(colors.muted))
                                            .child(metadata(message)),
                                    )
                                    .child(if expanded {
                                        div().text_sm().child(envelope(message))
                                    } else {
                                        div().truncate().child(
                                            message
                                                .body
                                                .lines()
                                                .next()
                                                .unwrap_or_default()
                                                .to_owned(),
                                        )
                                    })
                            })),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .border_t_1()
                            .border_color(rgb(colors.border))
                            .text_xs()
                            .text_color(rgb(colors.muted))
                            .child(actions.join(" · ")),
                    ),
            )
            .into_any_element()
    }
}

fn answerable(message: &Message, now: u64) -> bool {
    message.is_open_decision(now)
}

/// The choice a key answers: only an unmodified digit from 1, so no chord commits one.
pub fn choice_index(keystroke: &Keystroke) -> Option<usize> {
    if keystroke.modifiers.modified() {
        return None;
    }
    let digit = keystroke.key.parse::<usize>().ok()?;
    (1..=9).contains(&digit).then(|| digit - 1)
}

fn metadata(message: &Message) -> String {
    let sender = match &message.sender {
        Sender::Agent(agent) => &agent.id,
        Sender::User => "You",
        Sender::Shell => "Your shell",
        Sender::System => "Power",
    };
    let recipient = match &message.recipient {
        Recipient::Agent { id, .. } => id,
        Recipient::User => "You",
    };
    let state = if message.delivery.read_at.is_some() {
        "Read"
    } else if message.delivery.reminded_at.is_some() {
        "Reminded"
    } else if message.delivery.notified_at.is_some() {
        "Notified"
    } else if message.recipient == Recipient::User {
        "Unread"
    } else {
        "Queued"
    };
    let time = i64::try_from(message.created_at)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    let state = match &message.decision {
        Some(decision) if decision.answered_at.is_some() => "Decision answered",
        Some(decision) if !decision.is_open(now_ms()) => "Decision expired",
        Some(_) => "Decision open",
        None => state,
    };
    let from_box = match &message.sender {
        Sender::Agent(agent) => {
            let run = agent
                .run
                .as_deref()
                .map(|run| format!(" · run {run}"))
                .unwrap_or_default();
            format!(" · {} · box {}{run}", agent.role, agent.box_id)
        }
        _ => String::new(),
    };
    format!("{sender} → {recipient}{from_box} · {time} · {state}")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::{answerable, choice_index};
    use domain::talk::{
        Decision, Delivery, Message, MessageId, Recipient, Sender, ThreadId, open_decisions_first,
    };
    use gpui::Keystroke;

    fn message(id: &str, decision: Option<Decision>) -> Message {
        Message {
            id: MessageId(id.into()),
            box_id: "box".into(),
            sender: Sender::Shell,
            recipient: Recipient::User,
            body: id.into(),
            thread: ThreadId(id.into()),
            reply_to: None,
            broadcast: None,
            run: None,
            created_at: 0,
            delivery: Delivery::default(),
            urgent: false,
            decision,
        }
    }

    fn decision(expires_at: Option<u64>, answered_at: Option<u64>) -> Option<Decision> {
        Some(Decision {
            choices: vec!["Yes".into(), "No".into()],
            expires_at,
            answered_at,
        })
    }

    #[test]
    fn only_open_requests_lead_the_list_and_closed_ones_join_the_history() {
        let rows = vec![
            message("plain", None),
            message("expired", decision(Some(10), None)),
            message("answered", decision(None, Some(5))),
            message("open", decision(Some(100), None)),
        ];
        let ids = |rows: &[Message]| rows.iter().map(|m| m.id.0.clone()).collect::<Vec<_>>();
        let sorted = open_decisions_first(rows.clone(), 50);
        assert_eq!(ids(&sorted), ["open", "plain", "expired", "answered"]);
        assert!(answerable(&sorted[0], 50));
        assert!(!answerable(&sorted[2], 50));
        // At its deadline the last open request closes too.
        let later = open_decisions_first(rows, 100);
        assert_eq!(ids(&later), ["plain", "expired", "answered", "open"]);
        assert!(later.iter().all(|m| !answerable(m, 100)));
    }

    #[test]
    fn only_an_unmodified_digit_picks_a_choice() {
        let key = |text: &str| Keystroke::parse(text).unwrap();
        assert_eq!(choice_index(&key("1")), Some(0));
        assert_eq!(choice_index(&key("9")), Some(8));
        assert_eq!(choice_index(&key("0")), None);
        assert_eq!(choice_index(&key("a")), None);
        for chord in ["ctrl-1", "alt-2", "shift-3", "super-1", "ctrl-shift-1"] {
            assert_eq!(choice_index(&key(chord)), None, "{chord}");
        }
    }
}
