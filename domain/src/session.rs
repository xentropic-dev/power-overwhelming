use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Agent {
    Shell,
    Codex,
    Claude,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Self::Shell => "Shell",
            Self::Codex => "Codex",
            Self::Claude => "Claude",
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
