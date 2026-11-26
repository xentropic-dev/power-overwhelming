# Power talk decisions

## Decision

Power owns agent-to-agent messaging. `power talk` stores each message in the daemon’s durable store, then types a one-line notice into the recipient’s terminal when it is idle. The recipient reads the message with `power talk inbox`. Senders name a recipient by agent ID or by a role resolved from their own context (`parent`, `coordinator`, `task:<id>`, `user`), or broadcast to a run’s workers with `--run`. Messages have IDs and threads. `power talk send --wait` blocks until a reply arrives. Messages to busy agents queue. Idle comes from provider turn hooks that Power wires in (Claude Code’s Stop hook and Codex’s `notify`), with an output-quiet heuristic for profiles that have no hook. Messages survive daemon restarts and appear in the agent tree, DAG node details and a keyboard-driven talk dialog, where the user can also send them. Agents can message any agent in their own box. Every message arrives in an envelope that marks it as coming from a peer, not from the user. Restart and stall prompts stay where they are for now. [Requirements](requirements.md) · Tracking issue [#78](https://github.com/xentropic-dev/power-overwhelming/issues/78)

The user chose the recommended option for all seven open questions on 2026-11-26.

## Alternatives considered

- **Delivery.** Typing the whole message into the target’s PTY works for every provider today. It has no queue and no read state, and it corrupts a prompt the user is halfway through typing. An inbox with no typed notice leaves an idle agent unaware of a message until something else wakes it. The hybrid keeps the store as the source of truth and types only one short line, and only when the agent is idle.
- **Addressing.** Agent IDs alone are simple, but they make every worker look up its coordinator, which is exactly the mapping that failed in #47. A full set with names that agents pick themselves and box-wide broadcast adds names Power would have to keep unique and a broadcast nobody has needed yet. Role aliases cover the coordinator–worker and parent–child traffic that real runs need, and run broadcast covers “nudge every worker”.
- **Replies.** Fire-and-forget makes a coordinator poll and match replies by hand. Threads without blocking still make it poll. Threads with `--wait`, plus `power talk wait` to pick up after a timeout, give a request–reply call that behaves the same for every provider.
- **Busy targets.** Using only the heuristic is simpler, but it misreads agents that think for a long time without output. Making agents run `power talk ready` themselves is deterministic, but it depends on agents remembering to. Provider hooks are reliable where they exist, the heuristic covers the rest, and a hook’s busy state overrides the heuristic. Interrupting a busy agent is limited to the user’s `--urgent`, which types the notice at once but doesn’t cancel the turn.
- **Persistence and visibility.** An in-memory queue would lose messages in exactly the restart that motivated this feature. A CLI-only first cut would leave the user unable to see what coordinators and workers are telling each other. The plan persists messages in `dag.sqlite3` and ships the agent tree counts and the talk dialog together.
- **Trust.** Allowing messages only along delegation edges blocks reviewer–author and peer conversations within a box. Allowing anyone to message anyone across boxes lets an agent steer work in an unrelated repository. Same-box sends with a peer envelope, a rate limit and injected precedence rules fit how boxes already group work. The envelope and the rules don’t make this a security boundary. Every agent runs as the same OS user.
- **Restart recovery.** #72 already prompts restored workers, and #77 is adding a restart guard and stall detection. Sending those prompts through talk in this graph would conflict with work in flight. Talk ships a `system` sender, so that migration is a small follow-up once both have merged.

## Rationale

Coordinators in #47 couldn’t reach Codex workers and sent Claude messages to the wrong sessions, because messaging lived in one provider and used that provider’s names. Moving it into the daemon, which already knows every agent’s box, parent, run, task and session, makes addressing correct by construction and makes delivery the same for every provider. A durable store plus a notice typed only when the agent is idle avoids corrupting prompts and gives senders read state. The envelope and the injected rules keep peer requests from being mistaken for the user’s instructions.

## Open choices

- Key bindings and the talk dialog layout are proposed in the requirements.
- Box-wide broadcast and a box-strip highlight for messages to the user are deferred.
- The rate limit and idle defaults are starting values.
- Moving the #72 and #77 recovery prompts onto talk is a follow-up.
