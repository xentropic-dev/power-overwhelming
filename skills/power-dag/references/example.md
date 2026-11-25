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
