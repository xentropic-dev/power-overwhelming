# DAG orchestration in Power

## Action gates

Use the same Power executable that served this guide to load the reference for your task.
From an external box, substitute `power` on PATH for `"$POWER_BIN"`.
`power dag guide` is an alias for `power skills get power-dag --full`, which prints this guide
followed by every reference.

| When | Load |
| --- | --- |
| You are a dispatched worker or `POWER_TASK` is set | `"$POWER_BIN" skills get power-dag --reference references/worker-contract.md` |
| A task failed or was interrupted, or you are resuming an existing run | `"$POWER_BIN" skills get power-dag --reference references/recovery.md` |
| You need a worked example of parallel tasks and dependencies | `"$POWER_BIN" skills get power-dag --reference references/example.md` |

Dispatched workers should load the worker contract before acting on the task.
The steps below are for coordinators.

## Find the box and run

- Outside Power, use `power box list` to find the repository's box. Pass `--box "$BOX"` to `power dag list` and `power dag create`. The selector can be a box ID, unique ID prefix, shortcut digit, or unique repository folder name. On create, supply `--coordinator "$LABEL"` to identify the external coordinator, for example `herdr:w6J:p3`.
- Inside Power, omit both flags to use your terminal's box and register or reuse its agent as the coordinator on create.
- To reopen a run, find its ID with `power dag list` and load it with `power dag show "$RUN"`. To start one, use `power dag create --title "Task plan"` with the appropriate flags and save `run.id` as `RUN`.

Both `power dag create` and `power dag show` return JSON with `run` and `link` fields (`{"run":Run,"link":"pwr://dag/<id>"}`). After creating or reopening a run, include the returned `link` verbatim on its own line in your reply. The user can click it or press Ctrl+S then `o` to open the live graph.

## Define the tasks

Use `power dag apply "$RUN" --file tasks.json` to add a batch. The file is an array of tasks, or an object with a `tasks` array. Each task has `id`, `title`, and optional `instructions` and `deps`. IDs are unique within the run and match `[a-z0-9][a-z0-9_-]{0,63}`. Dependencies refer to IDs in the batch or already in the run. An invalid reference or cycle rejects the entire batch.

To add tasks and dependencies one at a time, use these forms. Repeat `--after` for multiple prerequisites, and use `--instructions-file` to supply instructions from a file.

```sh
power dag add-task "$RUN" --id test --title "Run tests" --instructions "Run the workspace tests" --after build
power dag add-dep "$RUN" test lint  # test depends on lint
```

## Dispatch and monitor

1. Use `power dag ready "$RUN"` to find tasks whose prerequisites have all completed successfully. Set `PROFILE` to a provider, which is an agent profile ID: a built-in `claude`, `codex`, or `grok`, or a profile from `config.lua`. Pick one whose agent is installed. Use `git -C "$REPOSITORY" worktree list` to find an existing worktree of the box's repository. Use separate worktrees for concurrent edits. For test fixtures, `--provider command` runs the command supplied after `--`.
2. Run `power dag dispatch "$RUN" "$TASK" --provider "$PROFILE" --worktree "$WORKTREE"`. Power starts the worker in a background terminal and gives it the task instructions, completed prerequisite summaries, and reporting contract. Dispatch other ready tasks as appropriate.
3. Run `power dag wait "$RUN" --since "$CURSOR" --timeout 60`. Start `CURSOR` at `0` for a new run. Process the returned `events`, then save the returned `cursor` and pass it as `--since` on the next wait. A timeout returns an empty event list with the same cursor. After results, check `power dag ready "$RUN"` again and dispatch newly ready tasks.
4. On `needs_input`, surface the worker's question to the user. They can open the worker from the graph (Ctrl+B then `d`) or the agent tree (Ctrl+B then `a`) and answer in that worker's terminal. The worker reports `working` once answered, clearing the request.
