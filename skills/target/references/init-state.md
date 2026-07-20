# Initialize State (Steps 1-3f)

**Load when:** starting a fresh target run (Step 1 onwards). All steps run sequentially before pipeline execution.

This reference covers config load, codemap, state initialization, input-type detection, cross-project detection, Linear auto-detection, plan validation, domain resolution, discovery gate, pre-execute checkpoint, and kill-criteria check.

## Step 1: Load Workspace Config (MANDATORY)

Check for `config.toml` in `.fno/` (project override) or `$HOME/.fno/` (primary). Extracts: `worktree_base`, testing auth shortcuts, project gotchas. Store config path and current project in target-state.md. If not found, cross-project features are disabled.

## Step 1a: Structural Context (AUTO)

Generate fresh codemap for structural awareness:

```bash
fno codemap --tokens 2048 2>/dev/null || true
```

If `fno` is unavailable or `fno codemap`'s deps are missing, skip this step silently. Read `.fno/codemap.md` if it exists. Top files in the output are the highest-importance nodes in the codebase - changes to these have wide blast radius and may need extra verification. This context informs the operator's execution and the sigma-review's risk assessment.

## Step 1b: Load Project Config (AUTO)

Source `scripts/lib/config.sh` and apply defaults for any unset flags:

- `expertise` → `get_config "expertise" ""`
- `max_iterations` → `get_config "max_iterations" "40"`
- `no_external` → `config_is_true "no_external"`
- `no_docs` → `config_is_true "no_docs"`
- `no_browser` → `config_is_true "no_browser"`
- `no_how_to` → `config_is_true "no_how_to"`
- `no_clean` → `config_is_true "no_clean"`
- `no_ship` → `config_is_true "no_ship"`

CLI flags always take precedence over config values.

## Step 1c: Resolve Size Profile (AUTO)

After loading config, resolve the size profile:

1. Check arguments for `-S`, `-M`, or `-L` (mutually exclusive, last wins)
2. If no size flag: read `default_size` from config.toml (`get_config "default_size" "M"`)
3. Load [size-profiles.md](size-profiles.md) and apply the resolved profile's flag template
4. Individual CLI flags and config values override profile values (CLI > config > profile > size default)

The size profile sets the base values for all toggles. Individual flags then override specific toggles on top. For example, `M adversarial` starts with the Medium profile and adds adversarial.

**Executor resolution:** If the resolved profile sets `executor: do`, invoke `/do` instead of `/do waves` for the execute phase. This is the key behavioral difference between Small and Medium/Large.

**Plan resolution:** If the resolved size is Small and `input_type == idea`, invoke `/blueprint quick` instead of `/blueprint` for the plan phase. Small features get a single-file plan that feeds into `/do`. Medium and Large get the full folder plan.

**Skip flag provenance (ENFORCED):** Flags are set by (in priority order):
1. CLI flags (`--no-external`, `--no-docs`) and positional modifiers (`adversarial`, `clean`)
2. Project config (`.fno/config.toml`)
3. Size profile (from resolved -S/-M/-L)

**FORBIDDEN:** Setting skip flags to `true` based on your own judgment (e.g., "this project doesn't need docs" or "no external review needed"). If no CLI flag, config, or size profile sets it, the phase MUST run.

## Step 1c-blast: Blast-Radius Modulation (AUTO, opt-in, x-518f)

When `config.target.blast.enabled: true` **and** the input is a plan or node (a File Ownership Map exists), `fno target init` performs a deterministic blast read on the plan's touched surface BEFORE it writes the immutable manifest, and modulates the size resolved in Step 1c. This is fully internal to the verb — there is no separate LLM step — but the agent should understand the resulting `target_size` may differ from the operator/default size, and an announce line is printed to stderr:

- **high blast** (touched surface matches the blast map: the loc-ratchet control-plane globs plus a general auth/migrations/sql/infra/billing list, extended by `config.target.blast.high_blast_globs`) → ceremony is **floored at `M`**, non-overridable downward even over an explicit `S`. Announce: `blast: high (<matched-path>) -> floor M ...`.
- **low blast** (all paths known, none match) **and no size was pinned** → **downgraded to `S`** (do + PR, fast path). An explicit operator size is never downgraded. Announce: `blast: low -> fast path S ...`. Suppressed when `config.target.blast.downgrade: false` (safety-only mode: floor up, never down).
- **unknown** (empty/unparseable map, classifier error, or `fno target blast-check` failure) → **no change**, fail-safe to the Step 1c size. A blast read never blocks init.

Disabled (the default) is byte-for-byte the pre-feature behavior. Inspect a plan's verdict directly with `fno target blast-check <plan>` (prints `{verdict, matched_paths, reason}`; `--quiet` for the bare token).

## Step 2: Initialize State

First, invoke the init helper with an explicit trigger so the state file is created (if missing) with fresh provider capability detection and scratchpad scaffolding. The helper is guarded: it only acts when `TARGET_START=1` is set, which prevents ambient tool uses from spawning stub state files in unrelated projects.

Pass `TARGET_INPUT` and (if the input is an existing plan path) `TARGET_PLAN_PATH` in the same invocation. The helper writes them into the initial state file so the stop hook's stub detector never mistakes a just-starting target for an abandoned empty state.

Also pass `TARGET_SIZE` (the size resolved in Step 1c — `S`, `M`, or `L`) so the helper renders the live skip-flag block and the `skip_flags_initial` provenance snapshot from the same source. This is REQUIRED for the snapshot to match the resolved profile; without it, the snapshot freezes to the legacy `M`-shaped defaults and the drift detector will block any later phase-skip the size profile actually requires (the failure mode that produced the BLOCKED-reject loop in inbox msg-b5312b / PR #500).

Pass `--beastmode` / `--beast` (or `TARGET_BEASTMODE=1`) when the invocation carried the `beastmode` / `beast` modifier, so init stamps `authority: full`. Without it the field is absent and the session keeps the default stop-and-ask posture. See [SKILL.md §Authority](../SKILL.md#authority-the-beastmode-grant).

Per-flag CLI overrides from Step 1c (e.g. `--no-docs`) can be layered on top via `TARGET_NO_EXTERNAL`, `TARGET_NO_DOCS`, `TARGET_NO_SHIP`, `TARGET_NO_BROWSER`, `TARGET_NO_CLEAN`, `TARGET_NO_HOW_TO`, `TARGET_NO_MEMORY` (any of `1` / `true` / `yes` to set the flag, empty / unset to keep the profile default).

```bash
TARGET_START=1 \
TARGET_INPUT="{user-input}" \
TARGET_PLAN_PATH="{plan-path}" \
TARGET_SIZE="{resolved-size-S-M-or-L}" \
bash "${CLAUDE_PLUGIN_ROOT}/hooks/helpers/init-target-state.sh"
```

Then read or overwrite `.fno/target-state.md` with the session-specific fields below. If a prior `status: COMPLETE` or `status: BLOCKED` file exists and you are starting a new run (not resuming), delete it before writing the fresh state so the helper does not short-circuit. See [state-schema.md](state-schema.md) for full schema.

> **Note (ab-d0337fbc):** The example below shows a PRE-WEDGE manifest. The current manifest has no `status`, `current_phase`, `iteration`, or `completion_gates`. See `references/state-schema.md` for the current field list.

```yaml
---
# PRE-WEDGE FORMAT - for historical reference only
input: "Add AI chat feature"
input_type: idea | plan
execution_mode: main | agent | fork  # How to dispatch tasks
status: IN_PROGRESS           # removed in ab-d0337fbc
current_phase: think          # removed in ab-d0337fbc
iteration: 1                  # removed in ab-d0337fbc
mode: interactive              # interactive | autonomous
---
```

## Step 3: Detect Input Type

First resolve any graph-ID argument via the shared helper so `ab-xxxxxxxx` is transparently swapped for the node's `plan_path` before input-type detection:

```bash
source "${SKILL_DIR}/scripts/lib/graph-resolve.sh"
input=$(resolve_arg "$input")
```

`resolve_arg` echoes non-ID arguments (paths, descriptions) unchanged and soft-fails on unknown IDs (stderr warning + echo arg as-is), so downstream detection stays identical whether the user supplied an ID, a path, or a feature description.

```bash
if [[ -f "$input" ]] || [[ -d "$input" ]]; then
  input_type="plan"   # Skip think/blueprint phases
  plan_path="$input"  # Set plan_path for artifact archival
else
  input_type="idea"   # Run full pipeline
  plan_path=null      # Set after /blueprint creates the plan
fi
```

**IMPORTANT:** Update `plan_path:` in target-state.md whenever the plan directory is known:
- For plan input: set immediately from the input argument
- For idea input: set after `/blueprint` creates the plan directory

The stop hook uses `plan_path` to archive the scratchpad (if any) to `scratchpad-archive/` inside the plan folder, and to drive `stamp-plan.py` on ship. Session-state files (HANDOFF.md, SUMMARY.md, STATE.md, target-state.md) are transient and are NOT archived - the plan frontmatter stamp, COMPLETION.md, ledger.json, and git history are the durable record.

## Step 3a: Cross-Project Is Retired (migration shim)

The `scope: cross-project` parallel-worktree pipeline has been removed.
There is no longer a cross-project detection step that forks a separate
pipeline. A session works only in its own project; multi-repo features are
modeled as one backlog node per project (linked by `blocked_by`), each
shipping its own PR. See the "CROSS-PROJECT IS RETIRED (migration shim)"
section in SKILL.md.

`fno target init` still persists `cross_project: false` by default (the
manifest schema field is retained for back-compat with already-stamped
legacy plans, whose graduation timing `fno-agents finalize` still honors).
A legacy plan carrying `scope: cross-project` (or a `cross-project`
subcommand) sets `cross_project: true`; that no longer forks the pipeline,
it only triggers the SKILL.md deprecation warning + spawn-into-project
routing.

## Step 3b: Linear Ticket Auto-Detection (if linear plugin installed)

For plan inputs: find `00-INDEX.md`, check for `linear:` field. If the linear plugin is installed and the field is missing, auto-create via `/linear --from-index`. Store `linear_ticket` and `linear_url` in target-state.md. If the linear plugin is not installed, skip this step.

## Step 3c: Pre-Execution Plan Validation (MANDATORY for plan inputs)

If `input_type == plan`, run the plan validator before spending tokens on execution:

```bash
bash "${CLAUDE_PLUGIN_ROOT}/scripts/validate-plan.sh" "$PLAN_DIR"
```

**On ERROR:** STOP. Report the validation errors. Do NOT proceed to /do. The plan needs fixes first.

**On WARN:** Log warnings in target-state.md under `## Plan Validation Warnings`, then proceed.

**On PASS:** Proceed to /do.

**Why here and not just in /blueprint?** Plans can be:
1. Created by `/blueprint` (validated at creation)
2. Written manually by the user
3. Created in a previous session
4. Modified after creation

Running validation at execution time catches all four cases.

**Quick-plan exception:** Single-file plans (no folder, no `00-INDEX.md`) cannot be validated by `validate-plan.sh` (which expects a folder structure). Skip validation for those and rely on the kill-criteria check instead.

## Step 3d: Domain Resolution (AUTO)

Read domain from the lookup chain:
1. `--domain` CLI flag (if provided in arguments)
2. Plan's `00-INDEX.md` `domain:` field
3. Settings: `config.default_domain` in config.toml
4. Default: `"code"`

```bash
source scripts/lib/config.sh
DOMAIN=$(resolve_domain "$FLAG_DOMAIN" "$PLAN_DOMAIN" "$(get_config 'default_domain' '')")
```

If `domain != "code"` and `domain_exists "$DOMAIN"` returns false, log a warning:
`"WARNING: unknown domain '$DOMAIN' — falling back to code defaults"`

Write resolved domain and phase mapping to target-state.md:
```yaml
domain: research
domain_phases:
  execute: fno:do waves
  review: fno:fact-check
  validate: "python3 scripts/verify-citations.py"
  ship: fno:publish-to-obsidian
  external: none
  docs: none
```

Phase resolution uses a three-level chain:
1. **Plan override**: `phases:` section in 00-INDEX.md (highest priority)
2. **Domain profile**: `domains.{name}.phases.{phase}` in config.toml
3. **Code default**: hardcoded in `_code_default_phase()` in config.sh

For each of the 6 phases (execute, review, validate, ship, external, docs):
- If plan has a `phases.{phase}` override → use that
- Else → `get_domain_phase "$DOMAIN" "$phase"` (handles domain → code fallback)

See [domain-profiles.md](domain-profiles.md) for full schema and examples.

## Step 3d2: Discovery Gate (idea input only)

When `input_type == idea`, run the discovery protocol between think and plan to surface unknowns before planning. This is the most important touch point - it prevents target from silently assuming its way through ambiguity.

```
think -> DISCOVERY GATE -> plan -> execute -> review -> ship
```

Load the protocol: `${SKILL_DIR}/../blueprint/references/discovery-gate.md`

| Mode | Condition | Discovery behavior |
|------|-----------|-----------------------|
| Interactive, idea input | (default) | Ask 3-5 questions, wait for answers |
| Autonomous, idea input | `mode: autonomous` | Self-answer from context, log assumptions |
| Any mode, plan input | `input_type: plan` | Skip (plan already exists) |
| Small size | `-S` | Skip (too lightweight for ceremony) |

For autonomous mode, self-answers are written as an `## Assumptions` section in target-state.md so the human can audit what target decided on its own.

**Skip detection:** If /think already produced a design doc with a `## Discovery` or `## Assumptions` section, skip this step. Check `scratchpad_path/think-findings.md` for these sections.

Update target-state.md:

```yaml
discovery:
  status: pending | answered | self_answered | skipped
  questions: []     # the questions asked
  answers: []       # human or self-generated answers
  skip_reason: null  # "plan_input" | "small_size" | "think_already_ran"
```

## Step 3e: Create Pre-Execute Checkpoint

Before invoking `/do waves`, create a git checkpoint so we can rollback if execution fails repeatedly:

```bash
source "${CLAUDE_PLUGIN_ROOT}/scripts/lib/checkpoint.sh"
CHECKPOINT=$(create_checkpoint "execute" "${CURRENT_WAVE:-0}")
```

Update target-state.md checkpoint section:
```yaml
checkpoint:
  latest_ref: stash@{0}              # from CHECKPOINT output
  latest_name: target-checkpoint-execute-wave-0
```

If `create_checkpoint` returns `:clean` suffix, no changes to stash - skip checkpoint tracking update.

## Step 3f: Kill Criteria Check (AUTO, every iteration)

Evaluate the plan's `kill_criteria:` block before executing the pipeline. If any predicate fires, emit `<aborted reason="{name}">` and stop - the stop hook treats `<aborted>` symmetrically to `<promise>`: clean exit, state archive, ledger entry tagged `aborted`.

```bash
PLAN_PATH=$(sed -n 's/^plan_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null | head -1 | tr -d '"')
if [[ -n "$PLAN_PATH" && -f "${CLAUDE_PLUGIN_ROOT}/scripts/lib/kill-criteria.sh" ]]; then
    source "${CLAUDE_PLUGIN_ROOT}/scripts/lib/kill-criteria.sh"
    if ! KC_OUT=$(check_kill_criteria "$PLAN_PATH"); then
        FIRED_LINE="${KC_OUT#KILL_CRITERIA_FIRED }"
        KC_NAME="${FIRED_LINE%%|*}"
        KC_REASON="${FIRED_LINE#*|}"
        echo "target: kill_criteria fired — $KC_NAME: $KC_REASON" >&2
    fi
fi
```

Evaluator is backward compatible: plans without a `kill_criteria:` block return exit 0 (no abort). Malformed predicates log WARN to stderr and are skipped (never abort on an unparseable criterion).

When an abort fires, the agent's user-facing text MUST include the literal tag `<aborted reason="{name}">MISSION ABORTED: {reason}</aborted>` in place of the `<promise>` tag. Do NOT output BOTH tags - if both appear, the stop hook treats the abort as winning (safer default) and logs the ambiguity.

See [kill-criteria.md](kill-criteria.md) for predicate syntax and examples.
