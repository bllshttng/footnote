> **SUPERSEDED (2026-06-05, ab-d0337fbc):** the machinery this file describes was deleted by the control-plane collapse wedge. Kept for historical context; see docs/architecture/control-plane-loop.md.

# BLOCKED reasons taxonomy

Canonical enum for the `blocked_reason.kind` field in `target-state.md` and the `last_blocked_reason.kind` field on graph nodes (`~/.fno/graph.json`). Phases 01-04 of the typed-blocker plan establish that BLOCKED can only be authored by hook-verifiable trip signals; this phase classifies *what kind* of BLOCKED was reached.

Skills, hooks, preflight, and per-phase verifiers reference this file when setting `blocked_reason`. Downstream consumers (the autocorrect spec, megawalk retros, postmortems) read the same enum.

## Tier 1 - Trip signals (LLM-side BLOCKED transitions)

These are the only kinds that can validly cause an LLM-side BLOCKED transition. Each maps to a hook-verifiable signal that the typed-blocker plan ships.

| kind | meaning | example details |
|------|---------|----------------|
| `user_cancel` | external cancel signal trip - `TARGET_CANCEL=1` env var or `.fno/.target-cancelled` sentinel with mtime>created_at | `"user touched .fno/.target-cancelled at 2026-04-27T15:30Z"` |
| `circuit_breaker` | three consecutive same-error trips from the stop-hook thrashing/stale detector | `"same test_failure on src/x.test.ts repeated 3 times"` |
| `rollback_exhausted` | retries exhausted across all configured rollback layers (max_rollbacks reached) | `"all 3 retry attempts failed: foo, bar, baz"` |

A BLOCKED entry whose `kind` is one of these MUST also have `trip_signal` set to the same value (typed-blocker invariant). The validator (`scripts/lib/blocked-taxonomy.sh`) rejects any state where `kind` is a trip signal and `trip_signal` is null.

## Tier 2 - Infrastructure-set categorizations

Set by the hook, preflight, sigma-review wrapper, or other infrastructure code paths - never by the LLM directly. These describe *why* a trip signal fired, or in some cases describe a hook-detected condition that pre-empted execution. `trip_signal` for these is null.

| kind | meaning | example details |
|------|---------|----------------|
| `environment` | preflight failure, env setup issue | `"uncommitted changes on main"` |
| `auth_failure` | gh / supabase / vercel auth expired or missing | `"gh auth status returned 401"` |
| `test_failure` | tests fail at HEAD or after change | `"3 tests failing: foo.test.ts:bar"` |
| `build_failure` | build/compile error | `"tsc exit 2: src/x.ts(5,3) error TS2345"` |
| `plan_outdated` | plan references files that no longer exist or have moved | `"plan references src/auth.ts which was deleted"` |
| `review_blocked` | sigma-review found blocking issues that auto-fix can't address | `"sigma-review L1 issue: type unsafe cast in src/x.ts"` |
| `external_review_pending` | check-pr is waiting for external action | `"PR #42 awaiting human review for 4h"` |
| `scope_creep` | files outside `plan_path` being touched | `"plan scoped to src/auth, but agent edited src/billing"` |
| `cost_exceeded` | budget cap hit | `"$25 cap reached at iteration 8"` |
| `iteration_ceiling` | iteration > N | `"iteration 16 of 15-cap"` |
| `model_fallback_exhausted` | all models in fallback chain returned errors | `"opus, sonnet, haiku all rate-limited"` |
| `verifier_failure` | per-phase postcondition verifier failed | `"do-postcondition: file D.ts listed in plan but not produced"` |

## Tier 3 - Catch-all

| kind | meaning | example details |
|------|---------|----------------|
| `other` | anything not matching above | free-text in details, classified later by autocorrect |

Use `other` only when the producing code path knows the BLOCKED is novel and not yet worth a dedicated kind. The autocorrect spec mines `other` entries to identify recurring classes that should be promoted into the enum.

## Adding a new kind

The enum is closed: adding a new kind requires a code change (`scripts/lib/blocked-taxonomy.sh::_BT_KINDS`), not just a state-file edit. The validator rejects unknown kinds.

Decision rule for one-time vs recurring:

- **One-time (use `other`):** A specific failure mode that is unlikely to recur or whose handling is fully captured by the free-text `details` field.
- **Recurring (extend the enum):** A failure mode that has occurred 3+ times across recent runs and that downstream consumers (autocorrect, retros) want to switch on. Open a follow-up spec; add the kind in tier 2; document the example.

The closed-enum invariant matters because every downstream consumer (autocorrect rules, megawalk retros, postmortem templates) switches on `kind`. A free-text drift would silently bypass classification.

## State-file shape

The `blocked_reason` field in `target-state.md` is structured (typed-blocker phase 05):

```yaml
blocked_reason:
  kind: <enum>             # one of the values above
  details: <string>        # human-readable specifics; free-text
  source_phase: <string>   # which phase set this (think/plan/do/review/etc.)
  iteration: <int>         # iteration counter when set
  session_id: <string>     # the session that hit this
  timestamp: <ISO-8601>    # when set
  trip_signal: <enum|null> # one of {user_cancel, circuit_breaker, rollback_exhausted} when kind is a tier-1 trip; null otherwise
```

A graph node mirror at `~/.fno/graph.json[node_id].last_blocked_reason` holds the same structured object plus `blocked_count` (incremented on every BLOCKED transition for that node). The autocorrect spec reads the graph mirror so it can correlate failure modes per-node across the whole backlog.

Legacy in-flight states with free-text `blocked_reason: "string"` are migrated to `{kind: 'other', details: 'string', trip_signal: null, ...}` automatically by `bt_get_blocked` on first read; a warning is logged.

## See also

- `scripts/lib/blocked-taxonomy.sh` - validator + setter helpers (`bt_validate_kind`, `bt_validate_trip_signal`, `bt_set_blocked`, `bt_get_blocked`, `bt_list_kinds`, `bt_list_trip_signals`)
- `internal/fno/plans/2026-04-23-typed-blocker-transitions/` - phases 01-04 establish the typed-blocker invariant; phase 05 is this taxonomy.
- `skills/target/references/state-schema.md` - full target-state.md frontmatter schema
- `skills/target/references/gate-artifacts.md` - parallel three-factor pattern on completion gates
