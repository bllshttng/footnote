# Cross-agent bus log

> **Superseded.** This document records the Group 3/4 bus-epic substrate, where the markdown thread was the durable-first write and `fno inbox` / `fno agents send` were the messaging verbs. The canon is now flipped (the `messages.jsonl` log is the durable-first write; the markdown is a derived render, regenerable with `fno mail rebuild-render`) and messaging is one namespace, `fno mail` (`fno inbox` and `fno agents send` are retired). The verb names and write-order below are historical; the log + cursor substrate itself carries forward unchanged.

This document describes the canonical message-bus substrate shipped in Group 3 of the cross-agent message bus epic: the global JSONL log, per-agent cursors, the markdown render, `--to-project` anycast resolution, and the `fno inbox` alias. The full design (verb taxonomy, delivery tiers, liveness model) lives in the maintainers' vault; this doc is the shipped-substrate reference.

## Problem

Before this work, agent-to-agent messages and the cross-project inbox were two stores. `fno agents send`/`ask` wrote per-recipient markdown thread files via the inbox store, and "what A and B said" was scattered across per-recipient directories with provider-specific shapes. There was no single, provider-neutral, append-only transcript and no cursor model for "messages to me since I last looked."

## The substrate

One global append-only log is the system of record. The per-recipient markdown thread file is demoted to a render of that log (the `graph.json -> graph.md` pattern): regenerated on every mutation, carrying zero authority. Read-state is a per-consumer cursor, not a per-thread flag.

| Layer | Lives | Role |
| :--- | :--- | :--- |
| Registry (exists) | `~/.fno/agents/registry.json` | WHO: name, provider, session id, cwd, status. Sole addressing authority. |
| Bus log (this work) | `<bus_dir>/messages.jsonl` (+ rotated `.N`) | WHAT WAS SAID: canonical, append-only, provider-neutral transcript. |
| Cursors (this work) | `<bus_dir>/cursors/<name>.json` | Per-consumer read position, keyed by last-seen message-id. |
| Markdown render (demoted) | `<recipient>/inbox/*.md` | Obsidian-visible render of the log; no authority. |

`bus_dir` resolves via `fno.paths.bus_dir()`: `FNO_BUS_DIR` env override, else `<FNO_INBOX_ROOT>/.bus` when the inbox store is test-isolated (so every existing inbox test/smoke co-isolates the bus), else `config.paths.bus_dir`, else `state_dir()/bus` (default `~/.fno/bus/`).

### The envelope (versioned)

One JSON object per line (`fno.bus.log.Envelope`):

```json
{"v":1,"id":"msg-3f8f96","ts":"2026-06-07T19:51:32Z","thread":"msg-3f8f96","from":"alice","to":"bob","kind":"send","provider_from":"claude","provider_to":"codex","in_reply_to":"...","delivery":"hosted","meta":{...},"body":"..."}
```

`from`/`to` are the addresses (registry names, or a project name in `--to-project` durable mode). `provider_from`/`provider_to` are metadata-only transport/audit tags, never used for addressing. Reply correlation is `request_id`/`in_reply_to`, independent of provider tags. `meta` carries inbox passthrough (`refs`, `persist_to_memory`, `render_path`) so triage->graph provenance and Obsidian visibility survive without polluting the canonical address/correlation fields. Optional fields are omitted when unset so lines stay scannable; the body is written last. A root message threads under its own id (`thread == id`); a reply sets `in_reply_to`.

Serialization is single-source: only the Python CLI writes `messages.jsonl` (the Rust daemon does PTY *delivery*, not envelope appends), so there is no Python/Rust byte-divergence to reconcile on this surface.

### Write discipline

`append()` takes an `flock` on a sidecar lockfile (`messages.jsonl.lock`), checks/performs rotation under that lock, then writes the whole line with `O_APPEND`. `O_APPEND` alone fixes the offset race but does not make a multi-KB line atomic on a regular file (the POSIX small-write guarantee is for pipes; the macOS threshold is tiny). Lock + `O_APPEND` is bulletproof at any body size and uncontended at agent-messaging rates. The lock serializes *writers*; lock-free readers may transiently miss the just-renamed `live -> .1` segment during a rotation, which the cursor fallback (below) covers. Rotation is size-triggered (`messages.jsonl -> .1 -> .2 ...`, default 5 MB, retain 5; `FNO_BUS_MAX_BYTES` / `FNO_BUS_RETAIN` override). The log is append-only: corrections and delivery-state changes are new envelopes, never edits.

### Reader

`iter_messages()` yields every retained envelope oldest -> newest across all segments, skipping a malformed line with a stderr warning (a corrupt line never aborts the scan; a genuine I/O error surfaces at the segment level with its own warning). `iter_thread(thread_id)` filters to one conversation.

### Cursors

Read/unread is a per-consumer cursor file keyed by the last-seen message-id, never a raw byte offset, so a rotation cannot silently reset a position. `scan_unread(name)` returns messages with `to == name` after the cursor; `advance_cursor(name, msg_id)` acks. Failure posture is fail-open toward never losing unprocessed mail:

- absent cursor -> scan from the start of retained segments (a never-seen peer still receives durable mail), not "from now";
- corrupt cursor -> treated as absent (rescan), with a warning;
- cursor id rotated out / unresolvable -> rescan retained segments (worst case: re-see old messages, deduped by sink idempotency).

## Read surface

`fno agents inbox [--name X] [--json]` is the cursor-filtered view ("my inbox" is a view over the one log, not a physical file). `fno agents ack <msg-id> [--name X]` advances the cursor; it refuses an id absent from the retained log (writing it would leave a cursor `scan_unread` can't find, silently re-surfacing all mail).

## Project-destination addressing (anycast)

Project/cwd is demoted from address to resolver. `fno agents send --to-project X <msg>` (and `ask --to-project`) resolves over the registry cwd->project mapping, with `config.inbox.peers.<name>.project` as an optional hint that degrades to empty (never raises) on a malformed config. The rule (`resolve_to_project`):

- exactly one live peer -> deliver live (the envelope records the resolved recipient);
- none -> durable queue addressed to the project (one bus line, picked up at that project's next drain);
- many -> error listing the live candidates, delivering to none, unless `--any` breaks the tie (most recent `last_message_at`, lexicographic name as the final tiebreak).

`ProjectResolution` enforces exactly-one-outcome at construction. `ask` is synchronous, so `ask --to-project` requires exactly one live peer (none/ambiguous is an error; use `send` for the durable path).

## `fno inbox` alias + legacy migration

`fno agents send` (with `fno agents send --to-project <project>` for project-destination anycast) routes through `write_new_thread`, which mirrors a canonical envelope into the bus on every write: one log line per send, the md render and the envelope agree (no md-store divergence), and the existing triage drain finds it. The mirror is best-effort (the md render is the durable copy); a mirror failure warns loudly on stderr because a bus reader would otherwise diverge from the md drain until backfill.

**Rollout / stale install:** during the Group 4 rollout a stale installed `fno` may still expose the removed `fno inbox send` verb, while a fresh install removes it and errors with a pointer to `fno agents send`. `fno doctor` is the staleness-detection surface (it probes for verb skew between the installed snapshot and the source), so if `fno doctor` reports stale, run `fno update` to pick up the `fno agents send` cutover.

`fno inbox migrate-bus` (and `migrate_md_threads_to_bus`) backfills markdown threads written before the bus existed (or by a stale pre-G3 `fno`) into the canonical log, so a cursor scan never strands unread legacy mail. It is idempotent (dedup by message-id), resilient (one unappendable message is counted in `MigrationResult.failed` and skipped, not aborting the batch), and never re-migrates threads already on the bus.

## What is NOT in this group

The triage drain (`fno inbox drain`: heads-up -> triage, question -> wake-signal, fyi -> memory/log) still reads the markdown render in Group 3; its rewiring to the bus cursor, the register-existing-session hooks, the internal call-site migration, and deleting the `fno inbox` alias are Group 4. One known follow-up: an owner-authored reply on `append_to_thread` currently mirrors with `to == thread-owner`; Group 4 must revisit that addressing when the drain reads the bus cursor.

## Addressed delivery (Group 1)

Same-project by-name delivery was the unfinished half of the bus: `fno agents send <name>` mirrored an envelope addressed `to == <name>`, but the loop-boundary nudge only drained `to == project`, so a worker never surfaced mail addressed to it by name. This group closes that on the existing global bus (the deliberate "Option A" choice over a per-project file; the design doc in the maintainers' vault records the Execution Decision Revision for why per-project was revisited).

**Envelope enrichment (additive).** Three optional fields join the envelope, omitted when unset so pre-existing lines serialize byte-identically and old lines still parse (pinned by a byte-identical test):

- `from_session` - the sender's session id, the audit/robustness key for sender-exclusion on a broadcast.
- `from_model` - the sender's model, reserved for the render. No truthful source exists in `AgentEntry` today, so no producer sets it yet; the field is forward-compat room, not fabricated.
- `to_kind` - the addressing discriminator: `name | session | project`. `fno agents send <name>` sets `name`; the `--to-project` durable path sets `project`.

**Sender-exclusion.** `scan_unread(name, *, exclude_from=...)` drops messages whose `from`/`from_session` is in `exclude_from`. By-name reads pass nothing (a direct address is never a self-echo); the project-broadcast read excludes the worker itself so it never drains its own broadcast back. The load-bearing exclusion key is the sender name (always present); `from_session` is secondary.

**Loop-boundary drain.** `peek_nudge` now drains the union of (a) by-name mail to this worker and (b) project broadcasts not sent by it, restoring global oldest-first order across the two cursor-bounded scans. The worker's own registry name is resolved best-effort from its unique live cwd (`_resolve_self_name`); zero-or-many live entries at one cwd degrade to project-only delivery rather than guess. The Rust `nudge.rs` is unchanged: it already shells out to the Python `fno agents nudge-peek`, so the fix lands entirely in the Python it calls.

**Projection.** `fno inbox view [--from P] [--all] [--json] [-n N]` renders the bus log (the source of record) read-only, surfacing the enriched fields when present and ignoring unknown fields (forward-compatible). It is project-scoped by default (traffic to/from the project or an agent in it) so a cross-project body is not leaked; `--all` is the explicit operator view. The bus log file is created `0o600` (it holds message bodies; create-only, so pre-existing logs keep their prior mode).

## Code map

| Concern | Location |
| :--- | :--- |
| Envelope + locked writer + rotation + reader | `cli/src/fno/bus/log.py` |
| Per-agent cursors + sender-exclusion (`exclude_from`) | `cli/src/fno/bus/cursor.py` |
| Loop-boundary addressed drain + self-name resolver | `cli/src/fno/agents/nudge.py` |
| `fno inbox view` projection | `cli/src/fno/inbox/cli.py` |
| `bus_dir` resolver | `cli/src/fno/paths.py`, `config.paths.bus_dir` |
| Store dual-write + migration | `cli/src/fno/inbox/store.py` |
| `--to-project` resolver + dispatch | `cli/src/fno/agents/dispatch.py` |
| `inbox` / `ack` / `send --to-project` verbs | `cli/src/fno/agents/cli.py` |
| `migrate-bus` verb | `cli/src/fno/inbox/cli.py` |
| peer-project hint reader | `cli/src/fno/inbox/settings.py` |

See also [cross-project-inbox.md](cross-project-inbox.md) (the inbox this converges) and [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md) (the addressing authority).
