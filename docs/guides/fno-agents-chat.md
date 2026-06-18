# How-to: open a live channel between two agents with `fno agents chat`

`fno agents chat A B "<seed>"` is the one **costed** verb in the agents surface. Where [`send`](fno-agents-send.md) drops an addressed message a peer drains at its next loop boundary, `chat` opens a **live, real-time** channel between two claude workers: it adopts both onto the shipped stream-json switchboard lane and drives a bounded A↔B relay synchronously, right now. Every hop spends Agent SDK plan credit (isolated from your interactive subscription), so `chat` ALWAYS confirms before it launches.

Use this guide when you want two existing claude sessions to converse in real time - e.g. compare two workers' approaches to the same problem - rather than exchange one async message. v1 is **claude↔claude only**; cross-provider chat is a tracked follow-on.

## Prerequisites

- `fno` CLI installed and the compiled `fno-agents` daemon running (`fno agents restart` after an `fno update`). `chat` drives the daemon's `agent.spawn` (stream-lane adopt) and `agent.switchboard` RPCs - both shipped substrate.
- Both peers already registered (via `fno agents spawn`) and **settled** - a session whose `--bg` loop is still actively running is refused (see "When chat refuses", below).
- Each peer needs a resolved full session UUID (captured best-effort at spawn). A peer with no UUID cannot be live-escalated - see "When chat fails".

## Basic chat

```bash
fno agents chat tgt-core tgt-g5 "compare your approaches to the doctor rewire"
```

`chat` first echoes the exact command and the plan-credit caveat, then gates on `[y/N]` (always - regardless of `config.agents.confirm`). On a non-interactive shell with no `--yes`, it refuses rather than launch a billed channel silently:

```
$ fno agents chat tgt-core tgt-g5 'compare your approaches to the doctor rewire'
note: chat opens a live stream-json channel: every hop spends Agent SDK plan credit (isolated from your interactive subscription).
Open this billed live channel? [y/N]
```

Pass `-y` / `--yes` to skip the gate when you have already confirmed (the caveat still prints):

```bash
fno agents chat tgt-core tgt-g5 "..." --yes
```

## What it does

1. **Adopts both peers** onto the stream-json lane under fresh host names `tgt-core-chat` and `tgt-g5-chat`, each keyed by the peer's full resume UUID. The daemon refuses adopting under a name already in the registry and claude has no fresh stream host, so the adopt resumes the peer's transcript into a new headless stream thread - that thread *is* the peer's conversation, resumed.
2. **Drives the seed** from A's host into B's host, mirrors B's reply back into A, and lets the relay alternate up to `config.agents.a2a.turn_ceiling` (default 6). With `config.agents.a2a.auto = false` it is a single mirrored hop with no autonomous relay.
3. **Reports the terminal state** on one stdout line:

```
chat tgt-core<->tgt-g5: 6/6 turns over [tgt-core-chat, tgt-g5-chat] (observe: fno agents watch tgt-g5-chat)
```

The channel is **headless** - follow it with [`fno agents watch`](fno-agents-list-logs.md), never a TUI.

## When chat refuses (exit 1)

```
chat refused: tgt-core is busy (running loop), cannot open a live channel; observe it with: fno agents watch tgt-core
```

A peer whose `--bg` /target loop is actively running cannot be adopted - resuming its transcript into a second writer would corrupt it (single-writer invariant). Let the loop settle, or just `watch` it. The daemon's atomic `session:<uuid>` claim is the authoritative guard; this fast refusal is the friendly pre-check.

## When chat fails (exit 1)

`chat` never reports a phantom channel. It fails loud, with the specific reason, on:

- **Unknown peer** - `chat failed: unknown agent 'ghost'; spawn it first`.
- **Self-chat** - `chat failed: cannot chat an agent with itself; A and B must differ`.
- **No resolved UUID** - `chat failed: no resolved session UUID for tgt-core; cannot open a live channel (claude has no fresh stream host). Re-spawn to capture the UUID, or use 'fno mail send tgt-core' (the async bus).` There is no "guess a UUID" path - re-spawn the peer (which captures the UUID) or fall back to `send`.
- **Dead adopt child / undelivered seed** - the `--resume` child failed to come up, or the seed turn was not delivered. `chat` reports the reason and best-effort **unwinds** the side it already adopted (it never tears down a *reused* pre-existing channel). An unwind the daemon could not confirm is reported as "may still be a live billed channel" with a `fno agents list` / `fno agents stop` hint - never asserted torn down.

## chat vs send vs ask

| Verb | Latency | Cost | Use when |
|------|---------|------|----------|
| [`send`](fno-agents-send.md) | recipient's next loop boundary | free (bus) | hand off work, fire-and-forget, recipient may be offline |
| [`ask`](fno-agents-ask-followup.md) | synchronous follow-up | free | a one-shot question to an existing peer |
| `chat` | real-time relay, now | **Agent SDK plan credit / hop** | two workers should converse live, bounded by the turn ceiling |

Reach for `chat` only when you genuinely want a live exchange - the free addressed bus (`send`) is the default claude↔claude channel.

## Configuration

- `config.agents.a2a.turn_ceiling` (default 6) - the hard bound on relay turns. A reached ceiling ends the relay with a visible "loop ceiling reached" note.
- `config.agents.a2a.auto` (default off-on-malformed) - when false, `chat` is a single mirrored hop; when true, the bounded autonomous relay runs. The first autonomous use confirms once (see [live-session-comms.md](../architecture/live-session-comms.md)).

See [docs/provider-command-matrix.md](../provider-command-matrix.md) for the per-provider support row and [the agent skill](../../skills/agent/SKILL.md) for the full verb router.
