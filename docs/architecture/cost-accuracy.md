# Cost Accuracy: Transcript Dedup + Version-Aware Pricing

How session cost numbers are computed, why they were ~7.5x inflated until
2026-06, and how to keep them honest.

## The two bugs this architecture prevents

1. **Per-line double counting (all models, ~2.5-2.8x).** Claude Code writes
   one transcript JSONL line per content block; every line of the same API
   message repeats identical `message.usage`. Summing usage per line
   overstated tokens by the duplication factor (verified on a live
   transcript: 502 assistant lines -> 185 unique messages).
2. **Unknown-opus pricing fallback (3x per new opus release).**
   `model_tier()` matched known opus versions explicitly and fell through to
   the legacy opus-4.0 tier ($15/$75 per Mtok) for anything unrecognized.
   Each new opus release (4.7, then 4.8 at $5/$25) was priced 3x high until
   someone updated the table. Two one-shot backfills cleaned up the
   historical rows (the opus-4.7 repricing, then a general ledger recompute);
   both scripts are deleted now.

The bugs multiplied: opus-4-8 sessions registered at ~7.5x true cost, and
budget caps (`cost_cap_usd`) tripped sessions at ~13% of their real budget.

## Component map

| Surface | Role |
|---|---|
| `scripts/lib/cost_tracker.py` | **Single pricing source of truth.** `PRICING` table + `model_tier()` + `calculate_cost()` + the `estimate` CLI for shell callers. Pricing sources cited in the header: the Anthropic pricing page (canonical) and LiteLLM's `model_prices_and_context_window.json` (machine-readable reference, the same one community cost tools use). |
| `scripts/metrics/session-cost.py` | Transcript parser. Dedups usage by `(message.id, requestId)`; computes `SessionMetrics`; `--json` feeds the register path. |
| `scripts/metrics/cost-tracker.sh` | Shell shim. `estimate_cost` delegates to `cost_tracker.py estimate` - there is deliberately no shell pricing table. |
| `fno doctor --cost-check` | Opt-in drift tripwire vs the reference cost tool. |

```
transcript JSONL ──parse (dedup by message.id+requestId)──> SessionMetrics
                                                                 │
                                    model_tier (version-aware) ──┤
                                                                 ▼
stop hook ──register-session-cost.sh──> session-cost.py ──> ledger.json
                                                                 │
                budget cap (loopcheck.rs cost/wall-clock caps)   ┤
                backlog graph cost_sessions (register path)──────┤
                ledger.md render ────────────────────────────────┘
```

## Dedup semantics

- Dedup key: `(message.id, requestId)`. All content-block lines of one API
  message share both fields and byte-identical usage; the first line counts,
  the rest are skipped. Lines missing either field (or carrying non-string
  values) count as-is - over-counting toward the old behavior is the safe
  failure direction for a cost meter; false dedup is not.
- The `seen` set is shared across all transcripts within one logical sum (`main()` across session IDs, one set per ledger entry in backfills). Resumed sessions copy prior history lines, with usage, into the new transcript file, so per-file dedup alone would re-count history. This is the same reason the community cost tools dedup globally.
- Compaction detection is unaffected: duplicates carry identical usage, so
  skipping them does not change the context-size series.

## Pricing fallback policy (optimistic, not pessimistic)

`model_tier()` extracts the opus version numerically (first digit pair after
"opus", so the live `[1m]` context suffix never parses as a version; minors
longer than 2 digits are date stamps, so `claude-opus-4-20250514` stays on
the legacy tier):

- version >= 4.5 -> exact tier if present in `PRICING`, else the **latest
  modern tier** (`LATEST_MODERN_OPUS_TIER`)
- version < 4.5, or `claude-3` -> legacy `opus-4.0` tier (those really were
  $15/$75; known history is never silently repriced)
- unparseable -> latest modern tier + one-time stderr warning, and the model
  ID lands in `FALLBACK_MODELS_SEEN`, which `session-cost.py --json`
  surfaces as `pricing_fallback_models` so the drift is machine-visible in
  the ledger (stop-hook stderr is swallowed; JSON is not)

Rationale: every future opus is >= 4.5. The pessimistic fallback produced
two 3x inflation incidents; the optimistic default degrades to a small error
only if Anthropic raises prices, which the doctor cross-check catches. When
a new opus ships, add its tier to `PRICING` and update
`LATEST_MODERN_OPUS_TIER`.

## Historical backfills (retired)

Two one-shot scripts corrected the historical ledger after the pricing fixes, completed their runs, and were deleted. `backfill-opus47-costs.py` repriced the opus-4.7 rows; `backfill-cost-recompute.py` recomputed every ledger entry and its graph `cost_sessions` from surviving transcripts (full recompute when a transcript survived, cost/3 for opus-4-8 on pricing alone, untouched when nothing was known; marker-based and idempotent, holding the register path's ledger flock). No recompute pass is maintained: the parser-level fixes above keep new rows honest.

## Drift tripwire: `fno doctor --cost-check`

Opt-in (doctor's default run stays network-free and never assumes the reference tool is installed). Finds a recent ledger session with a surviving transcript, runs `session-cost.py --json`, then the reference tool's `session --json`, and compares:

| Outcome | Meaning | Exit |
|---|---|---|
| OK | divergence <= 10% | 0 |
| WARN | > 10% - pricing table or dedup drift; both numbers printed | 1 |
| skipped (reason) | reference tool absent / no candidate session / reference tool error | 0 |

Ground truth at ship time: the fixed parser reproduced the reference tool's $31.30 for the reference transcript to the cent at the measurement cutoff.

## Adding a new model (checklist)

1. Add the tier to `PRICING` in `scripts/lib/cost_tracker.py` (cite the pricing page in the header comment if rates changed).
2. If it is the newest opus, update `LATEST_MODERN_OPUS_TIER`.
3. Extend the `model_tier` matrix test in `tests/lib/test_cost_tracker_pricing.py`.
4. Nothing else: the shell shim and every register-path consumer read the same table by construction.
