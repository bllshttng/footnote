# Event log storage: events.db is the only authoritative store

One SQLite database (`events.db`) serves each resolved journal path (project, global, agents lifecycle). State-root journals route through the layout table. Other journals retain sibling stores. Writers commit through the Rust `event_store` owner in `fno-agents`. The `fno` crate receives a generated copy. Readers query committed rows. JSONL files are legacy import sources and explicit export targets.

## Schema (user_version 2)

The `events` table is the ordered record:

| column | meaning |
|---|---|
| `seq` | commit order (the read order for every consumer) |
| `event_id` | stable identity. Producer-supplied ids win; otherwise `evt:<sha256(line)>`, so a byte-identical retry reads back as an idempotent hit |
| `row_hash` | sha256 of the canonical line; the legacy-import dedupe key |
| `ts_ms`, `type`, `source`, `scope` | parsed envelope fields |
| `retention_class` | `durable` (default), `gate`, `telemetry`, or `ephemeral`, from the schema's per-type retention |
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

`durable` and `gate` rows never auto-expire. `ephemeral` rows leave at the schema floor (`retention.minimum_ephemeral_ttl_hours`, currently 672) in bounded deletes. Rejected and migration rows never expire. An explicit operator deletion is the only other removal.

`telemetry` rows leave after `retention.telemetry_ttl_hours` (168). These are the high-volume readouts: `control_plane_tick`, `inside_leg_report`, `codex_thread_inside_leg`, and the two store-sweep unlink kinds. One of these rows is noise. The shape of many is the signal, and a week keeps enough to read it. The daily prune claims its pass under the write lock, so two syncs never prune at once. It deletes telemetry in 1,000-row batches for at most 3 seconds. The match is by kind, so rows stored as `durable` before a kind joined the class expire too. A pass that leaves a backlog runs again in 5 minutes.

`fno doctor event signals [--events <journal>] [--window-hours 24] [--check]` reads those shapes. It is read-only and flags four of them:

- `burst`: one type wrote 1,000 rows in one minute.
- `spike`: one type is at 5 times its prior daily average.
- `arm_errors`: an arm's tick detail reported a failure 10 times.
- `daemon_restarts`: the daemon started more than 12 times.

If any signal fires, `--check` exits 3.

## Poll coalescing and the coverage epoch

Declared poll kinds coalesce at both insert paths (`append_envelope` and `import_file`). `OBSERVATION_HEARTBEATS` declares `(guard_decision, 300s)` and `(advance_skipped, 1800s)`. A healthy poll is one whose subject, fingerprint, and heartbeat window match the last stored observation. It is counted pending in `event_observation_pending` instead of stored. A block, a malformed payload, an undeclared kind, and every heartbeat expiry store ordinary rows. When the window closes, one summary row carries the total as an explicit `occurrence_count` in its payload. The window closes on a fingerprint change, a heartbeat expiry, or the end-of-sync sweep. Readers sum `occurrence_count` (default 1) across matched rows to recover represented totals. Stored rows and represented occurrences are two counts, never merged.

Every open stamps `events_meta.coverage_complete_since_ms` once: the first moment this build observed the store. History proven complete starts there, never at `MIN(ts_ms)`. `coverage(journal, since_ms, types)` returns a receipt with a status. `unreadable` means a missing or unopenable store. `unknown` means no stamp, a store opened only by pre-epoch builds. `partial` means the requested `since` reaches before the proven start. `complete` means the `since` sits at or after it. The proven start is the epoch for durable and gate kinds. For ephemeral and telemetry kinds it is the later of the epoch and the last prune's cutoff for that class. A missing row before the proven start never reads as a confident zero.

## Ownership boundaries

`events.db` owns event facts only:

- graph nodes, decisions, claims, and relations stay in graph.db
- questions stay with the inbox
- mail messages, receipts, and cursors stay with the bus
- run and cost ledger rows stay with the ledger
- plan prose, vendor transcripts, and OS sockets stay files

Projections can join these owners by stable id. They must not copy them into the event schema or fall back to scanning them.

## Import and export

The importer walks every retained journal generation oldest-first, then the live file, then the ephemeral sibling. Each pass runs in one transaction and keys on row hash, so replays are free and a crash mid-import loses nothing. `fno doctor event export` writes the JSONL snapshot atomically from committed rows. An export is a snapshot for downgrades and audits, never re-ingested as new identity.

## Diagnose a malformed store on a copy

A journal path is an import anchor. Resolve its database with `fno doctor event rows --events <journal> --store-path-only` before interpreting a SQLite error. State-root journals can route to `db/events.db`. Agents journals retain their sibling store. Import SQL failures name the database and journal. History query failures name the database. File read failures continue to name the journal.

Copy the database and journal family into an isolated directory. When a WAL exists, copy it too. Record source sizes and modification times before and after copying. Discard a copy whose sources changed during capture. Run `PRAGMA integrity_check` and every recovery attempt on copies only. Retain an untouched copy before attempting `REINDEX`, logical reconstruction, or SQLite recovery.

Inspect tables with `SELECT * FROM <table> NOT INDEXED` to distinguish damaged indexes from damaged table pages. A readable events table does not prove a readable ingest cursor. The cursor records import progress. Before resetting it in a reconstruction, preserve every readable authoritative table, schema version, row identity, sequence, and observation state. Verify row counts, content digests, integrity, check-in readback, and repeated-import deduplication on the reconstructed copy. An unreadable authoritative table requires separate recovery assessment. Rebuilding from retained JSONL alone can lose committed history.

Readers refuse corruption. They never repair, replace, or silently rebuild a live store. A copy proof does not authorize installation of a repaired store or deployment of a worktree build.

Writers and importers run no integrity sweep on open: a scan walks every page of a healthy store, and the write-open door sits on the per-fire hook append path, so the sweep cost scaled with store size on every Bash turn. Page damage instead surfaces as a SQLite statement error when a statement touches the damaged page. A write door that hits such an error refuses the write and appends a pin to the existing questions journal beside the store family; every door that hits the damage pins the same deduplicated question. Damage on pages a door never reads does not block that door. Deliberate sweeps still belong on a copy: `PRAGMA quick_check` and `PRAGMA integrity_check` remain the recovery-time instruments, and the copy procedure below is unchanged. Both native crates bundle SQLite 3.53.2 through rusqlite 0.40.2. SQLite documents the [WAL-reset race and its fix](https://sqlite.org/wal.html#walreset) in 3.51.3 and later. A vulnerable engine and WAL mode establish exposure. They do not prove the historical race occurred.

Use the native copy verb on a quiescent offline database with its matching `-wal` sibling. The output directory must not exist. Both paths must stay outside the operator state root.

```bash
fno doctor event recover --copy-store --source /tmp/evidence/events.db --output /tmp/recovered-events
```

The verb captures DB and WAL into scratch and refuses changed sources or linked files. SQL reconstruction preserves every readable table, explicit index, schema version, event identity, sequence, metadata row, and observation state. It resets only the derived `ingest_cursor`. Each preserved table receives a row count and matching source/candidate digest. Publication requires both `quick_check` and `integrity_check` to return `ok`. The new `events.db` and `recovery.json` are copy artifacts only. An unreadable authoritative table fails the verb. No JSONL rebuild or live installation occurs.

## Grammar

- writers: `fno.events.append_event` (Python), `EventEmitter` and
  `append_event_line` (Rust), `_append_bounded_event` (shell) - all commit
  through the native store
- readers: `fno.events.store_client` (Python), `query_events` and `journal_text` (Rust)
- operator surface: `fno doctor event emit|find|audit|gc|export`
- pre-runtime only: `hook-events.jsonl` stays a bounded diagnostic for
  failures before the native binary can answer; no gate reads it
