# Cost Accuracy: Transcript Dedup + Catalog Pricing

How session cost numbers are computed, why they were ~7.5x inflated until
2026-06, and how to keep them honest.

## The two bugs this architecture prevents

1. **Per-line double counting (all models, ~2.5-2.8x).** Claude Code writes
   one transcript JSONL line per content block; every line of the same API
   message repeats identical `message.usage`. Summing usage per line
   overstated tokens by the duplication factor (verified on a live
   transcript: 502 assistant lines -> 185 unique messages).
2. **Unknown-opus pricing fallback (3x per new opus release).** The old
   hand-kept Python table (`cost_tracker.py` + `pricing.yaml`, deleted
   2026-10-02) matched known opus versions explicitly and fell through to
   the legacy opus-4.0 tier for anything unrecognized. Each new opus release
   (4.7, then 4.8 at $5/$25) was priced 3x high until someone updated the
   table. That hand step is the reason the table is gone: a catalog that
   updates daily deletes it.

The bugs multiplied: opus-4-8 sessions registered at ~7.5x true cost, and
budget caps (`cost_cap_usd`) tripped sessions at ~13% of their real budget.

## Component map

| Surface | Role |
|---|---|
| `crates/fno-agents/src/model_price.rs` | **Single pricing source of truth.** Reads the `<state>/cache/models-dev.json` catalog `fno` fetches daily (`crates/fno/src/model_catalog.rs`); `PriceBook::rates` + `cost()` + the running fold. No fallback tier: a nonzero token kind with no rate is unpriced. |
| `fno-agents context-run --model-price` | The price leg's CLI door: tokens in, four-decimal dollars out; `unpriced` + exit 3 on a miss. |
| `cli/src/fno/cost/_session_cost.py` | Transcript parser + ledger writer. Dedups usage by `(message.id, requestId)`; `calculate_cost` shells the price leg; unpriced sessions write `cost_usd: null` + `unpriced_model`. |
| `scripts/metrics/backfill-cost-recompute.py` | One-shot historical correction for ledger.json + graph.json (idempotent, marker-based). |
| `fno doctor --cost-check` | Opt-in drift tripwire vs the reference cost tool. |

```
transcript JSONL ──parse (dedup by message.id+requestId)──> SessionMetrics
                                                                 │
                        context-run --model-price (models.dev) ──┤
                                                                 ▼
stop hook ──register-session-cost.sh──> _session_cost.py ──> ledger.json
                                                                 │
                budget cap (loopcheck.rs cost/wall-clock caps)   ┤
                graph.json cost_sessions (register path)─────────┤
                ledger.md render ────────────────────────────────┘
```

The mux card reads the same leg through the daemon's sweep
(`liveness_sweep::measure_session_cost`), so the card's `~$` and the
ledger's `cost_usd` cannot drift into two tables.

## Dedup semantics

- Dedup key: `(message.id, requestId)`. All content-block lines of one API
  message share both fields and byte-identical usage; the first line counts,
  the rest are skipped. Lines missing either field (or carrying non-string
  values) count as-is - over-counting toward the old behavior is the safe
  failure direction for a cost meter; false dedup is not.
- The `seen` set is shared across all transcripts within one logical sum (`main()` across session IDs, one set per ledger entry in backfills). Resumed sessions copy prior history lines, with usage, into the new transcript file, so per-file dedup alone would re-count history. This is the same reason the community cost tools dedup globally.
- The Rust running fold dedups claude usage by `message.id` (the same rule)
  and reads codex cumulative totals instead of re-parsing.
- Compaction detection is unaffected: duplicates carry identical usage, so
  skipping them does not change the context-size series.

## Unpriced policy (absent, not guessed)

There is no fallback tier. A session prices only when every token kind with
a nonzero count has a catalog rate:

- priced: `~$` on the card, a dollar figure in the ledger.
- unpriced: the card shows the raw token count (`78.0M tok`), the ledger row
  carries `cost_usd: null` plus `unpriced_model`, `_session_cost --json`
  prints the same pair. A missing or failed catalog keeps everything
  unpriced until the next refresh lands one; the composer and the mux server
  re-stat hourly.

`~` marks every card figure as an estimate: context-tier rates
(`context_over_200k`), Opus fast mode and web-search fees are not priced,
and a session that switched models prices at its primary model in the
ledger while the card prices per model.

## Operator runbook: historical backfill

`scripts/metrics/backfill-cost-recompute.py` corrects ledger.json +
graph.json once, idempotently:

```bash
python3 scripts/metrics/backfill-cost-recompute.py            # dry-run, no writes
python3 scripts/metrics/backfill-cost-recompute.py --apply    # write
```

- Per-entry strategy (marker `cost_backfill`, re-runs skip marked entries):
  transcripts survive -> full recompute (`recomputed`); opus-4-8 without
  transcripts -> cost/3 (`pricing_only` - exact for the pricing component,
  the dedup factor is unknowable without data); anything else ->
  `no_transcript`, cost untouched, never guess.
- Graph `cost_sessions` rows are corrected via session-id cross-reference through `fno.graph.store.locked_mutate_graph` (flock + backup). `session_id` fields are never rewritten - the budget enforcement path greps by session-id prefix.
- Concurrency: holds the register path's ledger flock
  (`/tmp/fno-ledger.lock`); `--apply` refuses while live
  target-session claims exist in `~/.fno/claims` (both TTL and
  PID-liveness claim shapes). `--force` overrides for a quiesced system you
  know is safe. The ledger and graph passes are individually atomic but not
  mutually atomic; an interrupted apply is re-run safe.
- After applying, review `config.budget.*.cost_cap_usd` values: caps set
  against inflated observations now bind ~7.5x later in real terms.

## Drift tripwire: `fno doctor --cost-check`

Opt-in (doctor's default run stays network-free and never assumes the reference tool is installed). Finds a recent ledger session with a surviving transcript, runs `session-cost.py --json`, then the reference tool's `session --json`, and compares:

| Outcome | Meaning | Exit |
|---|---|---|
| OK | divergence <= 10% | 0 |
| WARN | > 10% - catalog pricing or dedup drift; both numbers printed | 1 |
| skipped (reason) | reference tool absent / no candidate session / reference tool error | 0 |

Ground truth at ship time: the fixed parser reproduced the reference tool's $31.30 for the reference transcript to the cent at the measurement cutoff.

## Adding a new model (checklist)

1. Nothing. The rates come from models.dev on the daily fetch; a model the
   catalog prices is priced on the next refresh. A model it does not stays
   unpriced - never add a hand tier.

## Reported request cost and local OTel ingest

With `[telemetry] claude_otel = true` (the default), the daemon serves OTLP/http-json logs on localhost. Its `~/.fno/agents/otel/port` record survives shutdown, and the next daemon uses the same port. A first start uses port 4318. A bind conflict or invalid port record reports an error and does not choose a different endpoint. Supervisor birth uses this endpoint before the listener starts, so later receiver startup and daemon restarts keep the exporter destination stable. Explicit operator telemetry settings take precedence.

The database retains every log event in `otel_events`, including unknown future event names. `api_requests` supplies typed cost and token columns. Both tables are defined in one canonical SQL file. The generated [schema reference](../reference/otel-schema.md) lists every table and column. A batch commits raw retention and typed projection together. Failed storage returns HTTP 503 so the exporter can retry. Invalid payloads return HTTP 400.

Content is redacted before storage. `OTEL_LOG_TOOL_DETAILS=1` provides real skill/plugin attribution but also exports command text and tool arguments. The receiver masks those values and keeps safe tool/skill/MCP names. It masks prompt/response text, API bodies, hook definitions and managed settings content even if a sender enables them. fno does not set `OTEL_LOG_USER_PROMPTS` or `OTEL_LOG_TOOL_CONTENT`. Metrics and traces remain off. Nothing is forwarded outside this machine.

### Health and coverage

```bash
fno doctor cost status
fno doctor cost status --json
```

Doctor and lead check-in use one telemetry reader. It reports API rows in the last hour, live Claude workers, uncovered sessions and unknown liveness. Missing data and unreadable data are distinct. A stale worker measurement cannot prove coverage. A default doctor invocation prints the same advisory on stderr, preserving its existing JSON stdout. The standalone cost status exits 1 for degraded coverage and 2 for a read failure.

A supervisor already running without telemetry keeps its old environment. Complete its hosted sessions before restarting it through the normal managed Claude lifecycle. Start the updated receiver, then let fno birth the supervisor and verify recent rows with the status command. Do not interrupt live workers to turn telemetry on. On the first upgrade from the old receiver, its shutdown can remove the old port record. The updated receiver then publishes the stable endpoint before the supervisor is restarted.

The off switch is `[telemetry] claude_otel = false`. It prevents a new listener and telemetry injection on supervisor birth. Existing supervisors keep their environment until restart.

### CSV export

```bash
fno doctor cost export --csv > request-costs.csv
fno doctor cost export --csv --output request-costs.csv
```

Export groups by UTC day, session, model and skill. It includes request counts, unpriced request counts, reported USD cost and token totals. Missing cost stays blank when the group has no priced requests. A partial cost sum is accompanied by its unpriced count. An absent database produces a header and an explicit empty-data diagnostic. An incompatible or unreadable database fails before output. CSV text is escaped.

These costs are provider-reported estimates, not billing statements. Burn watch prefers the stored per-session sum and falls back to the transcript ledger when no priced telemetry exists. The finalize handoff retains its existing ledger source.

To inspect events that have no typed cost projection:

```sql
SELECT event_name, COUNT(*) FROM otel_events GROUP BY event_name;
SELECT session_id, prompt_id, attributes, resource
FROM otel_events WHERE event_name = 'skill_activated';
```
