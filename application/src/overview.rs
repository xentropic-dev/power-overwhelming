//! Composes the dashboard overview from runs, agents, sessions, boxes, usage limits and
//! the machine's load.

use crate::{
    TerminalRuntime,
    dag::{BoxIcons, BoxRegistry, BoxSelection, DagApplication, DagStore},
};
use anyhow::Result;
use domain::{
    dag::{AgentStatus, ManagedAgent, Provider, RunStatus},
    overview::{
        BoxOverview, ModelInUse, ModelUsage, Overview, RunOverview, SystemStats, Totals, estimate,
    },
    session::Agent,
};
use std::collections::{BTreeSet, HashSet};

/// The machine's CPU and memory load.
pub trait SystemProbe: Send + Sync {
    fn stats(&self) -> SystemStats;
}

/// Rate-limit usage for the models in use. Returns one entry per requested model, with no
/// windows when the provider has reported none.
pub trait UsageSource: Send + Sync {
    fn usage(&self, models: &[ModelInUse]) -> Vec<ModelUsage>;
}

pub struct OverviewQuery<'a> {
    pub system: &'a dyn SystemProbe,
    pub usage: &'a dyn UsageSource,
}

impl OverviewQuery<'_> {
    /// Takes the overview at Unix second `now`.
    pub fn execute<T: TerminalRuntime, S: DagStore, B: BoxRegistry, I: BoxIcons>(
        &self,
        dag: &DagApplication<T, S, B, I>,
        now: u64,
    ) -> Result<Overview> {
        let everywhere = || BoxSelection {
            selector: None,
            caller: None,
        };
        let runs = dag.list_runs(everywhere(), false)?;
        let agents: Vec<ManagedAgent> = dag
            .list_agents(everywhere(), false)?
            .into_iter()
            .filter(|agent| !agent.status.is_finished())
            .collect();
        let sessions = dag.list_sessions();
        let boxes = dag.registered_boxes()?;
        let live: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
        let attached: HashSet<&str> = boxes
            .iter()
            .flat_map(|entry| &entry.sessions)
            .map(String::as_str)
            .filter(|session| live.contains(session))
            .collect();
        let working = |agent: &&ManagedAgent| agent.status == AgentStatus::Working;

        let box_overviews = boxes
            .iter()
            .map(|entry| {
                let id = entry.info.id.as_str();
                let in_view: HashSet<&str> = entry
                    .sessions
                    .iter()
                    .map(String::as_str)
                    .filter(|session| live.contains(session))
                    .collect();
                let detached = agents
                    .iter()
                    .filter(|agent| agent.box_id == id)
                    .filter_map(|agent| agent.session.as_deref())
                    .filter(|session| live.contains(session) && !attached.contains(session));
                let terminals: HashSet<&str> = in_view.iter().copied().chain(detached).collect();
                BoxOverview {
                    id: id.to_owned(),
                    name: entry.info.name.clone(),
                    terminals: terminals.len(),
                    attached: in_view.len(),
                    agents_working: agents
                        .iter()
                        .filter(working)
                        .filter(|agent| agent.box_id == id)
                        .count(),
                    runs_active: runs
                        .iter()
                        .filter(|run| run.box_id == id && run.status() == RunStatus::Active)
                        .count(),
                }
            })
            .collect();

        let run_overviews = runs
            .iter()
            .map(|run| RunOverview {
                run: run.id.clone(),
                box_id: run.box_id.clone(),
                title: run.title.clone(),
                completed: run.completed_tasks(),
                total: run.tasks.len(),
                workers: run.open_attempts().count(),
                eta_secs: estimate(run, now),
                status: run.status(),
                paused: run.paused_at.is_some(),
            })
            .collect();

        let usage = self.usage.usage(&models_in_use(&agents, &sessions));
        Ok(Overview {
            boxes: box_overviews,
            runs: run_overviews,
            usage,
            system: self.system.stats(),
            totals: Totals {
                agents_working: agents.iter().filter(working).count(),
                terminals: sessions.len(),
                attached: attached.len(),
            },
            at: now,
        })
    }
}

/// Each provider and model that a live agent runs, in order. A provider whose agents have
/// not reported a model appears once without one, unless another agent names its model.
fn models_in_use(
    agents: &[ManagedAgent],
    sessions: &[domain::session::Session],
) -> Vec<ModelInUse> {
    let mut named = BTreeSet::new();
    let mut unnamed = BTreeSet::new();
    let providers = agents
        .iter()
        .filter_map(|agent| match &agent.provider {
            Provider::Agent(id) => Some((id.as_str().to_owned(), agent.model.clone())),
            Provider::Command => None,
        })
        .chain(sessions.iter().filter_map(|session| match &session.agent {
            Agent::Coding(id) => Some((id.as_str().to_owned(), None)),
            Agent::Shell => None,
        }));
    for (provider, model) in providers {
        match model.filter(|model| !model.trim().is_empty()) {
            Some(model) => {
                named.insert(ModelInUse {
                    provider,
                    model: Some(model),
                });
            }
            None => {
                unnamed.insert(provider);
            }
        }
    }
    let mut models: Vec<_> = named.iter().cloned().collect();
    models.extend(
        unnamed
            .into_iter()
            .filter(|provider| !named.iter().any(|entry| &entry.provider == provider))
            .map(|provider| ModelInUse {
                provider,
                model: None,
            }),
    );
    models.sort();
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{
        dag::{AgentState, Provider},
        session::Session,
    };
    use std::path::PathBuf;

    fn agent(provider: Provider, model: Option<&str>) -> ManagedAgent {
        ManagedAgent {
            id: "a".into(),
            box_id: "box".into(),
            worktree: PathBuf::new(),
            provider,
            model: model.map(Into::into),
            parent: None,
            session: None,
            run: None,
            task: None,
            attempt: None,
            state: AgentState::Running,
            paused: false,
            status: AgentStatus::Working,
            label: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn models_in_use_name_each_model_once_and_providers_without_one() {
        let agents = [
            agent(Provider::Agent("claude".into()), Some("claude-opus-5-5")),
            agent(Provider::Agent("claude".into()), Some("claude-opus-5-5")),
            agent(Provider::Agent("claude".into()), None),
            agent(Provider::Agent("codex".into()), None),
            agent(Provider::Command, None),
        ];
        let sessions = [
            Session {
                id: "s".into(),
                worktree: PathBuf::new(),
                agent: Agent::Coding("grok".into()),
            },
            Session {
                id: "t".into(),
                worktree: PathBuf::new(),
                agent: Agent::Shell,
            },
        ];
        let model = |provider: &str, model: Option<&str>| ModelInUse {
            provider: provider.into(),
            model: model.map(Into::into),
        };
        assert_eq!(
            models_in_use(&agents, &sessions),
            [
                model("claude", Some("claude-opus-5-5")),
                model("codex", None),
                model("grok", None),
            ]
        );
    }
}
