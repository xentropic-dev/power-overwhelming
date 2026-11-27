## Recover and retry a run

On `failed` or `interrupted`, use `power dag show "$RUN"` to inspect the attempt summary or exit detail. To start a new attempt, use `power dag retry` or `dispatch` with `--retry`.

- If Power created the worktree for the previous attempt, run `power dag retry "$RUN" "$TASK" --provider "$PROFILE"`. The worker resumes in the same worktree and branch, with any uncommitted changes still in place.
- If you dispatched to an existing checkout by path, pass that path again with `--worktree "$WORKTREE"`, plus `--allow-main-checkout` if you used it.
- To retry in a fresh worktree, pass `--branch "$BRANCH"` with an unused branch name. Outside Power, also pass `--base` with the starting commit or branch. The earlier worktree stays available until pruned.

Use `show` to confirm all tasks are `completed` before calling the run complete.

## Correct instructions instead of restarting

If a task's instructions were wrong or a decision changed, correct the task with `power dag edit-task "$RUN" "$TASK" --instructions-file "$FILE"`. Don't add placeholder prerequisite tasks, and don't kill a worker to retry it with new instructions. A `working` or `needs_input` worker gets the new instructions through power talk and keeps its terminal and running commands. For a `failed` or `interrupted` task, edit it first, then `power dag retry`, and the new attempt's prompt uses the edited text. A `completed` task cannot be edited, so add a follow-up task that depends on it.

After a restart, find the same run with `list` and inspect it with `show`. Check `ready` for tasks you can dispatch now. Continue waiting from your saved cursor, or replay events from `0` if the cursor was lost. Workers survive a desktop restart. A daemon restart or reboot recreates each worker session under the same ID and resumes its agent conversation, so its attempt stays active. A worker whose attempt is `active` is also told to continue its task, and the run records `attempt_resumed`, which `wait` returns by default. A worker in `needs_input` resumes without a prompt and still waits for the user's answer. Attempts whose session could not be restored, such as command workers or workers whose worktree is gone, are marked `interrupted`, so inspect them before explicitly retrying.

For a stalled run with live workers, use `power talk send --run "$RUN" "Please check your task and report progress."` to queue a nudge for each active worker in the run.

Run `power worktree prune --dry-run` to preview which Power-created worktrees can be removed and why others would be kept. Outside Power, pass `--box "$BOX"` or `--all` to select the scope. Prune can remove a worktree when it has no uncommitted or untracked changes, its branch is merged into the repository's default branch, and no live session or active attempt uses it.
