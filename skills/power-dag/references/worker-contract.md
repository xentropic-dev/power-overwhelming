## Worker reporting contract

A dispatched worker starts with a prompt that names the run, the task ID and title, and the worktree, followed by the task instructions and a summary from each completed prerequisite. Work in that worktree.

If `POWER_TASK` is set, report from your worker terminal. `power dag report` uses `POWER_RUN`, `POWER_TASK`, and `POWER_ATTEMPT` to identify your assigned attempt, so you never pass them yourself.

If a question is for your coordinator, use `power talk send coordinator "Your question" --wait`. Reserve `needs-input` for decisions only the user can make. If you need user input, report the question, wait for the user's answer in this terminal, then report that you are working again before continuing.

```sh
power dag report needs-input --summary "Which configuration should I use?"
# Wait for the user's answer.
power dag report working --summary "Configuration confirmed"
```

Many agents share this machine, so never wait on processes by name with `pgrep`, `pidof`, `killall`, or `ps | grep`. Those match other agents' builds and your own wait loop, so the wait never ends. Wait on your own command instead: run it in the foreground, or use your tool's background-task completion or the PID you started. Give every long wait a timeout.

```sh
# Never: until ! pgrep -f "cargo clippy" >/dev/null; do sleep 5; done
timeout 900 cargo clippy --all-targets
```

Power limits how many workers run at once across the machine (`dag.max_active_workers` and `dag.min_available_memory`), and your attempt counts until you report `success` or `failed`. Report as soon as your work is done so queued workers can start. Keep your own footprint small: run one heavy build at a time.

Publish screenshots, reports, HTML pages, and other visual results from your worker session with `power artifact publish <file> --title "..."` so they appear on your task's node. Mention relevant artifacts and their links in your final summary.

Before exiting this terminal, report the outcome with one of these commands.

- When done: `power dag report success --summary "Reviewed setup and identified two missing steps"`
- If you cannot complete the task: `power dag report failed --summary "Required fixture is unavailable"`

Exiting without a terminal report leaves the attempt `interrupted`. A replayed terminal report or a report from a superseded attempt returns `applied: false` and leaves the current task state intact.
