#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub summary: SkillSummary,
    pub guide: String,
    pub references: Vec<SkillReference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillReference {
    pub name: String,
    pub markdown: String,
}
