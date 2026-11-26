use crate::{
    box_icons::IconFiles,
    boxes::BoxesFile,
    dag_store::SqliteDagStore,
    terminal::PtyRuntime,
    wire::{self, DagRequest, DagResponse, Response, WorktreePruneEntry, WorktreeScope},
};
use anyhow::{Context, Result};
use application::dag::{
    BoxRegistry, BoxSelection, DagApplication, Dispatch, DispatchOutcome, EventScope, StartAgent,
};
use std::{os::unix::net::UnixStream, time::Duration};

pub(crate) type DaemonDag = DagApplication<PtyRuntime, SqliteDagStore, BoxesFile, IconFiles>;

pub(crate) fn request(application: &DaemonDag, request: DagRequest) -> Result<DagResponse> {
    use DagResponse as Reply;
    let response = match request {
        DagRequest::ListBoxes => Reply::Boxes {
            boxes: convert(application.boxes()?),
        },
        DagRequest::SetBoxIcon {
            box_sel,
            caller,
            path,
        } => Reply::BoxIcon {
            box_id: application.set_box_icon(
                BoxSelection {
                    selector: box_sel,
                    caller,
                },
                path,
            )?,
        },
        DagRequest::ListAgents {
            box_sel,
            caller,
            include_finished,
        } => Reply::Agents {
            agents: convert(application.list_agents(
                BoxSelection {
                    selector: box_sel,
                    caller,
                },
                include_finished,
            )?),
        },
        DagRequest::ListRuns {
            box_sel,
            caller,
            include_finished,
        } => Reply::Runs {
            runs: convert(application.list_runs(
                BoxSelection {
                    selector: box_sel,
                    caller,
                },
                include_finished,
            )?),
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
        DagRequest::ArchiveRun { run, force } => Reply::Run {
            run: application.archive_run(&run, force)?.into(),
        },
        DagRequest::UnarchiveRun { run } => Reply::Run {
            run: application.unarchive_run(&run)?.into(),
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
            allow_main_checkout,
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
                    worktree: worktree.into_domain(allow_main_checkout),
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
            allow_main_checkout,
            retry,
            args,
            caller,
            budget,
        } => match application.dispatch(Dispatch {
            run: run.clone(),
            task: task.clone(),
            provider: provider.into(),
            worktree: worktree.into_domain(allow_main_checkout),
            retry,
            args,
            caller,
            budget: budget.into(),
        })? {
            DispatchOutcome::Started(result) => Reply::Dispatched {
                attempt: result.attempt.into(),
                agent: Box::new(result.agent.into()),
                branch: result.branch,
            },
            DispatchOutcome::Queued { reason } => Reply::Queued { run, task, reason },
        },
        DagRequest::StartQueued { run, task } => {
            let result = application.start_queued_now(&run, &task)?;
            Reply::Dispatched {
                attempt: result.attempt.into(),
                agent: Box::new(result.agent.into()),
                branch: result.branch,
            }
        }
        DagRequest::CreateViewWorktree {
            box_sel,
            from_session,
            branch,
        } => Reply::ViewWorktreeCreated {
            path: application.create_view_worktree(&box_sel, &from_session, &branch)?,
        },
        DagRequest::ListManagedWorktrees { scope } => {
            let box_id = worktree_box(application, scope)?;
            Reply::ManagedWorktrees {
                worktrees: convert(application.list_managed_worktrees(box_id.as_deref())?),
            }
        }
        DagRequest::PruneWorktrees {
            scope,
            dry_run,
            delete_branch,
        } => {
            let box_id = worktree_box(application, scope)?;
            Reply::WorktreesPruned {
                entries: application
                    .prune(box_id.as_deref(), dry_run, delete_branch)?
                    .into_iter()
                    .map(|(worktree, verdict)| WorktreePruneEntry {
                        worktree: worktree.into(),
                        verdict: verdict.into(),
                    })
                    .collect(),
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

fn worktree_box(application: &DaemonDag, scope: WorktreeScope) -> Result<Option<String>> {
    match scope {
        WorktreeScope::All => Ok(None),
        WorktreeScope::Box { selector } => application.select_box(&selector).map(Some),
        WorktreeScope::View { session } => BoxesFile
            .boxes()?
            .into_iter()
            .find(|entry| entry.sessions.contains(&session))
            .map(|entry| Some(entry.info.id))
            .with_context(|| format!("Power session {session:?} does not belong to a box")),
    }
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
                    dag: Box::new(DagResponse::Event {
                        event: event.into(),
                    }),
                },
            )?;
        }
    }
}

fn convert<T: Into<U>, U>(values: Vec<T>) -> Vec<U> {
    values.into_iter().map(Into::into).collect()
}
