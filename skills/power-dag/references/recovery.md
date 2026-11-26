## Recover and retry a run

On `failed` or `interrupted`, use `power dag show "$RUN"` to inspect the attempt summary or exit detail. Decide whether to start a new attempt with `power dag retry "$RUN" "$TASK" --provider "$PROFILE" --worktree "$WORKTREE"` or `dispatch` with `--retry`. Use `show` to confirm all tasks are `completed` before calling the run complete.

After a restart, find the same run with `list` and inspect it with `show`. Check `ready` for tasks you can dispatch now. Continue waiting from your saved cursor, or replay events from `0` if the cursor was lost. Workers survive a desktop restart while the daemon stays running. A daemon restart marks outstanding attempts with unavailable sessions `interrupted`, so inspect them before explicitly retrying.
