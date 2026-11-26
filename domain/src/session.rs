use crate::agent::{AgentId, display_name};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Agent {
    Shell,
    Coding(AgentId),
}

impl Agent {
    pub fn name(&self) -> &str {
        match self {
            Self::Shell => "Shell",
            Self::Coding(id) => display_name(id.as_str()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Shell => "shell",
            Self::Coding(id) => id.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub worktree: PathBuf,
    pub agent: Agent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Geometry {
    pub fn is_valid(self) -> bool {
        (1..=500).contains(&self.cols)
            && (1..=500).contains(&self.rows)
            && (1..=100).contains(&self.cell_width)
            && (1..=100).contains(&self.cell_height)
    }
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 18,
        }
    }
}
