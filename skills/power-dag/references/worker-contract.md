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
