use anyhow::{Result, bail};
use domain::{
    agent::AgentProfile,
    skill::{Skill, SkillSummary},
};
use std::collections::BTreeSet;

pub trait SkillCatalog: Send + Sync {
    fn list(&self) -> Result<Vec<SkillSummary>>;
    fn get(&self, name: &str) -> Result<Option<Skill>>;
}

pub struct ListSkills;

impl ListSkills {
    pub fn execute(catalog: &dyn SkillCatalog) -> Result<Vec<SkillSummary>> {
        let mut summaries = catalog.list()?;
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(summaries)
    }
}

/// Skill names an agent already discovers from its own skill folders.
pub trait InstalledSkills {
    fn installed(&self, profile: &AgentProfile) -> Result<BTreeSet<String>>;
}

/// Catalog skills to advertise when launching an agent: those it cannot already discover.
pub struct LaunchSkills;

impl LaunchSkills {
    pub fn execute(
        catalog: &dyn SkillCatalog,
        installed: &dyn InstalledSkills,
        profile: &AgentProfile,
    ) -> Result<Vec<SkillSummary>> {
        // Unreadable skill folders only cost a longer prompt, so advertise everything.
        let installed = installed.installed(profile).unwrap_or_default();
        let mut skills = ListSkills::execute(catalog)?;
        skills.retain(|skill| !installed.contains(&skill.name));
        Ok(skills)
    }
}

pub enum SkillSelection<'a> {
    Kernel,
    Reference(&'a str),
    References,
    Full,
}

pub enum SkillOutput {
    Guide { markdown: String, full: bool },
    Reference { name: String, markdown: String },
    References(Vec<String>),
}

pub struct GetSkill<'a> {
    pub name: &'a str,
    pub selection: SkillSelection<'a>,
}

impl GetSkill<'_> {
    pub fn execute(&self, catalog: &dyn SkillCatalog) -> Result<SkillOutput> {
        let Some(mut skill) = catalog.get(self.name)? else {
            let summaries = ListSkills::execute(catalog)?;
            let names = summaries.iter().map(|summary| summary.name.as_str());
            bail!(
                "Unknown skill {:?}. Available skills: {}",
                self.name,
                available_names(names)
            );
        };

        match self.selection {
            SkillSelection::Kernel => Ok(SkillOutput::Guide {
                markdown: skill.guide,
                full: false,
            }),
            SkillSelection::Full => {
                let mut markdown = skill.guide;
                for reference in skill.references {
                    markdown.truncate(markdown.trim_end_matches('\n').len());
                    markdown.push_str("\n\n---\n\n");
                    markdown.push_str(&reference.markdown);
                }
                Ok(SkillOutput::Guide {
                    markdown,
                    full: true,
                })
            }
            SkillSelection::References => Ok(SkillOutput::References(
                skill
                    .references
                    .into_iter()
                    .map(|entry| entry.name)
                    .collect(),
            )),
            SkillSelection::Reference(requested) => {
                let name = requested.trim();
                let name = name.strip_prefix("references/").unwrap_or(name);
                let name = name.strip_suffix(".md").unwrap_or(name);
                if let Some(index) = skill.references.iter().position(|entry| entry.name == name) {
                    let reference = skill.references.swap_remove(index);
                    return Ok(SkillOutput::Reference {
                        name: reference.name,
                        markdown: reference.markdown,
                    });
                }
                bail!(
                    "Unknown reference {requested:?} for {:?}. Available references: {}",
                    self.name,
                    available_names(skill.references.iter().map(|entry| entry.name.as_str()))
                );
            }
        }
    }
}

fn available_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names = names.collect::<Vec<_>>();
    if names.is_empty() {
        return "(none)".into();
    }
    names.join(", ")
}
