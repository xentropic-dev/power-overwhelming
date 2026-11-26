# Power talk design

- [requirements.md](requirements.md): what to build, by layer, with acceptance checks.
- [decisions.md](decisions.md): the choices settled with the user on 2026-11-26 and the alternatives considered.
- [power-talk.dag.json](power-talk.dag.json): the task graph for `power dag apply`. Tracking issue [#78](https://github.com/xentropic-dev/power-overwhelming/issues/78).

## Running the graph under the worker cap

The graph is meant to run with at most two workers at once and one heavy cargo build at a time.

- Each implementation task is followed by a review task. Run the review on a different provider from the author. A review depends only on its own implementation task, and the next implementation task depends on the review, so the usual pair running at once is one author and one reviewer.
- `usecases` depends on `store`, and `turn-hooks` depends on `cli`, only to keep heavy builds one at a time. They don’t need each other’s code.
- `skills` is a light documentation task. It may become ready while `turn-hooks` or its review is running. Hold it if two workers are already active.
- Merge each author PR after its review passes and before dispatching the tasks that depend on that review. Dependents branch from fresh origin/main.
- Dispatch with `--branch <name>`, so Power creates each worktree. Never restart the live daemon while workers are active.
