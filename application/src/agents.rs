use anyhow::{Context, Result};
use domain::agent::{AgentId, AgentProfile};

pub trait AgentProfileRegistry: Send + Sync {
    fn profiles(&self) -> &[AgentProfile];

    fn resolve(&self, id: &AgentId) -> Result<&AgentProfile> {
        let profiles = self.profiles();
        profiles
            .iter()
            .find(|profile| &profile.id == id)
            .with_context(|| {
                let known = profiles
                    .iter()
                    .map(|profile| profile.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Unknown agent profile {:?}. Known profiles: {known}",
                    id.as_str()
                )
            })
    }
}
