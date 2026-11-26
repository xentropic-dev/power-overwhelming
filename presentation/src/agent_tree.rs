use infrastructure::client::ManagedAgent;
use nucleo_matcher::{
    Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
};

#[derive(Default)]
pub(crate) struct AgentTree {
    agents: Vec<ManagedAgent>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    roots: Vec<usize>,
    collapsed: HashSet<usize>,
    visible: Vec<(usize, usize)>,
    searching: bool,
}

impl AgentTree {
    pub fn new(agents: Vec<ManagedAgent>) -> Self {
        let ids: HashMap<_, _> = agents
            .iter()
            .enumerate()
            .map(|(index, agent)| (agent.id.as_str(), index))
            .collect();
        let parents: Vec<_> = agents
            .iter()
            .map(|agent| agent.parent.as_deref().and_then(|id| ids.get(id).copied()))
            .collect();
        let mut children = vec![Vec::new(); agents.len()];
        let mut roots = Vec::new();
        for (index, parent) in parents.iter().copied().enumerate() {
            if let Some(parent) = parent {
                children[parent].push(index);
            } else {
                roots.push(index);
            }
        }
        Self {
            agents,
            parents,
            children,
            roots,
            ..Self::default()
        }
    }

    pub fn search(&mut self, query: &str, matcher: &mut Matcher) -> usize {
        self.searching = !query.trim().is_empty();
        if !self.searching {
            self.rebuild(|_| true);
            return 0;
        }
        let mut included = vec![false; self.agents.len()];
        let mut scores = vec![None; self.agents.len()];
        let pattern = Pattern::parse(query.trim(), CaseMatching::Ignore, Normalization::Smart);
        let mut buffer = Vec::new();
        for (index, agent) in self.agents.iter().enumerate() {
            let worktree = agent
                .worktree
                .file_name()
                .unwrap_or(agent.worktree.as_os_str());
            let text = format!(
                "{} {} {} {}",
                agent.label,
                agent.task.as_deref().unwrap_or(""),
                agent.provider.name(),
                worktree.to_string_lossy()
            );
            if let Some(score) = pattern.score(Utf32Str::new(&text, &mut buffer), matcher) {
                scores[index] = Some(score);
                let mut current = Some(index);
                while let Some(ancestor) = current.filter(|&index| !included[index]) {
                    included[ancestor] = true;
                    current = self.parents[ancestor];
                }
            }
        }
        self.rebuild(|index| included[index]);
        self.visible
            .iter()
            .enumerate()
            .filter_map(|(row, &(index, _))| Some((Reverse(scores[index]?), row)))
            .min()
            .map_or(0, |(_, row)| row)
    }

    fn rebuild(&mut self, included: impl Fn(usize) -> bool) {
        self.visible.clear();
        let mut pending: Vec<_> = self.roots.iter().rev().map(|&root| (root, 0)).collect();
        while let Some((index, depth)) = pending.pop() {
            if included(index) {
                self.visible.push((index, depth));
                if self.searching || !self.collapsed.contains(&index) {
                    pending.extend(
                        self.children[index]
                            .iter()
                            .rev()
                            .map(|&child| (child, depth + 1)),
                    );
                }
            }
        }
    }

    pub fn rows(&self) -> impl Iterator<Item = (&ManagedAgent, usize, &'static str)> {
        self.visible.iter().map(|&(index, depth)| {
            let marker = if self.children[index].is_empty() {
                "·"
            } else if !self.searching && self.collapsed.contains(&index) {
                "▸"
            } else {
                "▾"
            };
            (&self.agents[index], depth, marker)
        })
    }

    pub fn agent(&self, id: &str) -> Option<&ManagedAgent> {
        self.agents.iter().find(|agent| agent.id == id)
    }

    /// While filtering, keep matching workers and their ancestors open.
    pub fn branch(&mut self, selected: usize, expand: bool) -> Option<usize> {
        let &(index, _) = self.visible.get(selected)?;
        let target = if expand {
            if !self.searching && self.collapsed.remove(&index) {
                self.rebuild(|_| true);
                index
            } else {
                self.visible
                    .get(selected + 1)
                    .map(|&(next, _)| next)
                    .filter(|&next| self.parents[next] == Some(index))?
            }
        } else if !self.searching
            && !self.children[index].is_empty()
            && self.collapsed.insert(index)
        {
            self.rebuild(|_| true);
            index
        } else {
            self.parents[index]?
        };
        self.visible.iter().position(|&(index, _)| index == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infrastructure::client::Provider;
    use nucleo_matcher::Config;

    fn agent(id: &str, parent: Option<&str>, provider: Provider, worktree: &str) -> ManagedAgent {
        ManagedAgent {
            id: id.into(),
            box_id: "box".into(),
            worktree: worktree.into(),
            provider,
            parent: parent.map(str::to_owned),
            session: None,
            run: None,
            task: None,
            attempt: None,
            state: infrastructure::client::AgentState::Running,
            label: id.into(),
            created_at: 0,
        }
    }

    #[test]
    fn fuzzy_search_reveals_matches_and_ancestors_in_tree_order() {
        let mut tree = AgentTree::new(vec![
            agent(
                "worker",
                Some("delegate"),
                Provider::Agent("claude".into()),
                "/repo/api-fix",
            ),
            agent("delegator", None, Provider::Agent("codex".into()), "/repo"),
            agent(
                "unrelated",
                None,
                Provider::Agent("codex".into()),
                "/repo/unrelated",
            ),
            agent(
                "delegate",
                Some("delegator"),
                Provider::Agent("codex".into()),
                "/repo",
            ),
        ]);
        let mut matcher = Matcher::new(Config::DEFAULT);
        tree.search("", &mut matcher);
        assert_eq!(tree.branch(0, false), Some(0));
        assert_eq!(tree.rows().count(), 2);
        assert_eq!(tree.search("clade apfx", &mut matcher), 2);
        assert_eq!(
            tree.rows()
                .map(|(agent, depth, _)| (agent.id.as_str(), depth))
                .collect::<Vec<_>>(),
            [("delegator", 0), ("delegate", 1), ("worker", 2)]
        );
        tree.search("", &mut matcher);
        assert_eq!(tree.rows().count(), 2);
        assert_eq!(tree.branch(0, true), Some(0));
        assert_eq!(tree.branch(0, true), Some(1));
        assert_eq!(tree.branch(2, false), Some(1));
    }
}
