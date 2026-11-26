# Power talk

Every agent Power runs should be able to message every other agent in its box through Power, the same way for Claude Code, Codex, Grok, any configured profile, and `--provider command` fixtures. The user can read and send those messages from the keyboard. Power owns addressing, so a message names a Power agent, a delegation role, or a DAG task, never a provider’s own session name. [Decision record](decisions.md) · [Task graph](power-talk.dag.json) · Tracking issue [#78](https://github.com/xentropic-dev/power-overwhelming/issues/78)

## Why

On 2026-11-26 a daemon restart left a dozen DAG workers idle ([#47](https://github.com/xentropic-dev/power-overwhelming/issues/47)). A Claude coordinator could nudge the Claude workers through Claude Code’s own cross-session messaging, but it couldn’t reach the six Codex workers at all. The only fix was ending their sessions and retrying, which lost their conversations. Its messages also went to the wrong sessions, because Claude’s session names don’t map to Power’s sessions, agents or DAG roles. Power already knows every agent’s provider, box, parent, run, task, attempt and session, so it should own messaging.

## Current behavior

- The daemon speaks a JSON request protocol over a Unix socket. `Request::Input { session, data }` writes bytes into any session’s PTY, and `Subscribe` streams a session’s output. Nothing queues, records or acknowledges text sent this way. [Requests](../../../infrastructure/src/wire.rs#L85) · [Input handling](../../../infrastructure/src/daemon.rs#L378)
- Managed agents live in the daemon store with an ID, box, worktree, provider, parent, session, and optional run, task and attempt. Their `status` comes from their state and attempt, not from the terminal, so `Idle` means “no active attempt”, not “waiting at a prompt”. [ManagedAgent](../../../infrastructure/src/wire.rs#L404) · [Status derivation](../../../domain/src/dag.rs#L118) · [Store schema](../../../infrastructure/src/dag_store.rs#L30)
- Power injects a system prompt into each agent it launches, delivered per profile by `AgentProfile.injection`. Claude Code also gets a `--settings` JSON for the model status line. [instruction_args](../../../infrastructure/src/shell.rs#L448) · [model_callback_args](../../../infrastructure/src/shell.rs#L316) · [AgentProfile](../../../domain/src/agent.rs#L42)
- Sessions carry `POWER_SESSION`, `POWER_AGENT`, `POWER_BOX`, and for workers `POWER_RUN`, `POWER_TASK` and `POWER_ATTEMPT`.
- Coordinators learn about workers only through `power dag wait` events. A worker can reach nobody but the user, through `needs-input`. Coordinators send review feedback by asking the user or by typing into a terminal by hand.
- After a daemon restart, restored workers with an active attempt get a continuation prompt ([#72](https://github.com/xentropic-dev/power-overwhelming/pull/72)). The restart guard and stall detection in [#77](https://github.com/xentropic-dev/power-overwhelming/issues/77) are being built now.

## Related work in flight

Power talk builds on these issues and must not duplicate them.

- [#75](https://github.com/xentropic-dev/power-overwhelming/issues/75) ends finished workers. A message to an ended agent is refused, and its earlier messages stay readable.
- [#76](https://github.com/xentropic-dev/power-overwhelming/issues/76) is the worker budget. Talk never starts an agent, so the budget is unaffected.
- [#77](https://github.com/xentropic-dev/power-overwhelming/issues/77) is the restart guard and stall detection. If #77 lands an output-activity tracker first, talk’s idle detection reuses it rather than adding a second one. Moving #77’s restart and stall prompts onto power talk is a follow-up after both merge. It is not part of this graph.
- [#79](https://github.com/xentropic-dev/power-overwhelming/issues/79) adds pause, resume and stop. A paused agent’s messages queue, and Power sends no notice until the agent resumes.
- [#80](https://github.com/xentropic-dev/power-overwhelming/issues/80) collects papercuts. File talk papercuts there once this ships.

## Messages

- A message has an ID, a box, a sender, one recipient, a body, a thread ID, an optional reply-to ID, an optional broadcast ID, a creation time, and delivery timestamps for queued, notified and read.
- The sender is a Power agent, the user, or `system`, which is the daemon itself. Power works out the sender from the calling session: the daemon maps `POWER_SESSION` to its managed agent. A session with no agent, such as a plain shell tab or a terminal outside Power, sends as the user. Power never trusts a sender name given on the command line. All of these processes run as the same OS user, so this guards against mistakes, not attackers.
- The body is UTF-8 text of up to 16 KiB. A larger body is refused with the limit in the error. Pass long content as a file path or an artifact link.
- The thread ID is the ID of the thread’s first message. A reply copies its parent’s thread ID and sets reply-to.

## Addressing

`<to>` is resolved in the sender’s box.

| Address | Resolves to |
| --- | --- |
| Agent ID or unique ID prefix | That agent |
| `parent` | The sender’s `parent` agent |
| `coordinator` | The coordinator agent of the sender’s run. An external coordinator, such as a Herdr pane, is unreachable, and the error names its label. |
| `task:<id>` | The agent on the current attempt of that task in the sender’s run, or in the run given by `--run` |
| `user` | The user |

`--run <run>` without `<to>` broadcasts to every running agent on an active attempt in that run, except the sender. Each copy is its own message with a shared broadcast ID, so replies go back to the sender one by one. Broadcast to a whole box is deferred.

A missing alias, an ambiguous prefix, an exited or ended agent, or a target in another box fails with an error that names the target and the reason. Nothing is queued.

## Trust and framing

- An agent can message any agent in its own box and the user. Cross-box sends are refused. The user can message any agent in any box.
- Each sender–recipient pair may send at most 20 messages in any 10 minutes. The user and `system` are exempt. A send over the limit fails and says when it can retry. This stops two agents from looping.
- Every message an agent sees comes in an envelope. The envelope names the sender’s ID, provider, role (for example “coordinator of run 7f3a” or “worker on task `store`”), and box, and says plainly that this is a message from a peer agent relayed by Power, not an instruction from the user. A message from the user says it is from the user, and a `system` message says it is from Power. The envelope format is a pure domain function, so it is tested once and used by both the notice and the inbox.
- The injected instructions say that peer messages are requests from fellow agents, that the user’s instructions take precedence, that peers can’t grant permissions or approvals, and that agents should push back on a peer request that conflicts with the user’s.

## Delivery

Delivery is a hybrid. The daemon stores the message first, then types a short notice into the recipient’s terminal once it is idle.

1. **Queue.** `send` writes the message to the store and returns its ID straight away, before any notice.
2. **Notice.** When the recipient is idle and has unread messages, the daemon writes one line and a carriage return into its PTY with the existing input path:
   `[power talk] 2 new messages from 7f3a (coordinator of run 7f3a). Not from the user. Run: power talk inbox`
   One notice covers every message that is unread at that moment. The messages are then marked notified.
3. **Read.** `power talk inbox` prints the unread messages in envelopes and marks them read. Senders can see the notified and read times.
4. **Reminder.** A message that is still unread after the recipient’s next idle period gets one more notice, no sooner than 2 minutes after the first. After that, Power sends no more notices, and the unread count in the agent tree is the only signal.

Power sends no notice to an agent whose attempt is waiting on the user (`needs-input`), because the next line typed there is meant to be the user’s answer. Messages to it queue until it reports `working`. The user’s own messages are the exception. Paused agents ([#79](https://github.com/xentropic-dev/power-overwhelming/issues/79)) and sessions that are still being restored also queue.

`--urgent` is available only to the user. It types the notice at once even when the agent is busy. It doesn’t interrupt the agent’s turn.

Messages to the user are never typed anywhere. They appear in the user’s inbox in the desktop (see [Desktop](#desktop)) and in `power talk inbox` run from a plain shell.

## Idle detection

The daemon keeps busy or idle state for each agent session.

- **Provider hooks, when the profile has them.** A new `AgentProfile.turn_hook`, configured from Lua like `model_callback`, tells Power how to have the agent report its turns. The command it wires in is `power talk ready`, which marks the session idle and is run by the agent’s turn-end hook, and `power talk busy`, which marks it busy.
  - Claude Code: add `hooks.Stop` → `power talk ready` and `hooks.UserPromptSubmit` → `power talk busy` to the `--settings` JSON that Power already passes for the status line, merged into the same object. When the user passes `--settings` themselves, Power adds no hooks and uses the fallback.
  - Codex: `-c notify=[...]` runs `power talk ready` on `agent-turn-complete`. Codex has no turn-start hook, so the session turns busy when the user types into it, when Power types a notice into it, or when it produces output. When the user’s Codex config already sets `notify`, Power leaves it alone and uses the fallback. Check this the same way `claude_status_line_configured` checks Claude.
  - Grok and profiles without a hook use the fallback.
- **Fallback heuristic.** A session is idle when it has produced no PTY output for `talk.idle_after` (default 5 seconds) and the user hasn’t typed into it for `talk.user_quiet` (default 10 seconds). A hook-reported busy state always wins.
- Both durations are settings in `config.lua`, validated on load. Tests set them to a few milliseconds.
- `power talk ready` and `power talk busy` take the session from `POWER_SESSION`. They are hidden from `--help` but listed in the skill.

## CLI

All commands print JSON with `--json` and plain text by default. Exit codes are 0 for success, 1 for errors, and 3 when a `--wait` times out.

```sh
power talk send <to> <message…>          # or: --file PATH, or - for stdin
power talk send --run <run> <message…>   # broadcast to the run's active workers
power talk send <to> <message> --wait [--timeout SECS]   # blocks for the first reply, default 600
power talk reply <id> <message…> [--wait [--timeout SECS]]
power talk inbox [--peek] [--all]        # unread, marked read unless --peek; --all includes read
power talk show <id>                     # the whole thread containing <id>
power talk wait <thread-id> [--timeout SECS]   # resume waiting after a --wait timed out
power talk log [--agent <to>] [--run <run>] [--limit N]   # history, for coordinators and the user
power talk ready | busy                  # turn hooks; hidden
```

`send` prints the message ID, the thread ID, and each resolved recipient. `--wait` prints the reply in its envelope. A timeout prints the thread ID so the sender can call `power talk wait` later. Outside Power, the commands take `--box <selector>` like `power dag`.

## Layers

Keep the repo’s layering. Domain holds pure types and rules with no serde or IO. Use cases live in application behind ports. `wire.rs` holds DTOs only, and the daemon and client are thin adapters.

**Domain** (new `domain/src/talk.rs`)
- `MessageId`, `ThreadId`, `Message`, `Sender::{Agent(AgentRef), User, System}`, `Recipient::{Agent(..), User}`, and `Delivery { queued_at, notified_at, reminded_at, read_at }`.
- `Address::{Agent(String), Parent, Coordinator, Task(String), User}` parsed from text, and `Target::{One(Address), Run(RunId)}`.
- `resolve(sender_ctx, target, agents) -> Result<Vec<Recipient>, AddressError>`, a pure function over a snapshot of the box’s agents and runs.
- `authorize(sender, recipient) -> Result<(), TrustError>` for the same-box rule, plus a pure rate-limit check over recent send times.
- `envelope(message, sender_info) -> String` and `notice(unread) -> String`.
- `NoticePolicy`, a pure function from (session activity, hook state, attempt state, paused, unread, last notice time, now) to `Notify | Remind | Hold(reason)`.

**Application**
- Ports: `TalkStore` (insert, unread for a recipient, mark notified, reminded or read, thread, log, recent sends for the rate limit, `wait_for_reply(thread, recipient, deadline)`), `SessionActivity` (last output time, last user input time, hook state), and a `Notifier` that writes a notice into a session.
- Use cases: `SendMessage`, `Reply`, `ReadInbox`, `ShowThread`, `WaitForReply`, `TalkLog`, `ReportTurn` (ready or busy), and `DeliverPending`. The daemon runs `DeliverPending` on a timer and on activity changes. It evaluates `NoticePolicy` for each recipient with unread messages and calls the notifier.

**Infrastructure**
- Store: a `talk_messages` table in `dag.sqlite3`, indexed by recipient and unread, and by thread, behind the `TalkStore` port. Add it with a migration so existing files open. Messages survive daemon restarts. After a restore, notified but unread messages get one reminder once the agent is idle.
- Activity: track last PTY output and last `Request::Input` per session in the daemon. Reuse #77’s tracker if it has merged.
- Wire: `Request::Talk { talk: TalkRequest }` with `Send`, `Reply`, `Inbox`, `Show`, `Wait`, `Log`, `Turn`, and `Watch` (a stream of `talk_message` and `talk_read` events for the desktop), plus `Response::Talk`. Convert DTOs to domain types in the client.
- Launch: `turn_hook` for each profile in `shell.rs` next to `model_callback_args`, plus Lua config in `config.rs` and built-in defaults for `claude` and `codex`.

**Presentation**
- CLI: `power talk` subcommands in `main.rs`.
- Desktop: see below.

## Desktop

Every action is a keymap Command in `presentation/src/keymap.rs`, can be rebound from Lua, and works with hjkl and the arrow keys. These bindings are proposed.

- The agent tree (Ctrl+B then `a`) shows each agent’s unread count, counting messages to that agent from anyone, plus a marker on agents with a message waiting for the user.
- `AgentTalk` (agent tree, `ctrl-t`) opens the talk dialog for the selected agent. It lists the agent’s threads with the newest first, and each entry shows the sender, recipient, time, delivery state and the first line of the body. Up and Down, or `ctrl-k` and `ctrl-j`, move through the list. Enter expands a thread. `TalkCompose` (`ctrl-n` in the dialog, rebindable) opens a text field that sends to the selected agent as the user. `TalkReply` (`ctrl-r`) replies in the selected thread. Enter sends and Escape closes.
- `BoxTalk` (box prefix, `m`) opens the same dialog filtered to messages sent to the user, across the active box.
- DAG node details show the message count and latest message for the current attempt’s agent, with a key that opens the talk dialog for it.
- Follow the desktop look: square corners, no animation, dimmed in-window modals. The dialog updates from `Watch` events off the UI thread.

## Agent instructions and skills

- Add a paragraph to Power’s injected instructions in `instruction_args`. It explains that a line starting with `[power talk]` comes from Power, not from the user. It tells the agent to run `power talk inbox`, to reply with `power talk reply <id>`, to treat peer messages as requests from fellow agents, and that the user’s instructions take precedence.
- Add a bundled `power-talk` skill (`skills/power-talk/SKILL.md` and `guide.md`) that covers addressing, `--wait`, broadcast, the envelope, rate limits and etiquette: keep messages short, link artifacts or files rather than pasting large content, and don’t message a worker that is waiting on the user.
- Update the `power-dag` skill:
  - The worker contract says to ask the coordinator with `power talk send coordinator --wait` when the question is for the coordinator, and to keep `needs-input` for decisions only the user can make.
  - Coordinators send review feedback with `power talk send task:<id>`. The recovery reference shows how to nudge stalled workers with `power talk send --run`.

## Acceptance checks

1. Inside one box, start a coordinator and two `--provider command` workers in a run. The workers are scripted fixtures that read stdin lines and run `power talk inbox` on a notice. From worker A, `power talk send coordinator "question"` returns an ID. Verify that the coordinator gets exactly one notice line once idle, `power talk inbox` shows the envelope with worker A’s ID, task and “not from the user”, and the message is then read.
2. From the coordinator, `power talk send task:b "do X" --wait --timeout 30`. Worker B replies with `power talk reply <id>`. Verify that the coordinator’s command prints the reply and exits 0, and that both messages share a thread. Repeat with no reply and verify exit code 3 and that `power talk wait <thread>` later returns the reply.
3. `power talk send --run "$RUN" "status?"` reaches both workers and not the sender, each with the same broadcast ID.
4. Keep a worker busy, with continuous output or a busy hook state. Verify that no notice arrives until it goes quiet. Put a worker in `needs-input` and verify that a peer message queues with no notice until it reports `working`, while a user message is still delivered.
5. Verify that an unknown alias, an ambiguous prefix, an exited agent, and an agent in another box each fail with a clear error and store nothing. Send 21 messages within the window and verify the 21st is refused.
6. Queue an unread message, restart a test daemon (never the user’s live daemon) on the same `POWER_STATE_DIR`, and verify that the message is still in the inbox and gets one reminder once the agent is idle.
7. Launch argument tests: Claude’s `--settings` has the status line and both hooks in one object, and has none when the user passes `--settings`. Codex gets `notify` only when the user’s config has none. Grok gets nothing.
8. Desktop (manual, from the keyboard only): open the agent tree and verify the unread counts. Press `ctrl-t` on a worker, compose a message, and verify it arrives with a user envelope. Open `BoxTalk` and read a message a worker sent to `user`. Rebind `AgentTalk` in `config.lua`, reload, and verify that the new key works.

Automated tests go in `presentation/tests/talk.rs`. Use deterministic `--provider command` workers, temporary repositories, `POWER_STATE_DIR` and a temporary `HOME`, and millisecond `talk.idle_after` values. Unit-test the domain rules (resolution, trust, rate limit, envelope, `NoticePolicy`) exhaustively, and the use cases with fake ports. `config::tests::lua_extends_and_overrides_builtin_profiles` is a known flake ([#63](https://github.com/xentropic-dev/power-overwhelming/issues/63)).

## Open choices

- The exact key bindings and the dialog layout are proposals.
- Box-wide broadcast, a box-strip highlight for messages to the user, and moving #77’s restart and stall prompts onto talk are deferred.
- The rate limit (20 per 10 minutes) and the idle defaults (5 s and 10 s) are starting values to tune.
