use anyhow::{Context, Result, bail, ensure};
use application::{skill_install::SkillStub, skills::SkillCatalog};
use domain::skill::{Skill, SkillReference, SkillSummary};
use sha2::{Digest, Sha256};

struct EmbeddedSkill {
    stub: &'static str,
    shipped_stub_hashes: &'static [&'static str],
    guide: &'static str,
    references: &'static [(&'static str, &'static str)],
}

// When changing a stub, append its new SHA-256 and keep the previous release hashes.
const SKILLS: &[EmbeddedSkill] = &[
    EmbeddedSkill {
        stub: include_str!("../../skills/power-artifacts/SKILL.md"),
        shipped_stub_hashes: &["a4f012b3bd28d1f7207b112ba55673896ed2cf21123a48908f771395c3226ea3"],
        guide: include_str!("../../skills/power-artifacts/guide.md"),
        references: &[],
    },
    EmbeddedSkill {
        stub: include_str!("../../skills/power-dag/SKILL.md"),
        shipped_stub_hashes: &["776c7037a75329bb90681af1e0929c95e86bc8ad4b7eab41ee258456e5216513"],
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

impl EmbeddedSkillCatalog {
    pub fn stubs(&self) -> Result<Vec<SkillStub>> {
        SKILLS
            .iter()
            .map(|skill| {
                let summary = parse_frontmatter(skill.stub)?;
                Ok(SkillStub {
                    name: summary.name,
                    description: summary.description,
                    markdown: skill.stub,
                    current_hash: stub_hash(skill.stub.as_bytes()),
                    shipped_hashes: skill.shipped_stub_hashes,
                })
            })
            .collect()
    }
}

pub(crate) fn stub_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

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
pub(crate) fn parse_frontmatter(stub: &str) -> Result<SkillSummary> {
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
    fn current_stubs_have_shipped_hashes() {
        for stub in EmbeddedSkillCatalog.stubs().unwrap() {
            assert!(
                stub.shipped_hashes.contains(&stub.current_hash.as_str()),
                "{} is missing its current SHA-256 {}",
                stub.name,
                stub.current_hash
            );
        }
    }

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
