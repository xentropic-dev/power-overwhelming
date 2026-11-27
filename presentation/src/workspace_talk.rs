use super::Workspace;
use crate::{
    keymap::{Command, Scope},
    picker::{Choice, Kind},
    talk_dialog::{self, TalkDialog, Target},
};
use gpui::*;
use infrastructure::client::talk;
use std::{thread, time::Duration};

#[derive(Eq, PartialEq)]
struct SummaryScope {
    id: u64,
    box_id: String,
}

impl Workspace {
    pub(super) fn watch_talk(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> async_channel::Sender<()> {
        let (tx, rx) = async_channel::bounded(1);
        let wake = tx.clone();
        thread::spawn(move || {
            while !wake.is_closed() {
                let mut connected = false;
                let _ = talk::watch(talk::Caller::default(), |event| {
                    if !connected || event.is_some() {
                        let _ = wake.try_send(());
                        connected = true;
                    }
                    !wake.is_closed()
                });
                thread::sleep(Duration::from_secs(2));
            }
        });
        cx.spawn_in(window, async move |app, cx| {
            while rx.recv().await.is_ok() {
                let Ok((scope, query)) = app.update(cx, |workspace, _| {
                    (
                        workspace.summary_scope(),
                        workspace.talk.as_ref().map(TalkDialog::query),
                    )
                }) else {
                    break;
                };
                let update = cx
                    .background_executor()
                    .spawn(async move {
                        let summary = scope.map(|scope| {
                            let result = talk::summary(talk_dialog::caller(scope.box_id.clone()));
                            (scope, result)
                        });
                        let messages = query.map(|query| {
                            let result = query.load();
                            (query, result)
                        });
                        (summary, messages)
                    })
                    .await;
                if app
                    .update(cx, |workspace, cx| {
                        if let Some((scope, Ok(agents))) = update.0
                            && workspace.summary_scope().as_ref() == Some(&scope)
                        {
                            let agents = agents
                                .into_iter()
                                .map(|agent| (agent.agent.clone(), agent))
                                .collect();
                            if let Some(dag) = &mut workspace.dag {
                                dag.talk = agents;
                            } else if let Some(picker) = &mut workspace.picker {
                                picker.agent_talk = agents;
                            }
                        }
                        if let Some((query, result)) = update.1
                            && let Some(dialog) = &mut workspace.talk
                        {
                            dialog.apply(&query, result);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        tx
    }

    fn summary_scope(&self) -> Option<SummaryScope> {
        if let Some(dag) = &self.dag {
            return Some(SummaryScope {
                id: dag.id,
                box_id: dag.run.box_id.clone(),
            });
        }
        let picker = self.picker.as_ref()?;
        if let Kind::Agents { box_id, .. } = &picker.kind {
            Some(SummaryScope {
                id: picker.id,
                box_id: box_id.clone(),
            })
        } else {
            None
        }
    }

    pub(super) fn open_talk(
        &mut self,
        box_id: String,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        focus.focus(window);
        self.talk = Some(TalkDialog::new(self.next_id(), box_id, target, focus));
        let _ = self.talk_refresh.try_send(());
        cx.notify();
    }

    pub(super) fn open_agent_talk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else { return };
        if let Kind::Agents { box_id, tree } = &picker.kind
            && let Some(Choice::Agent(id)) = picker.selection()
            && let Some(agent) = tree.agent(&id)
        {
            self.open_talk(
                box_id.clone(),
                Target::Agent {
                    id,
                    label: agent.label.clone(),
                },
                window,
                cx,
            );
        }
    }

    pub(super) fn talk_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(dialog) = &mut self.talk else { return };
        let scope = if dialog.draft.is_some() {
            Scope::Picker
        } else {
            Scope::Talk
        };
        let command = self.keymap.command(scope, &event.keystroke);
        if command == Some(Command::PickerCancel) {
            if dialog.back() {
                self.talk = None;
                self.restore_focus(window, cx);
            } else {
                dialog.focus(window);
                let _ = self.talk_refresh.try_send(());
            }
        } else if let Some(draft) = &mut dialog.draft {
            if command == Some(Command::PickerSelect) {
                if !event.is_held {
                    self.send_talk(window, cx);
                }
            } else {
                draft.picker.edit(&event.keystroke, cx);
            }
        } else {
            match command {
                Some(Command::PickerUp | Command::DialogUp) => dialog.step(true),
                Some(Command::PickerDown | Command::DialogDown) => dialog.step(false),
                Some(Command::PickerSelect) if !event.is_held => {
                    if dialog.expand() {
                        let _ = self.talk_refresh.try_send(());
                    }
                }
                Some(command @ (Command::TalkCompose | Command::TalkReply)) if !event.is_held => {
                    let focus = cx.focus_handle();
                    let id = self.next_id();
                    if let Some(dialog) = &mut self.talk
                        && dialog.compose(command == Command::TalkReply, id, focus)
                    {
                        dialog.focus(window);
                    }
                }
                _ => {}
            }
        }
        cx.notify();
    }

    fn send_talk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, send)) = self.talk.as_mut().and_then(TalkDialog::prepare_send) else {
            return;
        };
        let sent = cx
            .background_executor()
            .spawn(async move { send.execute() });
        cx.spawn_in(window, async move |app, cx| {
            let result = sent.await;
            let _ = app.update_in(cx, |workspace, window, cx| {
                if let Some(dialog) = &mut workspace.talk {
                    dialog.sent(id, result);
                    dialog.focus(window);
                    let _ = workspace.talk_refresh.try_send(());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
