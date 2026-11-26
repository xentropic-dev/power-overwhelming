## Recover and retry a run

On `failed` or `interrupted`, use `power dag show "$RUN"` to inspect the attempt summary or exit detail. To start a new attempt, use `power dag retry` or `dispatch` with `--retry`.

- If Power created the worktree for the previous attempt, run `power dag retry "$RUN" "$TASK" --provider "$PROFILE"`. The worker resumes in the same worktree and branch, with any uncommitted changes still in place.
- If you dispatched to an existing checkout by path, pass that path again with `--worktree "$WORKTREE"`.
- To retry in a fresh worktree, pass `--worktree new --branch "$BRANCH"` with an unused branch name. Outside Power, also pass `--base` with the starting commit or branch. The earlier worktree stays available until pruned.

Use `show` to confirm all tasks are `completed` before calling the run complete.

After a restart, find the same run with `list` and inspect it with `show`. Check `ready` for tasks you can dispatch now. Continue waiting from your saved cursor, or replay events from `0` if the cursor was lost. Workers survive a desktop restart. A daemon restart or reboot recreates each worker session under the same ID and resumes its agent conversation, so its attempt stays active. Attempts whose session could not be restored, such as command workers or workers whose worktree is gone, are marked `interrupted`, so inspect them before explicitly retrying.

Run `power worktree prune --dry-run` to preview which Power-created worktrees can be removed and why others would be kept. Prune can remove a worktree when it has no uncommitted or untracked changes, its branch is merged into the repository's default branch, and no live session or active attempt uses it.
