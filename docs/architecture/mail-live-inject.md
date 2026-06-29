# Architecture: live-inject-first a2a delivery and the `<fno_mail>` envelope

`fno mail send` delivers an agent-to-agent message to a LIVE recipient first and queues the durable bus only as a fallback. This is the unification (node x-1f23, epic x-07c1) that sits on top of the G1 substrate (node x-26df): one send interface, one wire envelope, one live primitive per provider, with the durable bus demoted from a peer system to the offline pending-queue.

## The delivery model

`dispatch_send` (`cli/src/fno/agents/dispatch.py`) runs, in order:

1. Capture the sender provenance (short sessionId, provider, model) for the envelope.
2. If the recipient is `live`, attempt a single fire-and-forget live delivery via `_deliver_live`. On success the result is `hosted` and NOTHING is written to the durable bus.
3. Otherwise (offline recipient, or the live attempt did not confirm), write the durable envelope. That copy is the pending-queue an offline recipient drains on wake, and the recovery record when a live inject did not land.

The per-agent flock that `dispatch_send` already holds serializes concurrent sends to the same recipient, so two sends never interleave their envelopes.

### `_deliver_live` transports

Every live transport carries the same `<fno_mail>`-wrapped turn:

- **claude** (adopted `claude --bg`): the proven `control.sock` `op:'reply'` inject, reached through the `fno-agents mail-inject` verb (see below). The stream-json switchboard and MCP-channel fast lanes still apply first for peers that are live stream threads or MCP-routed; the `control.sock` inject is the successor to the dead per-worker messaging socket.
- **codex / gemini**: the daemon `agent.deliver` RPC, now carrying the `<fno_mail>` envelope (see [fno-agents-deliver-gate.md](fno-agents-deliver-gate.md)).

`_deliver_live` returns `True` on the first lane that succeeds, else `False`; `dispatch_send` writes durable exactly when it returns `False`. So a message takes exactly one of {one live transport, durable}.

## The `<fno_mail>` envelope

The wire format is locked in Rust (`crates/fno-agents/src/claude_drive.rs`) and rendered once in Python (`cli/src/fno/mail/envelope.py`); `test_fno_mail_envelope.py` pins the Python renderer to the Rust bytes so the two never drift.

```
<fno_mail from="<short-sid>" harness="<harness>" model="<model>"[ node="<id>"][ to="<short-sid>"]>
message text
</fno_mail>
```

`from` is the sender's short 8-hex sessionId (the identity). `harness` maps the provider to a reply vocabulary (`claude` -> `claude-code`; `codex`/`gemini` unchanged) via `harness_for_provider`. `node`/`to` are optional. A delivered turn is self-recording: it lands in both transcripts, so `grep <fno_mail>` across transcripts reconstructs the a2a history. That is what lets the durable bus stop being the history store and become only the offline pending-queue.

## Where the envelope lands: per-harness transcript map

A live `hosted` delivery puts the `<fno_mail>` turn into the recipient's session transcript by construction: the claude path injects it over `control.sock` and the `mail-inject` verb only reports `delivered` once that transcript grew; the codex/gemini path types it into the PTY worker, which the session records. So `grep <fno_mail>` reconstructs delivered a2a history, but you have to know where each harness keeps its transcript:

| Harness | Provider-native transcript | Notes |
|---------|----------------------------|-------|
| claude | `~/.claude/projects/<cwd-encoded>/<session_uuid>.jsonl` | The filename IS the full session uuid, so `find_transcript` globs `<uuid>.jsonl` across project dirs (sidestepping the lossy cwd-encoding). Override the base with `FNO_CLAUDE_PROJECTS_DIR`. |
| codex | `~/.codex/sessions/` rollout JSONL, indexed by `~/.codex/session_index.jsonl` | The index maps session uuids to rollout files. |
| gemini | `~/.gemini/tmp/<cwd-basename>/chats/` | Per-cwd chats dir; gemini pins a session to its cwd. |

Provider-agnostic fallback: footnote tees each spawned agent's I/O to a per-agent `output.jsonl` (the registry entry's `log_path`, surfaced by `fno agents logs <name>`). Because footnote owns that file's format and it captures both injected input and the model's output, it is the most uniform `grep <fno_mail>` target across all three harnesses. A durable (offline) message has no transcript yet; its `<fno_mail>` body lives in the bus log until drained.

## The `mail-inject` verb

`fno-agents mail-inject --session <uuid|short>` (`crates/fno-agents/src/mail_inject.rs`) is the one-shot claude live primitive `_deliver_live` shells out to. It reads the turn text from STDIN (sidestepping the argv size limit), resolves the recipient on the daemon roster, attaches to its `control.sock`, `op:'reply'`-injects the text verbatim, and confirms delivery by transcript GROWTH. It prints `{"delivered": bool, "reason": str}` and exits 0 when delivered. Every not-delivered reason (`not-live`, `no-transcript`, `attach-failed`, `not-confirmed`, ...) is a clean signal for Python to write the durable fallback.

It is binary-direct (a Python subprocess), NOT a routable `fno agents` verb: it is dispatched via `matches!` in `bin/client.rs` like `version`/`--emit-schema`, so it stays out of the verb-parity lists (`RUST_CLIENT_VERBS` / `CLIENT_VERB_USAGE`).

### Confirm-by-growth is best-effort

The verb confirms that the recipient transcript grew after the inject, i.e. the injected USER turn was recorded. For the target case (a session idle or blocked at a prompt) the turn records promptly, so growth fires within a poll interval. Two bounded edges remain:

- A BUSY recipient (mid tool call) queues the injected turn; if it is not recorded within the poll budget the verb reports `not-confirmed`, Python writes the durable fallback, yet the queued inject still lands later. That is a bounded DOUBLE delivery.
- Live-first writes durable only after a failed live attempt, so a process kill during the up-to-20s live window loses the message, a window the old durable-first did not have.

Both are accepted tradeoffs of live-inject-first. Hard exactly-once would carry the `msg_id` in the envelope and dedup at the recipient's drain (a follow-up, not built here).

## The relay variant

The cross-session relay PTY hop (`cli/src/fno/relay/envelope.py`) frames provenance on the same `<fno_mail>` tag, but as the SINGLE-LINE, no-close transport variant: `<fno_mail from="..." harness="..."[ model="..."]> <one-line body>`. It cannot carry the paired multiline form because the PTY Enter submits on newline. It shares the tag name and `harness` vocabulary so `grep <fno_mail>` reconstructs relay hops too.

### Cross-harness hop (G4, x-3f34)

The relay graph spans harnesses (claude -> codex, codex -> gemini, ...), not just claude<->claude. The hop splits into two halves with opposite generality:

- **Injection is harness-agnostic.** Every cross-harness turn lands via the daemon `worker.submit` RPC (raw text -> settle -> CR into the recipient's owned PTY), the same vehicle claude uses. A live non-claude interactive worker is surfaced as a routable peer by `fno.relay.registry.index()` (keyed by `short_id`, with a `worker:<short_id>` inject handle); `fno.relay.daemon.daemon_deliver` routes that handle through `fno.relay.roundtrip.deliver_worker` with NO `session:` claim probe (that single-writer interlock is claude-transcript-specific; the live `worker.sock` is the routability signal, and the worker actor's single-socket write serializes injects).
- **Reply capture is NOT harness-agnostic.** It is a strategy resolved per harness behind one seam (`roundtrip.capture_replies`): claude reads its transcript jsonl (faithful), every other harness tails its PTY pane via the `worker.snapshot` RPC (the safe default), and a strategy that throws on schema drift degrades to pty-tail and emits `relay_capture_degraded` rather than zeroing the reply. Adding a harness is a `register_capture_strategy` entry (or nothing -> pty-tail default), never a new injection path. Provenance framing (`<fno_mail>`) and the G3 envelope (dedup, ttl, cycle-cut) are inherited unchanged; an unframed cross-harness inject is refused (`relay_dropped{unframed-cross-provider}`).

## Durable body carries the same envelope

The durable fallback stores the `<fno_mail>`-wrapped body, the SAME envelope the live path injects, so a delivered message carries one consistent wire form across the live and durable paths and `grep <fno_mail>` reconstructs durable history too (not just live transcripts). The wrapped body round-trips through the per-recipient markdown render unchanged, so `mark_thread_read` does not strip it. A consequence: `fno mail unread` summaries (`body.split("\n")[0]`) surface the open tag rather than a content preview; that tag is a recognizable a2a marker (it names the `from` sender), which is the point of keeping it legible.

## What did NOT change

`fno agents chat` (the costed real-time bidirectional stream) is a distinct mode and is unchanged. The durable bus persistence and its drain-on-wake semantics are unchanged; only its role narrows to the offline pending-queue. Deprecating the bus's separate history store (now redundant with transcript-grep) is left for later.
