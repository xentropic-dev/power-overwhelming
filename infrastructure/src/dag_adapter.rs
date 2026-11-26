use crate::{
    boxes::BoxesFile,
    dag_store::SqliteDagStore,
    terminal::PtyRuntime,
    wire::{self, DagRequest, DagResponse, Response},
};
use anyhow::Result;
use application::dag::{BoxSelection, DagApplication, Dispatch, EventScope, StartAgent};
use domain::worktree::WorktreeRequest;
use std::{os::unix::net::UnixStream, time::Duration};

pub(crate) type DaemonDag = DagApplication<PtyRuntime, SqliteDagStore, BoxesFile>;

pub(crate) fn request(application: &DaemonDag, request: DagRequest) -> Result<DagResponse> {
    use DagResponse as Reply;
    let response = match request {
        DagRequest::ListBoxes => Reply::Boxes {
            boxes: convert(application.boxes()?),
        },
        DagRequest::ListAgents { box_sel, caller } => Reply::Agents {
            agents: convert(application.list_agents(BoxSelection {
                selector: box_sel,
                caller,
            })?),
        },
        DagRequest::ListRuns { box_sel, caller } => Reply::Runs {
            runs: convert(application.list_runs(BoxSelection {
                selector: box_sel,
                caller,
            })?),
        },
        DagRequest::CreateRun {
            box_sel,
            caller,
            title,
            coordinator,
        } => Reply::Run {
            run: application
                .create_run(
                    BoxSelection {
                        selector: box_sel,
                        caller,
                    },
                    title,
                    coordinator,
                )?
                .into(),
        },
        DagRequest::GetRun { run } => Reply::Run {
            run: application.run(&run)?.into(),
        },
        DagRequest::AddTasks { run, tasks } => Reply::Run {
            run: application.add_tasks(&run, &convert(tasks))?.into(),
        },
        DagRequest::AddDependency { run, task, dep } => Reply::Run {
            run: application.add_dependency(&run, &task, &dep)?.into(),
        },
        DagRequest::StartAgent {
            box_sel,
            caller,
            provider,
            worktree,
            parent,
            label,
            prompt,
            args,
        } => Reply::Agent {
            agent: application
                .start_agent(StartAgent {
                    selection: BoxSelection {
                        selector: box_sel,
                        caller,
                    },
                    provider: provider.into(),
                    worktree: WorktreeRequest::Existing(worktree),
                    parent,
                    label,
                    prompt,
                    args,
                })?
                .into(),
        },
        DagRequest::Dispatch {
            run,
            task,
            provider,
            worktree,
            retry,
            args,
            caller,
        } => {
            let result = application.dispatch(Dispatch {
                run,
                task,
                provider: provider.into(),
                worktree: WorktreeRequest::Existing(worktree),
                retry,
                args,
                caller,
            })?;
            Reply::Dispatched {
                attempt: result.attempt.into(),
                agent: result.agent.into(),
            }
        }
        DagRequest::Report {
            run,
            task,
            attempt,
            status,
            summary,
        } => {
            let result = application.report(&run, &task, &attempt, status.into(), &summary)?;
            Reply::Reported {
                applied: result.applied,
                reason: result.reason,
            }
        }
        DagRequest::Events {
            run,
            since,
            wait_ms,
            kinds,
        } => {
            let kinds = convert(kinds);
            let page = application.events(
                EventScope::Run(&run, &kinds),
                since,
                Duration::from_millis(wait_ms),
            )?;
            Reply::Events {
                cursor: page.cursor,
                events: convert(page.events),
            }
        }
        DagRequest::Attention => Reply::Attention {
            boxes: convert(application.attention()?),
        },
        DagRequest::Watch { .. } => unreachable!("DAG watches use the streaming handler"),
    };
    Ok(response)
}

pub(crate) fn watch(
    application: &DaemonDag,
    socket: &mut UnixStream,
    box_sel: Option<&str>,
) -> Result<()> {
    let box_id = box_sel
        .map(|selector| application.select_box(selector))
        .transpose()?;
    let scope = EventScope::Watch(box_id.as_deref());
    let mut cursor = application.latest_seq()?;
    loop {
        let page = application.events(scope, cursor, Duration::from_secs(5))?;
        cursor = page.cursor;
        if page.events.is_empty() {
            wire::send(&mut *socket, &Response::Ok)?;
        }
        for event in page.events {
            wire::send(
                &mut *socket,
                &Response::Dag {
                    dag: DagResponse::Event {
                        event: event.into(),
                    },
                },
            )?;
        }
    }
}

fn convert<T: Into<U>, U>(values: Vec<T>) -> Vec<U> {
    values.into_iter().map(Into::into).collect()
}
