# Cross-agent bus log

> **Superseded.** This document records the Group 3/4 bus-epic substrate, where the markdown thread was the durable-first write and `fno inbox` / `fno agents send` were the messaging verbs. The canon is now flipped (the `messages.jsonl` log is the durable-first write; the markdown is a derived render, regenerable with `fno agents mail rebuild-render`) and messaging is one namespace, `fno agents mail` (`fno inbox` and `fno agents send` are retired). The verb names and write-order below are historical; the log + cursor substrate itself carries forward unchanged.

This document describes the canonical message-bus substrate shipped in Group 3 of the cross-agent message bus epic: the global JSONL log, per-agent cursors, the markdown render, `--to-project` anycast resolution, and the `fno inbox` alias. The full design (verb taxonomy, delivery tiers, liveness model) lives in the maintainers' vault; this doc is the shipped-substrate reference.

## Problem

Before this work, agent-to-agent messages and the cross-project inbox were two stores. `fno agents send`/`ask` wrote per-recipient markdown thread files via the inbox store, and "what A and B said" was scattered across per-recipient directories with provider-specific shapes. There was no single, provider-neutral, append-only transcript and no cursor model for "messages to me since I last looked."

## The substrate

One global append-only log is the system of record. The per-recipient markdown thread file is demoted to a render of that log (the `graph.json -> graph.md` pattern): regenerated on every mutation, carrying zero authority. Read-state is a per-consumer cursor, not a per-thread flag.

| Layer | Lives | Role |
| :--- | :--- | :--- |
| Registry (exists) | `~/.fno/agents/registry.json` | WHO: name, provider, session id, cwd, status. Sole addressing authority. |
| Bus log (this work) | `<bus_dir>/messages.jsonl` (+ rotated `.N`) | WHAT WAS SAID: canonical, append-only, provider-neutral transcript. |
| Pair budget ledger | `<bus_dir>/word-budget/<pair-digest>.json` | WHAT MAY BE SENT NOW: lock-protected reservations for one canonical sender-recipient pair, pruned after 10 minutes or reset by an inbound authored message. |
| Cursors (this work) | `<bus_dir>/cursors/<name>.json` | Per-consumer read position, keyed by last-seen message-id. |
| Markdown render (demoted) | `<recipient>/inbox/*.md` | Obsidian-visible render of the log; no authority. |

`bus_dir` resolves via `fno.paths.bus_dir()`: `FNO_BUS_DIR` env override, else `<FNO_INBOX_ROOT>/.bus` when the inbox store is test-isolated (so every existing inbox test/smoke co-isolates the bus), else `config.paths.bus_dir`, else `state_dir()/bus` (default `~/.fno/bus/`).

### The envelope (versioned)

One JSON object per line (`fno.bus.log.Envelope`):

```json
{"v":1,"id":"msg-3f8f96","ts":"2026-06-07T19:51:32Z","thread":"msg-3f8f96","from":"alice","to":"bob","kind":"send","provider_from":"claude","provider_to":"codex","in_reply_to":"...","delivery":"hosted","word_count":17,"meta":{...},"body":"..."}
```

`from` and `to` are canonical registry names, session handles, or project names. `provider_from` and `provider_to` are audit tags, never addresses. Reply correlation uses `request_id` and `in_reply_to` independently of provider tags.

`word_count` stores the authored body's send-time count under the pure Rule 7 masking rules. It is never recomputed from a stored `<fno_mail>` wrapper.

`meta` carries `refs`, `persist_to_memory`, and `render_path` without polluting address or correlation fields. Optional fields are omitted so legacy rows parse and lines stay scannable. The body is written last.

A root message threads under its own id. A reply sets `in_reply_to`.

### Rolling sender-recipient word budget

Before any outward send effect, `fno.mail.budget.reserve` charges the authored word count to one canonical sender-recipient pair. The fixed policy is 80 masked words over a rolling 10-minute window.

An authored inbound `send`, `heads-up`, `question`, or `fyi` resets earlier reservations. Self-sends, migrations, withdrawals, and other non-authored rows do not reset them.

A refused or proven failed send releases its reservation. An unconfirmed or post-delivery audit failure stays charged until reset or expiry.

Pair files use a digest of the canonical addresses instead of an address-derived path. A sidecar lock serializes same-pair reservations. A temporary file and atomic replace persist the ledger before transport begins.

Different pairs do not share a lock. A malformed or unreadable active ledger fails closed instead of resetting the count.

A style exception or `FNO_STYLE_ENFORCE=0` bypasses refusal for that send. It still records and charges the real count.

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

## Crown-destination addressing (anycast over the crown)

`fno agents mail send --to-king <scope> <msg>` addresses the ROLE, not the session. The holder is resolved from the registry at SEND time. The resolver is `resolve_to_king` in `cli/src/fno/agents/crown.py`. It uses `crown_scope_matches`, the same territory rule the row-keyed king readers use.

Succession moves the crown row. It does not move the mail handle a peer learned while that handle was crowned. So a handle send after an abdication reaches the wrong session. Both failures are silent: the message was delivered, a session woke, and it answered.

The rule:

- exactly one live crowned row over that scope: deliver live to it.
- none: refuse, exit 16, queue nothing. A project queue has a future drain that reads it as that project. A vacant crown has no such reader. Queueing strands the message at the address.
- more than one: refuse, naming both holders. That is the split crown `fno agents court` already reports. It is not a multiplicity to pick between, so there is no `--any` tie-break here.

`resolve_to_king` returns the holder names as a plain list. A list has no illegal state to guard, so unlike `ProjectResolution` there is no construction-time check. The one caller reads the three outcomes off the length.

`--to-king` is exclusive with every other addressing mode: `--to-project`, `--to-self`, `--kind`, `--raw`, `--force`, `--any`, and a second positional. A second address decides where the message lands. The crown deciding that is the point.

A forwarding pointer written at abdication is the cheaper-looking fix, and it is refused on purpose. The pointer is itself a recorded identity. A second succession leaves it naming a session that is no longer crowned either.

## Recipient crown stamp

Every live-delivered envelope carries the RECIPIENT's own live crown, read at delivery from the same registry:

```
-- your crown: L1 fno
-- your crown: none right now
```

The line sits above the sender-standing trailer. The peer-mail authority notice stays the last line inside the envelope.

It is a trailer, not a tag attribute. The module's field rule reserves attributes for what a recipient cannot cheaply look up. A reader's own crown is exactly what it fails to look up.

Two gates, in order. With no resolved recipient session id, the envelope carries no line at all. `none right now` is a positive claim about the reader's authority, and an unresolved address is an absence rather than a reading. A crownless fleet (`fleet_has_crown()` false) carries no line either, so those envelopes stay byte-unchanged.

## Job-address lane

`fno agents mail send node:<id> <msg>` (or `pr:<n>`, normalized to `node:<id>` by the graph lookup) addresses the WORK, not the process. The lane lives in `cli/src/fno/mail/job_lane.py`.

A job address outlives its holder. That is the structural fix for the dead-handle strand. A session handle expired faster than the message. Mail to it then piled up on a queue the dead session never drained.

Two outcomes, matching the name lane's one-line stdout contract. There is no second delivery-verification receipt, because a receipt claiming delivery happened after the fact is the shape that has lied four times.

- A holder exists (claim live or suspect): live-inject to the holder's session. A confirmed inject IS delivery and writes no durable copy, the same as a hosted name-lane send. A live miss floors to a durable envelope addressed to `node:<id>`, so a successor drains it. The owner is `wake-daemon`. The holder exists and the inject missed, so the message waits for a drain rather than a turn boundary. A `node:<id>` thread surfaces at a holder's SessionStart scan, because the notify-self scan reads only the session's own handle. The receipt must not promise turn-boundary visibility.
- No holder (free, stale, corrupted, or no node): REFUSE, exit 16, queue nothing. Queueing strands the message at the job address, which is the defect again, one address over.

The inject targets the session id. Both `control.sock` and the codex daemon are keyed by it and cwd-independent, so a holder in another worktree is reachable from the sender's cwd. A bus-only holder is refused inside the injector, so the receipt names the policy rather than a miss.

## Name-lane address resolution

`_name_lane_send` in `cli/src/fno/mail/cli.py` is the one choke point every delivery rung lives in. Two rules there are easy to break by moving a line.

The durable copy must be addressed to the RESOLVED session's canonical handle. Deriving it from the raw token misaddresses every alias. A full session id is the collision escape hatch. It is written verbatim, never canonicalized. Two same-window codex sessions share their first eight characters, so canonicalizing a full id collapses both onto one durable key. `drain-self` reads the full id.

A non-id token, such as a spawn `--name` like `blueprint-auth-glm`, is not a mail address. The drain is handle-keyed, so a name never matches a session's handle and a durable write under it strands. `--force` is the exception, and for that same reason: it writes no durable row. It types at a pane the registry names, and the registry is what resolves a friendly name to the session behind it.

The codex head-8 refusal and the `--force` guard both sit ABOVE every lane that returns on its own. An address rule that covers only the lanes reached last is not an address rule. A dropped transport flag is worse than a refused one, because the receipt still reads like a success. Neither guard applies where the positional holds the message BODY. `--to-project` and `--to-king` address by option, so an eight-hex body there is content nobody is addressing.

## `fno inbox` alias + legacy migration

`fno agents send` (with `fno agents send --to-project <project>` for project-destination anycast) routes through `write_new_thread`, which mirrors a canonical envelope into the bus on every write: one log line per send, the md render and the envelope agree (no md-store divergence), and the existing triage drain finds it. The mirror is best-effort (the md render is the durable copy); a mirror failure warns loudly on stderr because a bus reader would otherwise diverge from the md drain until backfill.

**Rollout / stale install:** A stale installed `fno` can still expose the removed `fno inbox send` verb during Group 4. A fresh install removes it and points to `fno agents send`. `fno doctor` detects verb skew between the installed snapshot and source. If it reports stale, run `fno doctor update` to pick up the cutover.

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
