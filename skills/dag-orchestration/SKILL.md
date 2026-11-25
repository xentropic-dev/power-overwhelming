---
name: dag-orchestration
description: Coordinate background Codex or Claude Code workers in Power Overwhelming. Use when breaking work into tasks with dependencies, dispatching workers, monitoring results and input requests, or resuming a DAG run.
---

# DAG orchestration in Power

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

1. Use `power dag ready "$RUN"` to find tasks whose prerequisites have all completed successfully. Choose `codex` or `claude` based on the installed provider, and use `git -C "$REPOSITORY" worktree list` to find an existing worktree of the box's repository. Use separate worktrees for concurrent edits. For test fixtures, `--provider command` runs the command supplied after `--`.
2. Run `power dag dispatch "$RUN" "$TASK" --provider codex --worktree "$WORKTREE"`. Power starts the worker in a background terminal and gives it the task instructions, completed prerequisite summaries, and reporting contract. Dispatch other ready tasks as appropriate.
3. Run `power dag wait "$RUN" --since "$CURSOR" --timeout 60`. Start `CURSOR` at `0` for a new run. Process the returned `events`, then save the returned `cursor` and pass it as `--since` on the next wait. A timeout returns an empty event list with the same cursor. After results, check `power dag ready "$RUN"` again and dispatch newly ready tasks.
4. On `needs_input`, surface the worker's question to the user. They can open the worker from the graph (Ctrl+B then `d`) or the agent tree (Ctrl+B then `a`) and answer in that worker's terminal. The worker reports `working` once answered, clearing the request.
5. On `failed` or `interrupted`, use `power dag show "$RUN"` to inspect the attempt summary or exit detail. Decide whether to start a new attempt with `power dag retry "$RUN" "$TASK" --provider codex --worktree "$WORKTREE"` or `dispatch` with `--retry`. Use `show` to confirm all tasks are `completed` before calling the run complete.

After a restart, find the same run with `list` and inspect it with `show`. Check `ready` for tasks you can dispatch now. Continue waiting from your saved cursor, or replay events from `0` if the cursor was lost. Workers survive a desktop restart while the daemon stays running. A daemon restart marks outstanding attempts with unavailable sessions `interrupted`, so inspect them before explicitly retrying.

## Example

From outside Power, set `BOX` to your selector and `REPOSITORY` to the `repository` path from `power box list`. Pick `WT1` and `WT2` from `git -C "$REPOSITORY" worktree list`. Use an installed provider for each dispatch.

```sh
power dag create --box "$BOX" --coordinator "docs-review" --title "Review project documentation"
```

Set `RUN` to the returned `run.id`. Put the returned `link` verbatim on its own line in your reply so the user can follow the graph while the workers run. When resuming this example, use `power dag show "$RUN"` to retrieve the link again.

```sh
TASKS_FILE=$(mktemp)
cat > "$TASKS_FILE" <<'JSON'
[
  {"id":"setup","title":"Review setup docs","instructions":"Summarize missing setup steps."},
  {"id":"usage","title":"Review usage docs","instructions":"Summarize missing usage examples."},
  {"id":"synthesize","title":"Combine findings","instructions":"Prioritize changes using the two summaries.","deps":["setup","usage"]}
]
JSON
power dag apply "$RUN" --file "$TASKS_FILE"
power dag ready "$RUN"  # setup and usage
power dag dispatch "$RUN" setup --provider codex --worktree "$WT1"
power dag dispatch "$RUN" usage --provider codex --worktree "$WT2"
CURSOR=0
power dag wait "$RUN" --since "$CURSOR" --timeout 60
```

After each wait, update `CURSOR` from the response. Repeat until both reviews succeed and `power dag ready "$RUN"` lists `synthesize`.

```sh
power dag dispatch "$RUN" synthesize --provider codex --worktree "$WT1"
power dag wait "$RUN" --since "$CURSOR" --timeout 60
power dag show "$RUN"
```

Continue waiting until `synthesize` completes, then read its attempt summary with `show`.

## Worker reporting contract

If `POWER_TASK` is set, report from your worker terminal. `power dag report` uses `POWER_RUN`, `POWER_TASK`, and `POWER_ATTEMPT` to identify your assigned attempt. If you need input, report the question, wait for the user to answer in this terminal, then report that you are working again.

```sh
power dag report needs-input --summary "Which configuration should I use?"
# Wait for the user's answer.
power dag report working --summary "Configuration confirmed"
```

Before exiting, report the outcome with one of these commands.

- `power dag report success --summary "Reviewed setup and identified two missing steps"`
- `power dag report failed --summary "Required fixture is unavailable"`

Exiting without a terminal report leaves the attempt `interrupted`. A replayed terminal report or a report from a superseded attempt returns `applied: false` and leaves the current task state intact.
