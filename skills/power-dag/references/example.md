## Example

From outside Power, set `BOX` to your selector and `REPOSITORY` to the `repository` path from `power box list`. Set `WORKTREE` to the coordinator's checkout from `git -C "$REPOSITORY" worktree list`. This example uses `origin/main` as the starting point for the parallel edits. Set `PROFILE` to the agent profile ID of an installed agent, such as `codex`, `claude`, or `grok`.

Inside Power, set `WORKTREE` to your current checkout. Omit `--box` and `--coordinator` on create, and omit `--base origin/main` if you want to start the parallel edits at your current HEAD.

```sh
power dag create --box "$BOX" --coordinator "docs-update" --title "Update project documentation"
```

Set `RUN` to the returned `run.id`. Put the returned `link` verbatim on its own line in your reply so the user can follow the graph while the workers run. When resuming this example, use `power dag show "$RUN"` to retrieve the link again.

```sh
TASKS_FILE=$(mktemp)
cat > "$TASKS_FILE" <<'JSON'
[
  {"id":"setup","title":"Update setup docs","instructions":"Fill in missing setup steps and commit the changes."},
  {"id":"usage","title":"Update usage docs","instructions":"Add missing usage examples and commit the changes."},
  {"id":"review","title":"Review navigation","instructions":"Report links the updated docs should include. Leave the checkout unchanged."},
  {"id":"combine","title":"Combine documentation changes","instructions":"Merge docs/usage into this branch and apply the navigation findings. Commit any follow-up changes.","deps":["setup","usage","review"]}
]
JSON
power dag apply "$RUN" --file "$TASKS_FILE"
power dag ready "$RUN"  # setup, usage, and review
power dag dispatch "$RUN" setup --provider "$PROFILE" --worktree new --branch docs/setup --base origin/main
power dag dispatch "$RUN" usage --provider "$PROFILE" --worktree new --branch docs/usage --base origin/main
power dag dispatch "$RUN" review --provider "$PROFILE" --worktree "$WORKTREE"
CURSOR=0
power dag wait "$RUN" --since "$CURSOR" --timeout 60
```

After each wait, update `CURSOR` from the response. Repeat until all three tasks succeed and `power dag ready "$RUN"` lists `combine`. Use `--base docs/setup` to start from the setup task's branch. The combine task then brings in the usage changes.

```sh
power dag dispatch "$RUN" combine --provider "$PROFILE" --worktree new --branch docs/combined --base docs/setup
power dag wait "$RUN" --since "$CURSOR" --timeout 60
power dag show "$RUN"
```

Continue waiting until `combine` completes, then read its attempt summary with `show`.
