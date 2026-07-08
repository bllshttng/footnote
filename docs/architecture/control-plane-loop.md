# Control Plane Loop: Post-Wedge Architecture

## Scope

This document covers the stop-hook decision verb (`fno-agents loop-check`) that runs INSIDE a session. For the driver loop that dispatches sessions from outside - target, megawalk, megatron - see [unified-loop.md](unified-loop.md) (step 5).

## Principle

The stop hook reads the world; it does not maintain state. The only writer is the append-only event log. A session is done when the world (PR + CI + review) agrees it is done, not when a boolean says so.

## What was deleted

Three layers of bash control-plane machinery were removed:

| Layer | LOC | What it was |
|---|---|---|
| Stop hook + detectors | ~7575 | 1101-line `hooks/target-stop-hook.sh` + 27 `scripts/lib/` helpers (thrash/budget/phase-stall/help-escalation/orphan detectors, gate-provenance, cancel/blocked handlers) |
| `fno gate` CLI surface | ~1460 | `cli/src/fno/gates/` package + both `gate_reality_map.yaml` copies; `fno gate ...` now exits unknown-command |
| Phase verifiers | ~900 | `skills/target/scripts/verifiers/verify-{phase}.sh` + orchestrator (11 files) |

Total deleted: ~9935 LOC. Replaced by: `fno-agents loop-check` Rust verb (~1830 LOC including unit tests) + 118-line bash shim.

## The shim (`hooks/target-stop-hook.sh`)

A 118-line read-only shim. The ONLY decision it makes is binary resolution order:

1. `$FNO_AGENTS_BIN` (if set and executable)
2. `<repo>/crates/fno-agents/target/release/fno-agents`
3. `<repo>/crates/fno-agents/target/debug/fno-agents`
4. `command -v fno-agents` (PATH fallback)

**Binary missing:** emits `loop_check_binary_missing` event to both event logs and allows exit. The session must be re-spawned once the binary is available.

**Foreign session guard:** reads `claude_transcript_id` from the manifest; if the live session's transcript ID does not match, the shim exits clean without calling the verb (another session owns that manifest).

The shim sources NO `scripts/lib/*.sh` files. Its only writes: the `loop_check_binary_missing` event append (when binary is absent) and `loop-check.stderr.log` append (verb stderr).

## The verb (`fno-agents loop-check`)

CLI contract:

```bash
fno-agents loop-check \
  --state <target-state.md> \
  --transcript <transcript.jsonl> \
  --cwd <project-root> \
  [--events <p>] [--global-events <p>] \
  [--settings <p>] [--ledger <p>] \
  [--now <rfc3339>] [--gh-bin=<p>] [--git-bin=<p>]
```

Env overrides: `FNO_LOOPCHECK_GH_BIN`, `FNO_LOOPCHECK_GIT_BIN`.

Output: one JSON object on stdout:

```json
{"decision": "allow|block", "termination_reason": "...", "message": "...", "fires": 3, "fingerprint": "sha|pr|ci|ts"}
```

Exit 0 for both allow AND block. Exit 2 for CLI misuse only.

### Decision algorithm

1. `<aborted reason="...">` in transcript -> `Aborted` (terminate).
2. `<promise>MISSION COMPLETE: ...</promise>` in transcript -> run `done()`:
   - If all reads pass: terminate with `DonePRGreen` (or `DoneAdvisory` in advisory mode).
   - If a read fails: block, name the failing read in `message`. Loop continues.
3. No promise -> backstop check (see below).

### `done()` reads

Reads performed when a promise is seen. Each is skipped when the corresponding manifest flag is set:

| Read | Skipped when |
|---|---|
| 1. PR exists for HEAD commit (`gh pr view`) | `no_ship: true` |
| 2. CI is green on that PR (`gh pr checks`) | `no_ship: true` OR `ci.declared_none: true` in settings |
| 3. Every required bot has a completed review pass (`gh pr view --json reviews,comments`) | `no_external: true` in manifest |
| 4. No unaddressed blocking inline finding (`gh api /pulls/N/comments`) | same as Read 3 |

A "done but mute" session (all reads pass but no promise was emitted) resolves as a late `DonePRGreen` at backstop time.

### Review gate (step 2)

The wedge's floor (any one completed bot review flips `reviewed`) was sharpened to the contract grilled decision 5 locked: a session is reviewed only when **every bot in `config.review.required_bots` has at least one completed review pass**.

```yaml
# config.toml
config:
  review:
    required_bots:
      - chatgpt-codex-connector
```

- **Default** (key absent): `["chatgpt-codex-connector"]`. Codex-only - gemini is being removed, and claude is not a default reviewer (author-reviews-own-work is weak signal).
- **Explicit `[]`**: the declared no-review-gate path - review reads are skipped and PR + CI carry the gate, mirroring `ci.declared_none`. Declared, never auto-detected.
- **Malformed value** (scalar, bare key with no items): fails closed to the code default rather than disabling the gate.
- A "completed pass" is a top-level review with any non-empty `state` - in practice `COMMENTED` (both bots emit `COMMENTED`, never `APPROVED`; verified in practice). A pass on ANY commit counts: codex reviews once per PR and does not re-review on push, so requiring a pass on HEAD would make the gate unsatisfiable.
- Matching is case-insensitive substring, so a short name (`codex`) or full login both match, including gh's `[bot]`-suffixed forms.
- A missing bot blocks with the gap named: `PR #N: chatgpt-codex-connector has not reviewed`.
- `config.external_reviewers` is unchanged: it stays the *recognition* list (which logins count as bots); `required_bots` is the *must-have-reviewed* list.

**Single-bot tradeoff (accepted):** with one required bot there is no redundancy if codex no-shows; the session resolves via the budget / NoProgress backstop and the operator investigates. No review-timeout timer - a timer is a detector reborn, exactly what the wedge deleted.

#### Inline findings (Read 4)

Codex's P1s land on the `GET /pulls/{N}/comments` review-comments endpoint, which `gh pr view --json comments` does NOT return (verified in practice: inline comments are invisible to the GraphQL read). Read 4 fetches that endpoint (`--paginate`, concatenated pages parsed as a JSON stream) on the promise/backstop path only - quiet fires do not pay the extra API call.

- **A finding** is a root comment (`in_reply_to_id == null`) authored by a required bot whose body carries a blocking severity badge:
  - codex: `![P1 Badge]` / `badge/P1-` -> P1 blocking; P2/P3 advisory
  - gemini: `![critical]` / `![high]` / `*-priority.svg` -> critical|high blocking; medium|low advisory
  - Unparseable severity -> advisory, never blocking (under-blocking is the only safe failure; the agent cannot edit a bot's comment and PR history is the post-hoc audit).
- **Addressed** = the finding's thread has a non-bot reply AND (a commit landed after the finding's `created_at` OR a non-bot reply body contains `wontfix:`, case-insensitive). A commit alone never clears a P1 (anti-gaming); a bot's own reply is not an ack. Commit timestamps come from `gh pr view --json commits` (`committedDate`), fetched only when a blocking candidate exists.
- **Block message** names the gap: `PR #N: chatgpt-codex-connector[bot] P1 at src/x.rs:42 unaddressed (reply in-thread or wontfix:)`.
- **Fingerprint**: Read 4's newest comment timestamp folds into the fingerprint's 4th component, so a late inline finding (codex posts inline findings minutes after its review summary) advances the fingerprint - the session re-blocks rather than terminating `NoProgress`, and a fire that saw a clean PR before the finding arrived cannot have already terminated on stale data.
- **Failure**: a Read 4 / commits-read failure fails closed exactly like Reads 1-3 - block with the read named (`pulls_comments` / `pr_commits`), retry next fire.

The matching writer lives in `/pr check`: it replies in-thread (`in_reply_to`) per blocking finding - fix replies name the commit, declines carry `wontfix:`. Without that writer the gate is unsatisfiable (a PR addressed with zero in-thread replies could not have passed this gate).

### TerminationReason enum

| Value | Cause |
|---|---|
| `DonePRGreen` | PR green + reviewed (or advisory equivalent) |
| `DoneAdvisory` | Advisory mode: promise seen, no_ship or advisory flag set |
| `NoWork` | No state file or no recognizable work in progress |
| `Budget` | Budget cap reached (see Budget Resolution below) |
| `NoProgress` | Backstop: fingerprint unchanged for N fires |
| `Interrupted` | Cancel sentinel `.fno/.target-cancelled` detected |
| `Aborted` | `<aborted reason="...">` tag seen in transcript |

### Backstop fingerprint

4-component fingerprint, checked on every fire when no promise is present:

```
HEAD sha | PR state | CI conclusion | latest review/comment timestamp
```

If the fingerprint is identical across N consecutive fires, terminate with `NoProgress`:
- Unattended: N = 3
- Attended: N = 5

A "done but mute" session resolves as `DonePRGreen` when the reads pass (rather than `NoProgress`) so a session that completed but never emitted a promise closes cleanly rather than being flagged as stuck.

### Budget resolution

Priority order (first match wins):

1. `config.budget.unattended.cost_cap_usd` / `wall_clock_cap_minutes` (nested config)
2. `config.budget.attended.cost_cap_usd` / `wall_clock_cap_minutes` (nested config)
3. `budget_cost_cap_usd` / `budget_wall_clock_cap_minutes` in the manifest (flat fallback)
4. `budget_cap` in settings (legacy flat key)

When a cap is reached: terminate with `Budget` and record `budget_axis: wall_clock|cost`.

**Cap value parsing (manifest keys only):** the two manifest numeric caps tolerate a trailing `#` comment glued onto the value with or without whitespace (`200# Auto-merge inputs` parses as 200). These are machine-written fields, and manifests are immutable, so parser tolerance is what restores loop enforcement for sessions whose manifests were corrupted by the pre-fix init script (a heredoc bug). A value that is genuinely non-numeric after stripping the `#`-tail (e.g. `abc`, or a bare comment with no value) still fails closed: the session classifies as `Budget` and a `malformed budget cap` diagnostic lands on stderr. Settings.yaml cap values keep strict YAML comment semantics (whitespace required before `#`) and are not affected.

### Degraded modes

**(a) No `gh` binary (advisory mode):**
- `promise` -> `DoneAdvisory`
- `aborted` -> `Aborted`
- unattended: refuses with `NoWork` (gh is required for autonomous sessions)
- Cancel sentinel still -> `Interrupted`

**(b) Transient gh failure during `done()` read (step 2 semantics):**
- Affected read fails closed: block with the read named in `message`, emit `loop_check_gh_error`
- Loop retries on the next stop-hook fire
- gh-errored fires are TRANSPARENT to the NoProgress streak: they neither advance nor reset the consecutive count, and a tripped backstop never terminates `NoProgress` on a gh error (a healthy session must not be killed because GitHub blipped - reverses the wedge's behavior, locked decision 6). After the outage clears, the streak resumes from its pre-outage value. Budget remains the sole ceiling during a sustained outage.
- "No PR exists" is NOT an outage: `gh pr view`'s deterministic `no pull requests found` stderr classifies the fire as healthy world-state (`pr_state=none` fingerprint), so a session that never ships a PR still resolves via the NoProgress backstop. If gh ever changes that message, no-PR fires degrade to outage semantics (freeze -> budget ceiling) - safe, never premature termination.

**Back-compat - legacy manifest:**
- Only TERMINAL legacy statuses (COMPLETE, BLOCKED, ABORTED) short-circuit: allow-exit immediately, no `done()` or backstop runs, emit `loop_check_legacy_manifest` event.
- A legacy `status: IN_PROGRESS` session is NOT a short-circuit: the field is ignored and the session falls through to the normal external-truth loop (fingerprint, backstop, budget checks apply). The new booleans drive the decision; the old `status: IN_PROGRESS` string has no effect.

### Advisory units

Sessions with `no_ship: true` OR `advisory: true` in the manifest:
- `done()` skips all gh reads
- Promise -> `DoneAdvisory`
- Budget still applies
- Backstop still applies

## The immutable manifest

`target-state.md` is written once by `fno target init`. Fields:

**Core inputs:** `session_id`, `created_at`, `input`, `plan_path`, `target_size`

**Skip flags** (set from CLI args or config at init): `no_external`, `no_docs`, `no_ship`, `no_verify`, `no_goals`, `no_browser`, `no_clean`, `no_how_to`, `no_memory`, `no_deferrals_capture`

**Session context:** `has_ui`, `attended`, `advisory`, `cross_project`, `scratchpad_path`

**Budget caps** (omitted when unconfigured): `budget_wall_clock_cap_minutes`, `budget_cost_cap_usd`

**Provider:** `provider`, `provider_mode`, `provider_upgrade_reason`

**Ownership:** `owner_pid`, `owner_started_at`, `owner_cwd`, `claude_transcript_id`

**Auto-merge:** `auto_merge_enabled`, `auto_merge_approved`

**Mission fields:** `mission_id`, `mission_wave`, `mission_slug`, `mission_from_msg_id`

**Graph node** (appended when a node is found): `graph_node_id`, `graph_node_claim_refused`, `target_claim_key`, `target_claim_holder`, `target_claim_ttl`, `target_claim_blocked_reason`

**Write-once enforcement:**
- Detection: manifest is immutable iff frontmatter has no `status:` key.
- Allowed post-init: `fno state set --field plan_path` when current value is empty (first-fill only).
- Refused: any other field on an immutable manifest -> exit 5, `state_write_refused` event.

## Event types

All events land in BOTH `<cwd>/.fno/events.jsonl` AND `~/.fno/events.jsonl` with envelope `{ts, type, source:"hook", data}`.

| Event type | Emitted by | When |
|---|---|---|
| `loop_check` | verb | every stop-hook fire |
| `termination` | verb | when a TerminationReason fires |
| `loop_check_gh_error` | verb | gh CLI fails during a `done()` read |
| `loop_advisory_mode` | verb | session is in advisory mode |
| `loop_check_binary_missing` | shim | `fno-agents` binary not found |
| `loop_check_legacy_manifest` | verb | manifest has `status:` key (pre-wedge) |

## Back-compat

Legacy manifests (those with a `status:` key written by the pre-wedge init) are recognized and handled in allow-exit mode. The `loop_check_legacy_manifest` event is emitted for observability. No automatic migration occurs.

## What arrived

- **Step 5 group 1**: `run-target-loop.sh` replaced by the Rust loop runtime + target driver + 74-line exec shim. See [unified-loop.md](unified-loop.md).
- **Step 5 group 2**: megawalk driver added - `MegawalkQueue` / `MegawalkDispatcher` + deletion of the Python walker (~4135 LOC). See [unified-loop.md](unified-loop.md) "The megawalk driver" section.
- **Step 5 group 3**: megatron driver - `MegatronQueue` over fleet projects, each walked as a megawalk one altitude down; the Python commander poll loop deleted; batch-queue deprecated; the exit-12 `fno loop` stub removed. See [unified-loop.md](unified-loop.md) "The megatron driver" section.
- **Step 6**: side-effect re-homing - the final step. The mechanical side-effects (ledger session-record, plan stamp/graduate, handoff artifact) move OUT of the skill's pre-promise bash into a new `fno-agents finalize` writer the shim invokes on a terminal-allow decision, so they fire in every mode even when the agent compacted before pre-promise. Goal-verification is deleted (intent rides the CI read), and browser/memory/deferrals/docs are demoted to advisory run-and-log. `config.gates.strict` retires; `config.unattended.enabled` survives as the mode discriminator. See "The finalize writer" below.

## The finalize writer (step 6)

`loop-check` stays a pure read-only DECISION verb. `fno-agents finalize` is a separate WRITER the shim runs AFTER a terminal-allow decision (a non-null `termination_reason`):

```
decision = fno-agents loop-check ...        # pure read-only, unchanged
if decision.allow AND decision.is_terminal:
    fno-agents finalize --reason <termination_reason>   # writer, non-fatal
```

It splits into two trigger classes:

- **Always** (any terminal reason): one ledger session-record, via `session-cost.py` + `register-task.py`, carrying `graph_node_id` + `provider_id` + scalar `session_id` + `cost_usd` + a new `termination_reason` field. Every provider session that touched a node leaves exactly one row, so a node's true cost and full session list roll up by grouping ledger entries on `graph_node_id`.
- **Ship only** (`DonePRGreen` / `DoneAdvisory`): plan stamp + graduate (`stamp-plan.py`) and a git-derived handoff artifact.

It is idempotent (a `session_finalized` event keyed by `session_id` short-circuits re-fires) and strictly non-fatal (a sub-step failure emits `session_finalize_failed` naming the step, runs the remaining steps, and never changes the completion decision). Nothing it writes is read by a future `loop-check` decision as a gate: the budget axis filters ledger cost by `session_id`, so a terminal write can only push the same terminating session toward termination, never away. A delegating self-handoff session writes its own ledger record (`termination_reason: delegated`, ledger-only) inside `handoff.sh` before manifest archival, because the shim's finalize cannot read the archived manifest. New event kinds: `session_finalized`, `session_finalize_failed`.

The control-plane collapse is complete: a code unit's completion authority is exactly three external reads (PR + CI + reviews) plus a budget ceiling. Nothing an agent wrote to a local file is ever a precondition of `<promise>`.
