use anyhow::{Context, Result, bail, ensure};
use application::skills::SkillCatalog;
use domain::skill::{Skill, SkillReference, SkillSummary};

pub(crate) const ARTIFACTS_GUIDE: &str = include_str!("../../skills/power-artifacts/guide.md");

struct EmbeddedSkill {
    stub: &'static str,
    guide: &'static str,
    references: &'static [(&'static str, &'static str)],
}

const SKILLS: &[EmbeddedSkill] = &[
    EmbeddedSkill {
        stub: include_str!("../../skills/power-artifacts/SKILL.md"),
        guide: ARTIFACTS_GUIDE,
        references: &[],
    },
    EmbeddedSkill {
        stub: include_str!("../../skills/power-dag/SKILL.md"),
        guide: include_str!("../../skills/power-dag/guide.md"),
        references: &[
            (
                "worker-contract",
                include_str!("../../skills/power-dag/references/worker-contract.md"),
            ),
            (
                "recovery",
                include_str!("../../skills/power-dag/references/recovery.md"),
            ),
            (
                "example",
                include_str!("../../skills/power-dag/references/example.md"),
            ),
        ],
    },
];

pub struct EmbeddedSkillCatalog;

impl SkillCatalog for EmbeddedSkillCatalog {
    fn list(&self) -> Result<Vec<SkillSummary>> {
        SKILLS
            .iter()
            .map(|skill| parse_frontmatter(skill.stub))
            .collect()
    }

    fn get(&self, name: &str) -> Result<Option<Skill>> {
        for skill in SKILLS {
            let summary = parse_frontmatter(skill.stub)?;
            if summary.name == name {
                return Ok(Some(Skill {
                    summary,
                    guide: skill.guide.into(),
                    references: skill
                        .references
                        .iter()
                        .map(|&(name, markdown)| SkillReference {
                            name: name.into(),
                            markdown: markdown.into(),
                        })
                        .collect(),
                }));
            }
        }
        Ok(None)
    }
}

/// Bundled stubs use single-line plain scalars for name and description.
fn parse_frontmatter(stub: &str) -> Result<SkillSummary> {
    let mut lines = stub.lines();
    ensure!(lines.next() == Some("---"), "Skill is missing frontmatter");
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line == "---" {
            return Ok(SkillSummary {
                name: name.context("Skill frontmatter is missing name")?,
                description: description.context("Skill frontmatter is missing description")?,
            });
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let field = match key {
            "name" => &mut name,
            "description" => &mut description,
            _ => continue,
        };
        let value = value.trim();
        ensure!(!value.is_empty(), "Skill frontmatter has an empty {key}");
        ensure!(field.is_none(), "Skill frontmatter repeats {key}");
        *field = Some(value.to_owned());
    }
    bail!("Skill frontmatter is missing its closing delimiter");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_from_the_frontmatter_only() {
        let stub = "---\r\ndescription:  Load pwr://dag links.  \r\nname: power-dag\r\n---\r\nname: ignored\r\n";
        assert_eq!(
            parse_frontmatter(stub).unwrap(),
            SkillSummary {
                name: "power-dag".into(),
                description: "Load pwr://dag links.".into(),
            }
        );
    }

    #[test]
    fn rejects_missing_delimiters() {
        for stub in [
            "name: power-dag\n",
            "---\nname: power-dag\ndescription: Run DAGs.\n",
        ] {
            assert!(parse_frontmatter(stub).is_err());
        }
    }

    #[test]
    fn rejects_missing_empty_and_duplicate_fields() {
        for stub in [
            "---\ndescription: Run DAGs.\n---\n",
            "---\nname: power-dag\n---\ndescription: Outside frontmatter.\n",
            "---\nname: power-dag\ndescription:  \n---\n",
            "---\nname: power-dag\nname: another\ndescription: Run DAGs.\n---\n",
        ] {
            assert!(parse_frontmatter(stub).is_err());
        }
    }
}
