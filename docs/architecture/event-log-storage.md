# Event log storage: events.db is the only authoritative store

One SQLite database (`events.db`) sits beside each resolved journal path
(project, global, agents lifecycle). Every writer in every language commits
through the `fno-event-store` crate's single transaction, and every reader
queries committed rows in commit order. The JSONL files are legacy import
sources and explicit export targets only; nothing treats them as authoritative.

## Schema (user_version 2)

The `events` table is the ordered record:

| column | meaning |
|---|---|
| `seq` | commit order (the read order for every consumer) |
| `event_id` | stable identity. Producer-supplied ids win; otherwise `evt:<sha256(line)>`, so a byte-identical retry reads back as an idempotent hit |
| `row_hash` | sha256 of the canonical line; the legacy-import dedupe key |
| `ts_ms`, `type`, `source`, `scope` | parsed envelope fields |
| `retention_class` | `durable` (default), `gate`, or `ephemeral`, from the schema's per-type retention |
| `session_id`, `node_id`, `pr_number`, `head_sha`, `repo` | identity columns extracted from `data` for gate queries (`node` and `pr` spellings accepted) |
| `reject_reason` | why an imported line failed validation; the line stays queryable verbatim |
| `line` | the canonical envelope, byte-for-byte |

A pre-`seq` database (v1) migrates in place, in one transaction, oldest
first, with `event_id = legacy:<sha256>`. A crash mid-migration rolls back:
`user_version`, row counts, and completion metadata are untouched, and the
retry imports zero duplicates (row-hash dedupe).

## Commit is the acknowledgement boundary

`append` runs one `BEGIN IMMEDIATE` transaction on a WAL store with
`synchronous=FULL` and a busy timeout, then reads the row back by
`event_id` before returning a receipt. A lost client reply recovers by
re-appending the same envelope: identical bytes read back as
`inserted: false`, different bytes under a supplied id are an identity
collision. A failed commit never falls back to a file write.

## Retention

`durable` and `gate` rows never auto-expire. `ephemeral` rows leave at the
schema floor (`retention.minimum_ephemeral_ttl_hours`, currently 672) in
bounded deletes. Rejected and migration rows never expire; an explicit
operator deletion is the only other removal.

## Ownership boundaries

`events.db` owns event facts only:

- graph nodes, decisions, claims, and relations stay in graph.db
- questions stay with the inbox
- mail messages, receipts, and cursors stay with the bus
- run and cost ledger rows stay with the ledger
- plan prose, vendor transcripts, and OS sockets stay files

Projections may join these owners by stable id and may not copy them into
the event schema or fall back to scanning them.

## Import and export

The importer walks every retained journal generation oldest-first, then the
live file, then the ephemeral sibling, in one transaction per pass, keyed by
row hash, so replays are free and a crash mid-import loses nothing. `fno
doctor event export` writes the JSONL snapshot atomically from committed
rows; an export is a snapshot for downgrades and audits, never re-ingested
as new identity.

## Grammar

- writers: `fno.events.append_event` (Python), `EventEmitter` and
  `append_event_line` (Rust), `_append_bounded_event` (shell) - all commit
  through the native store
- readers: `fno.events.store_client` (Python), `query_events` and `journal_text` (Rust)
- operator surface: `fno doctor event emit|find|audit|gc|export`
- pre-runtime only: `hook-events.jsonl` stays a bounded diagnostic for
  failures before the native binary can answer; no gate reads it
