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
- To reopen a run, find its ID with `power dag list` (add `--all` to include completed and archived runs) and load it with `power dag show "$RUN"`. To start one, use `power dag create --title "Task plan"` with the appropriate flags and save `run.id` as `RUN`.

Both `power dag create` and `power dag show` return JSON with `run` and `link` fields (`{"run":Run,"link":"pwr://dag/<id>"}`). After creating or reopening a run, include the returned `link` verbatim on its own line in your reply. The user can click it or press Ctrl+S then `o` to open the live graph.

## Define the tasks

Use `power dag apply "$RUN" --file tasks.json` to add a batch. The file is an array of tasks, or an object with a `tasks` array. Each task has `id`, `title`, and optional `instructions` and `deps`. IDs are unique within the run and match `[a-z0-9][a-z0-9_-]{0,63}`. Dependencies refer to IDs in the batch or already in the run. An invalid reference or cycle rejects the entire batch.

To add tasks and dependencies one at a time, use these forms. Repeat `--after` for multiple prerequisites, and use `--instructions-file` to supply instructions from a file.

```sh
power dag add-task "$RUN" --id test --title "Run tests" --instructions "Run the workspace tests" --after build
power dag add-dep "$RUN" test lint  # test depends on lint
```

## Dispatch and monitor

1. Use `power dag ready "$RUN"` to find tasks whose prerequisites have all completed successfully. Set `PROFILE` to an installed agent profile ID, such as `claude`, `codex`, `grok`, `jcode`, or a profile from `config.lua`. For test fixtures, `--provider command` runs the command supplied after `--`. It follows the same worktree rules as other providers, so pointing it at the main checkout with `--worktree` also needs `--allow-main-checkout`.
2. Dispatch each ready task with the appropriate worktree.

   - For edits, run `power dag dispatch "$RUN" "$TASK" --provider "$PROFILE" --branch "$BRANCH"`. Power creates a worktree for the task on that branch, which is the default when `--worktree` is omitted. Choose a distinct, unused branch name for each task. Inside Power, the new branch starts at the coordinator's current HEAD. Pass `--base` with a branch or other commit-ish to override it. For a dependent task that builds on a prerequisite's commits, pass the prerequisite's branch, for example `--base docs/setup`. Outside Power, `--base` is required when creating a worktree.
   - For read-only tasks, you can pass an existing worktree with `power dag dispatch "$RUN" "$TASK" --provider "$PROFILE" --worktree "$WORKTREE"`. Use `git -C "$REPOSITORY" worktree list` to find existing worktrees of the box's repository. Power refuses the repository's main checkout, the first entry in that list, unless you also pass `--allow-main-checkout`. Never send editing work there.

   Make your own code changes in a worktree too, never in the main checkout. The user and other coordinators work there.

   Power starts the worker in a background terminal and gives it the task instructions, completed prerequisite summaries, and reporting contract. If Power creates a worktree, the dispatch output includes its path and branch. The default location is `~/.power/<box>/<branch-slug>`, where `<branch-slug>` is the branch name with `/` replaced by `-`. Set `worktrees.root` in `config.lua` to choose a different root. Older releases used `~/.local/share/power-overwhelming/worktrees`. Power does not move worktrees created there, so they keep working in place; set `worktrees.root` to that path if you want new worktrees alongside them.

   Power enforces a machine-wide worker budget, so dispatch every ready task and let Power pace them. When the budget is full, `power dag dispatch` prints `{"queued": true, "reason": ...}` instead of an attempt, the task shows as `queued` in `power dag show` and the graph, and Power starts it as soon as a worker reports or enough memory frees up. `power dag ready` lists queued task IDs under `queued`. Don't hold tasks back, poll, or re-dispatch queued ones. Pass `--no-queue` to fail instead of queueing. Only the user overrides the budget, with `--over-budget` or `s` on a queued task in the graph; never pass `--over-budget` unless they ask. A queued dispatch can fail to start in two ways. If it fails before Power records an attempt, for example because the worktree or provider is invalid, Power drops it from the queue, emits `queued_dispatch_failed` with the error as its summary, and the task returns to the state it had before it was queued: `ready` for a first dispatch, or `failed` or `interrupted` for a queued `--retry`. If it fails after the attempt is recorded, for example when Git cannot create the new worktree, the queue entry is already gone, Power emits `attempt_failed`, and the task becomes `failed`. Watch for both events, and retry with `--retry` after fixing the cause.

3. Run `power dag wait "$RUN" --since "$CURSOR" --timeout 60`. Start `CURSOR` at `0` for a new run. Process the returned `events`, then save the returned `cursor` and pass it as `--since` on the next wait. A timeout returns an empty event list with the same cursor. After results, check `power dag ready "$RUN"` again and dispatch newly ready tasks.
4. On `needs_input`, surface the worker's question to the user. They can open the worker from the graph (Ctrl+B then `d`) or the agent tree (Ctrl+B then `a`) and answer in that worker's terminal. The worker reports `working` once answered, clearing the request.

To set aside a run you have abandoned, run `power dag archive "$RUN"`. Archived runs leave `power dag list` and the run picker until `power dag unarchive "$RUN"`, but `power dag show` still loads them. Power refuses to archive a run with open attempts so live workers stay visible; pass `--force` only when the user confirms those workers can be hidden.

To take load off the machine without losing work, `power dag pause "$RUN"` pauses every active worker of the run and stops dispatching from it; its tasks show `paused`, which is not working. `power dag resume "$RUN"` continues them. Never treat a paused worker as making progress.

When retrying, Power reuses the task's Power-created worktree and branch by default. See the recovery reference for retry commands. To preview which Power-created worktrees can be removed, run `power worktree prune --dry-run`. Outside Power, also select a box with `--box "$BOX"` or use `--all`.

Use `power dag show "$RUN"` to read each attempt's `artifacts` (`id`, `title`, `link`) in publish order, newest last. In the DAG view (Ctrl+B then `d`), the selected node's Artifacts list shows the current attempt's artifacts. Open a listed artifact or its `pwr://artifact/<id>` link in the preview pane.

Send review feedback to a worker with `power talk send task:<id> "Your feedback"`. Add `--run "$RUN"` when coordinating more than one run.
