> **SUPERSEDED (2026-06-05, ab-d0337fbc):** the machinery this file describes was deleted by the control-plane collapse wedge. Kept for historical context; see docs/architecture/control-plane-loop.md.

# Postmortem format

Every BLOCKED target transition writes a single postmortem file. The format is fixed so the autocorrect monthly review can parse it mechanically without per-file logic.

## File location

`~/.fno/postmortems/{YYYY-MM-DD}-{session_id_short}.md`

- `YYYY-MM-DD` derives from the `generated_at` timestamp, not the session start.
- `session_id_short` is the first 8 chars of the session_id (e.g. `20260427T153200Z-12345-a1b2c3` -> `20260427`).
- If a collision is encountered (theoretically impossible because session_ids embed the start timestamp plus PID plus 6 random hex), append a `.2`, `.3` suffix to the filename before the `.md` extension.

## Schema

A complete postmortem has YAML frontmatter (machine-readable) followed by three markdown body sections (human-readable).

```yaml
---
type: target-postmortem
session_id: 20260427T153200Z-12345-a1b2c3
generated_at: 2026-04-27T15:32:04Z
target_invocation: "/target M ab-a5e142a2"
plan_path: ~/code/abilities/internal/fno/plans/2026-04-27-postmortem-and-verifiers/
mode: medium
blocked_phase: validate
blocked_reason:
  kind: test_failure
  trip_signal: null
  details: "3 tests failing in src/auth/login.test.ts"
  source_phase: validate
  iteration: 4
iteration_count: 4
restart_count: 0
cost_usd: 1.23
duration_minutes: 18
---

# Postmortem: 20260427

## Phase timeline

| Phase | Status | Duration | Notes |
|---|---|---|---|
| think | complete | 1m | design doc produced |
| plan | complete | 2m | 4 phases planned |
| do | complete | 8m | 3 of 4 stories done; story 4 deferred |
| clean | complete | 1m | minor simplification |
| review | complete | 3m | sigma-review clean |
| validate | BLOCKED | 3m | 3 tests failing |

## Last output of failed phase

```
[verbatim last 50 lines of the failed phase's tool output, captured from
 the validate-{session_id}.md artifact's body if present, otherwise from
 the target-stop-hook.log tail at BLOCKED time]
```

## Hypotheses

1. Test setup may have drifted from production: check seed data fixtures.
2. Story 4 deferral might be related: missing fixture migration?
3. Flake risk: rerun once before deeper investigation.
```

## Frontmatter contract

| Field | Source | Required | When unknown |
|---|---|---|---|
| `type` | Always literal `target-postmortem` | yes | n/a |
| `session_id` | `session_id:` from target-state.md | yes | `unknown-{generated_at_epoch}` |
| `generated_at` | UTC ISO-8601 timestamp at write time | yes | n/a |
| `target_invocation` | Reconstructed from state's `input:` + size hints in body | optional | omit |
| `plan_path` | `plan_path:` from target-state.md | optional | omit |
| `mode` | One of: `small`, `medium`, `large` (parsed from the body's `size:` line) | optional | omit |
| `blocked_phase` | `current_phase:` from target-state.md at BLOCKED-write time | yes | `unknown` |
| `blocked_reason.kind` | `blocked_reason:` from target-state.md (left side of `:`) | yes | `other` |
| `blocked_reason.trip_signal` | `trip_signal` from blocked-taxonomy or null when not a tier-1 trip signal | yes (literal `null` allowed) | `null` |
| `blocked_reason.details` | `blocked_reason_details:` from target-state.md, or the suffix of `blocked_reason:` after `:` | optional | omit |
| `blocked_reason.source_phase` | `current_phase:` at BLOCKED-write time | optional | omit |
| `blocked_reason.iteration` | `iteration:` from target-state.md | optional | omit |
| `iteration_count` | `iteration:` from target-state.md | yes | `1` |
| `restart_count` | Count of prior archived state files for the same input | optional | `0` |
| `cost_usd` | `total_cost_usd:` from target-state.md or session-cost.py | optional | `unknown` (literal string) |
| `duration_minutes` | `(generated_at - created_at)` minutes, rounded | optional | `unknown` |

Unknown numeric fields use the literal string `unknown` rather than `null` or `0` so a downstream consumer cannot confuse "no data" with "zero".

## Body section contract

Three sections, in this fixed order:

### `## Phase timeline`

A markdown table with one row per phase that this session entered. Columns: `Phase`, `Status`, `Duration`, `Notes`.

- `Phase` comes from the handoff artifact name (`.fno/artifacts/handoff/{phase}-{session_id}.md`).
- `Status` is `complete` for phases with an artifact, `BLOCKED` for the failing phase, `not-reached` for phases that never ran.
- `Duration` is derived from artifact mtime deltas; `unknown` if only one artifact exists.
- `Notes` is a one-line summary pulled from the handoff artifact (e.g. `stories_completed:` count, `verdict:`, `notes_for_next_phase:` first line).

If no handoff artifacts exist (e.g. session blocked before any phase completed), the table contains a single row: `| init | BLOCKED | <duration> | no phase artifacts found |`.

### `## Last output of failed phase`

A markdown code fence holding the last 50 lines of relevant output. Source priority:

1. Phase artifact body at `.fno/artifacts/{phase}-{session_id}.md` (when present).
2. `.fno/target-stop-hook.log` tail (always present after BLOCKED write).
3. The literal string `(no captured output)` when both are missing.

50 lines is the cap; long outputs are truncated from the top, preserving the tail. A truncation marker `... (NN lines elided) ...` is inserted as the first line of the fence when truncation happened.

### `## Hypotheses`

A numbered list (1, 2, 3) of one-paragraph hypotheses about the cause. v1 of `generate-postmortem.sh` ships with a templated lookup table keyed on `blocked_reason.kind`:

| kind | Hypothesis paragraphs |
|---|---|
| `test_failure` | (1) fixture/seed drift after recent migration. (2) recently changed dependency broke the suite. (3) flake; rerun before deeper investigation. |
| `build_failure` | (1) compile error from recent edit. (2) dependency version drift; check lockfile. (3) generated file (proto/codegen) out of date. |
| `auth_failure` | (1) gh CLI session expired; re-run `gh auth login`. (2) missing scope (e.g. `repo` for pushes). (3) OAuth redirect URI mismatch on non-default base. |
| `plan_outdated` | (1) files moved or renamed since plan was authored. (2) a blocking dependency was reordered. (3) plan's expected post-conditions no longer match repo state. |
| `review_blocked` | (1) sigma-review blocking issue not yet addressed. (2) sigma-review found a regression in code we just shipped. (3) sigma-review surfaced a missing test. |
| `external_review_pending` | (1) Gemini reviewer still processing (median 7m, max ~4h). (2) PR has changed since review started; reviewer waiting for stable state. (3) external reviewer is rate-limited. |
| `scope_creep` | (1) implementation drifted from plan; check files outside plan_path. (2) refactor temptation; the work expanded beyond the original ask. (3) missing planning step for adjacent feature. |
| `cost_exceeded` | (1) too many iterations on the same problem; root cause unclear. (2) plan was under-scoped for the model used. (3) consider rotating to a more capable model. |
| `iteration_ceiling` | (1) thrash detector tripped; planning likely wrong. (2) same fingerprint 5+ times; LLM stuck. (3) split the task; current scope too large. |
| `model_fallback_exhausted` | (1) all configured providers returned errors. (2) check provider rotation queue health. (3) verify API keys and quota for each provider. |
| `verifier_failure` | (1) phase verifier caught silent under-delivery. (2) artifact present but content invalid. (3) phase contract violated; check verifier reason JSON. |
| `user_cancel` | (1) user actively cancelled via sentinel or env var. (2) intentional pause; no autonomous follow-up needed. |
| `circuit_breaker` | (1) circuit breaker tripped after 3 same-error failures. (2) approach is wrong; rotate strategy. (3) consider involving a fresh agent. |
| `rollback_exhausted` | (1) validation rolled back 3+ times; no working checkpoint. (2) plan needs to be redesigned. |
| `environment` | (1) shell/tool failure unrelated to code. (2) disk space, permission, or auth precondition missing. |
| `other` | (1) failure mode not in taxonomy; check target-state.md and target-stop-hook.log for context. |

The lookup table lives inside `generate-postmortem.sh`; postmortem-format.md is the spec the script implements.

A future v2 may run a small LLM call to produce phase-specific hypotheses. v1 is deterministic so the script can run without any API access at BLOCKED time.

## Append-only invariant

Postmortem files are append-only by convention - no skill or hook in this project deletes them. The plan's architectural invariants forbid both manual deletion and `--skip-postmortem` flags. Old postmortems accumulate at `~/.fno/postmortems/`; the user prunes manually if desired.

If `generate-postmortem.sh` is invoked twice on the same session (rare, theoretically possible via resumption), the second call appends a `.2`, `.3` suffix to the filename and writes a new file rather than overwriting. The previous postmortem stays exactly as it was.

## What to populate when a section can't be filled

The script never crashes on missing data. Every field has a documented "when unknown" behavior:

- Missing `session_id` -> `unknown-{epoch_at_write_time}` so the file still has a stable filename.
- Missing `cost_usd` or `duration_minutes` -> literal string `unknown` in frontmatter; downstream consumers parse and skip.
- Missing handoff artifacts -> phase timeline table degrades to a single `init | BLOCKED | unknown | no phase artifacts found` row.
- Missing phase output -> "Last output of failed phase" section contains the literal `(no captured output)` inside an empty code fence.
- Missing `blocked_reason` -> `kind: other`, `details:` omitted, hypothesis paragraph 1 from the `other` row of the lookup table.

A partial postmortem is more useful than no postmortem; the autocorrect loop's reviewer can interpret `unknown` fields and act anyway. Crashing the script would mean BLOCKED target runs exit with no record at all.

## Scannability requirements

The format is optimized for human-readable terminal viewing:

- Frontmatter is at most ~20 lines of YAML, all flat key:value pairs.
- Phase timeline is a markdown table; viewable rendered or raw.
- Code fences use no language hint so they don't fight terminal renderers.
- Hypothesis section uses plain numbered list, one paragraph per item, no nested structure.

A ~80-column terminal should be able to display the entire postmortem with `less`. No long single-line content; long stdout output is wrapped or truncated.

## Example: minimum-information postmortem

When target blocked extremely early (e.g. preflight failure):

```yaml
---
type: target-postmortem
session_id: 20260427T155500Z-00042-deadbe
generated_at: 2026-04-27T15:55:02Z
plan_path: ~/code/abilities/internal/fno/plans/2026-04-27-postmortem-and-verifiers/
blocked_phase: init
blocked_reason:
  kind: environment
  trip_signal: null
  details: "preflight BLOCKED: working tree dirty"
  source_phase: init
  iteration: 1
iteration_count: 1
restart_count: 0
cost_usd: unknown
duration_minutes: unknown
---

# Postmortem: 20260427

## Phase timeline

| Phase | Status | Duration | Notes |
|---|---|---|---|
| init | BLOCKED | unknown | no phase artifacts found |

## Last output of failed phase

```
preflight BLOCKED: one or more environment checks failed.
  x working-tree-clean: 4 modified files
Fix the issues listed above, then re-run /target.
```

## Hypotheses

1. Shell/tool failure unrelated to code: investigate the environment preconditions surfaced above.
2. Run `git status` and either commit/stash the dirty files or re-run with `--skip-preflight`.
```
