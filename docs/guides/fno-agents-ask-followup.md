# How-to: send a follow-up message to a Claude agent

> Creation moved to `fno agents spawn` - see [fno-agents-spawn.md](fno-agents-spawn.md).

`fno agents ask <name> "<message>"` messages an EXISTING agent: it delivers the message to the running session and prints the recipient's reply on stdout. An unknown name errors with exit 16 ("spawn it first") - creation moved to `fno agents spawn` in the cross-agent bus epic (Group 1): `fno agents spawn <name> "<initial-prompt>" --provider claude` creates the `claude --bg` session and prints a compact JSON receipt carrying the supervisor's 8-hex `short_id`.

Use this guide when you have an orchestrator (script, LLM session, CI job) that needs to hand work to a long-running Claude agent and read back the response without managing the session id, socket, or attach lifecycle yourself.

## Prerequisites

- `fno` CLI installed (`uv tool install /path/to/footnote/cli` or via the footnote plugin postinstall).
- `claude` CLI 2.1.143+ on `$PATH`, signed in.
- A Claude `--bg` session created via an earlier `fno agents spawn <name> "<initial-prompt>" --provider claude`.

## Send a follow-up

```bash
fno agents ask frontend-worker "add zod validation to Login.tsx"
```

`stdout` receives the recipient's reply verbatim — no banner, no JSON wrapper, no trailing newline added by footnote. Pipe it freely:

```bash
fno agents ask frontend-worker "summarize the diff" | tee summary.txt
```

`stderr` is silent on success (modulo a `Waiting for agent '<name>' lock...` message if a concurrent caller is holding the per-agent flock for more than one second). Exit code 0 means the recipient transitioned to a terminal state and the reply was delivered to stdout.

## Identify yourself to the recipient (`--from-name`)

The default `--from-name` is `"footnote"`. When an LLM orchestrator wants to advertise its own identity in the envelope so the recipient knows who's asking:

```bash
fno agents ask frontend-worker "add validation" --from-name "orchestrator-main"
```

The recipient sees the message wrapped as `<cross-session-message from-name="orchestrator-main">`. Names containing `"`, `<`, `>`, or `&` are rejected with exit 2 to keep the envelope well-formed. Names longer than 128 characters are also rejected.

## Control the reply wait

`--timeout` caps the poll loop in seconds (default 600 = 10 minutes). The send is single-shot and synchronous; the timeout governs how long fno waits for the recipient's state.json to transition into a terminal state.

```bash
fno agents ask frontend-worker "quick yes-or-no" --timeout 30
```

Reaching the timeout returns exit 15 with the suggestion `Try 'fno agents logs <name>' to read the transcript.` The message was still delivered — at-least-once semantics, see "Failure modes" below.

## Failure modes

| Symptom | Exit | What happened | Recovery |
|---|---|---|---|
| `is not running (reason: not-found)` | 13 | The supervisor session for that agent is gone (process died, claude restarted) | `fno agents rm <name>` then start over |
| `is not running (reason: socket-null; session is suspended)` | 13 | Bg session auto-stopped (~1h idle). Socket path went null. | `claude attach <short-id>` to wake, then retry. Automatic wake is planned. |
| `is not running (reason: liveness-failed)` | 13 | Socket path exists but the 250 ms connect probe failed | `claude attach <short-id>` or `fno agents rm <name>` |
| `refusing to follow-up as provider=...` | 2 | `--provider X` disagrees with the registered provider | drop the `--provider` flag (it's only required on create) |
| `messaging socket error: ...` | 1 | AF_UNIX connect/write/close error mid-send | inspect with `fno agents logs <name>`; transient on heavy load |
| `message sent but no reply within <N>s` | 15 | Poll timeout. Recipient didn't transition to a terminal state. | `fno agents logs <name>` to read the transcript; the message WAS delivered |
| `registry write failed: ...` | 12 | Disk/permission error after the send succeeded | message delivered but registry inconsistent. **Do not retry** — manual cleanup needed via `fno agents rm` once cleared |
| `lock timeout for agent '<name>' after 30s` | 11 | Another `ask <name>` is holding the per-agent flock | wait or `fno agents logs <name>` to see what's running |

The exit codes are stable and distinct so LLM orchestrators can branch deterministically (`if [ $? -eq 13 ]; then ...`) instead of parsing prose.

## Delivery semantics

Once `fno agents ask` reaches the send step, the message **is delivered** to the recipient. Subsequent failures on fno's side — registry write, poll timeout, Ctrl-C — do **not** un-deliver it. Treat retries idempotently or accept that the recipient may see a duplicate.

The recipient frames the message with `<cross-session-message from-name="...">` semantics, which renders to claude's TUI as a "system notification, not user input". The recipient knows you are an orchestrator, not a user, and will reply in a more directed style.

## Inspect history

Use `claude logs <short-id>` directly, or `fno agents logs <name>`.

## See also

- [fno-agents-send.md](fno-agents-send.md) - async sibling: fire-and-forget delivery (live-inject first, durable only on a miss, no reply wait)
- [docs/architecture/fno-agents-followup.md](../architecture/fno-agents-followup.md) — substrate details, exit-code reference, schema migration
- [docs/architecture/fno-agents-registry-and-dispatch.md](../architecture/fno-agents-registry-and-dispatch.md) — registry storage + dispatch primitives
