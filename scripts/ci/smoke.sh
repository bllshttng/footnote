#!/usr/bin/env bash
# scripts/ci/smoke.sh - the cli-ci smoke job's test/lint step registry.
#
# One ordered list of the smoke job's test and lint steps. The workflow
# (.github/workflows/cli-ci.yml) calls `bash scripts/ci/smoke.sh` instead of
# spelling ~40 `run:` steps inline, so "run what CI runs" is one command and
# local/CI parity is structural, not maintained. Environment PROVISIONING
# (checkout, setup-python, setup-uv, rust toolchain install, cargo cache, the
# system PyYAML install) stays in the yml - those are CI-runner concerns and
# the CI/local divergence there is deliberate. Everything a test needs at run
# time (uv sync/build, the fno-agents debug build) lives here.
#
# Modes:
#   (default)         fail-fast; exactly the pre-extraction CI semantics.
#   --keep-going      run every step, print a summary table, exit non-zero if
#                     any failed. Records failed step names for --retry-failed.
#   --only <glob>     run only steps whose name matches the shell glob.
#   --retry-failed    re-run the steps recorded by the last --keep-going run;
#                     full run if no (or a corrupt) record exists.
#   --list [--verbose]  print the registry (names; +cwd/cmd with --verbose) and
#                     exit without running anything.
#
# Prerequisites (asserted up front, exit 2 naming the missing one): `uv`,
# `python3` with `yaml` importable OR `uv` to supply it, and `cargo` when a
# selected step needs it.
# Never auto-installs at system level.
#
# Exit codes: 0 all selected steps passed (>=1 ran); 1 a step failed or zero
#   steps ran; 2 a missing prerequisite; 3 (reserved for preflight lock).
#
# Bash 3.2 compatible.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "smoke: not a git repo (run from inside the checkout)" >&2
    exit 2
}
cd "$REPO_ROOT"

# fno requires Python >=3.11, but a system `python3` may be an older build (macOS
# Xcode ships 3.9) that can't import fno (typing.TypeAlias / `str | None`) and
# silently breaks steps. Put the project venv's interpreter+tools first on PATH
# so every step's `python3`/`fno`/`pytest` resolves to the pinned 3.11+ env. The
# venv is created by the first step (`uv sync`); the entry is inert until then,
# and the only pre-sync python3 use is the yaml prereq, which the system 3.9
# satisfies. `PATH=""` degrade tests override this locally, so they still pass.
PATH="$REPO_ROOT/cli/.venv/bin:$PATH"; export PATH

# Failed-step record lives in whatever checkout runs the script. For preflight
# that is the preflight worktree, never the invoking worktree.
# SMOKE_FAILURE_RECORD overrides the location (test seam).
FAILURE_RECORD="${SMOKE_FAILURE_RECORD:-$REPO_ROOT/.fno/preflight-last-failures.txt}"

# ----------------------------------------------------------------------------
# Registry. `step "name" "cwd" 'cmd'` appends one entry; cwd is repo-relative
# ("." = repo root). Multi-command steps embed newlines and run under
# `bash -eo pipefail` to match GitHub's default shell (any sub-command failing
# fails the step). Per-step env is folded into the command string.
# ----------------------------------------------------------------------------
STEP_NAMES=(); STEP_CWDS=(); STEP_CMDS=()
step() { STEP_NAMES+=("$1"); STEP_CWDS+=("$2"); STEP_CMDS+=("$3"); }

register_steps() {
    step "Sync + build" "cli" 'uv sync
uv build'
    step "Pytest (unit + integration)" "cli" 'uv run pytest --tb=short -q'
    step "paths.sh hash gate" "cli" 'uv run fno-py paths verify ../scripts/lib/paths.sh'
    step "Bash events-validate harness" "." 'bash tests/events/test-bash-validator.sh'
    step "frontend-craft gate harness (frontend-craft-gate plan)" "." 'bash tests/lib/test_frontend_surface.sh
bash tests/lib/test_infer_has_ui.sh
bash tests/lib/test_resolve_plan_executor.sh'
    step "config global-precedence harness (ab-5d6c3d47)" "." 'bash tests/lib/test_config_global_precedence.sh'
    # These extract fenced blocks out of skills/pr/references/merged.md and run
    # them, so they are the ONLY thing that executes the post-merge ritual's
    # shell. They sat unregistered, and a change to the Step 2 scan broke three
    # assertions without anything noticing. 77 is the harnesses' own "skipped,
    # no jq" code and must not read as a failure.
    step "post-merge ritual harness (merged.md Step 2 scan + Step 8a reap)" "." 'for t in tests/post-merge/test_reap_build_worker.sh tests/post-merge/test_watch.sh; do
  bash "$t" || { rc=$?; [ "$rc" = 77 ] && echo "skipped: $t (missing jq)" || exit "$rc"; }
done'
    step "cost-accuracy harness (ab-c0f92987)" "." 'uv run --project cli python tests/lib/test_cost_tracker_pricing.py
uv run --project cli python tests/metrics/test_session_cost_dedup.py
uv run --project cli python tests/metrics/test_backfill_cost_recompute.py
bash tests/lib/test_cost_tracker_sh_parity.sh'
    step "loop-check shim + immutable manifest harness (ab-d0337fbc)" "." 'bash tests/hooks/test_loop_check_shim.sh
bash tests/hooks/test_manifest_immutable.sh
bash tests/hooks/test_graph_write_protect.sh
bash tests/hooks/test_worktree_write_protect.sh
bash tests/hooks/test_worktree_harness_guard.sh
bash tests/hooks/test_setup_nudge_session_start.sh
bash tests/hooks/test_init_target_session_id.sh
bash tests/hooks/test_agy_stop_hook.sh
bash tests/hooks/test_check_impl_location.sh'
    step "worktree lifecycle: remove-hook contract + cwd-anchored liveness + ttyless archive + job reap (x-415c)" "." 'bash tests/hooks/test_worktree_remove_lifecycle.sh'
    step "in_review dispatch guard (x-2dc5)" "." 'bash tests/hooks/test_init_in_review_gate.sh'
    step "born-with-why offer inject hook harness" "." 'bash tests/hooks/test_born_with_why_offer_inject.sh'
    step "eval-sweep hygiene harness (x-dbdf: canonical stamp + singleton + timeout)" "." 'bash tests/hooks/test_eval_sweep_session_start.sh'
    step "ship-phase PR->node link verify guard (x-e106)" "." 'bash tests/target/test_ship_phase_link_verify.sh'
    step "docs-before-ship phase-ordering guard (ab-2e4a09f1)" "." 'bash tests/test-docs-before-ship.sh'
    step ".fno/ dir-hygiene harness (ab-d5a984f6)" "." 'python3 tests/metrics/test_completion_summary_path.py
bash tests/lib/test_rotate_append_log.sh
bash scripts/tests/test_prune_fno_dir.sh'
    step "corrections.log placement migration (ab-f063 Wave 2)" "." 'bash scripts/tests/test_corrections_migrate.sh'
    step "placement-rule lint self-test (ab-f063 Wave 2)" "." 'bash scripts/tests/test_check_placement_rule.sh'
    step "Build fno-agents debug binary (for journey tests)" "crates/fno-agents" 'cargo build'
    # Scoped @requires_rust coverage: the main pytest step runs with the binary
    # deleted so these skip (they would otherwise fail on a missing provider CLI
    # that CI lacks). Here the binary is present. test_ask_e2e_dispatch self-fakes
    # the providers it *executes*, but test_rust_verb_parity presence-checks a real
    # codex/gemini on PATH (resume/attach --print-command) without faking one, so
    # seed trivial stubs on PATH for the step; per-test fakes still prepend and win
    # wherever a provider is actually run. Reuses the debug build; no second compile.
    step "Scoped @requires_rust parity suites (binary present; stubbed providers)" "cli" 'FAKE_BIN="$(mktemp -d)"
for p in codex gemini; do printf "%s\n%s\n" "#!/bin/sh" "exit 0" > "$FAKE_BIN/$p"; chmod +x "$FAKE_BIN/$p"; done
PATH="$FAKE_BIN:$PATH" uv run pytest --tb=short -q tests/agents/test_rust_verb_parity.py tests/agents/test_ask_e2e_dispatch.py'
    # The heal is a Rust->Python shellout, so neither test tree can cover it
    # alone; this needs the debug binary (built above) and the cli venv.
    step "registry-miss heal across the Rust/Python seam (x-da8c)" "." 'bash tests/test-agents-heal-token.sh'
    step "Cross-impl claims compat matrix (merge gate; fails loudly, never skips here)" "cli" 'FNO_CLAIMS_COMPAT_REQUIRED=1 uv run pytest --tb=short -q tests/integration/test_claims_cross_impl.py'
    step "loop-check journey tests (e2e + emission-schema + backstop-subprocess)" "." 'bash tests/hooks/test_loop_check_e2e.sh
bash tests/events/test-loop-check-emission-schema.sh
bash tests/hooks/test_loop_check_backstop_subprocess.sh'
    step "megawalk-walk smoke test (task 2.4, ab-7303e5d7)" "." 'bash tests/smoke-megawalk-walk.sh'
    step "Target self-handoff harness (ab-534bcc55, ab-c2edd785)" "." 'bash tests/test-handoff.sh
bash tests/target/test_handoff_ledger_record.sh'
    step "Plan Mode front door harness (ab-09853cb6)" "." 'bash tests/hooks/test_capture_plan_mode.sh
bash tests/hooks/test_pending_plan_wipe.sh
bash tests/target/test_backfill_plan.sh
bash tests/target/test_detect_pending_plan.sh
bash tests/target/test_plan_mode_e2e.sh'
    step "bg-dispatch + ready-gated auto-launch harness (ab-e366539f)" "." 'bash tests/test-bg-dispatch.sh'
    step "agent skill harness (ab-4940daba)" "." 'bash tests/skills/test_agent_normalize.sh
bash tests/skills/test_agent_receipt.sh
# ab-994222ee: dashless bareword grammar + free-lane confirm posture
# (co-located with the skill per the design'"'"'s verify commands).
bash skills/agent/tests/test_normalize.sh
bash skills/agent/tests/test_confirm.sh
bash skills/agent/tests/test_auto_worktree.sh
bash skills/agent/tests/test_spawn_guard.sh'
    step "mail skill harness (ab-7479fdb2)" "." 'bash skills/mail/tests/test_normalize.sh'
    step "events-discipline lint" "." 'bash scripts/lint/events-discipline.sh'
    step "events-discipline lint self-test" "." 'bash tests/lint/test-events-discipline.sh'
    step "No quarantined events.invalid.jsonl rows" "." 'bash scripts/lint/no-invalid-events.sh'
    step "ruff (repo-wide) + mypy (path-config modules)" "cli" 'uv run ruff check --no-respect-gitignore src/
uv run mypy src/fno/paths.py src/fno/config/ src/fno/config_io.py'
    step "Smoke tests" "." 'bash cli/tests/smoke/run-all.sh'
    step "no hardcoded paths" "." 'bash scripts/ci/check-no-hardcoded-paths.sh'
    step "placement rule (ab-f063 Wave 2)" "." 'bash scripts/ci/check-placement-rule.sh'
    step "No residual old-name patterns (rename guard)" "." 'bash scripts/rename/residual-check.sh'
    step "No stale skill refs (consolidation audit)" "." 'bash scripts/ci/check-no-stale-skill-refs.sh'
    step "Skill snippet hazard lint (x-f47f)" "." 'bash scripts/ci/check-skill-snippets.sh'
    step "Skill snippet lint self-test (x-f47f)" "." 'bash tests/ci/test_check_skill_snippets.sh'
    step "No stale /spec refs (blueprint rename audit)" "." 'bash scripts/ci/check-no-stale-spec-refs.sh'
    step "Config schema docs freshness" "." 'bash scripts/ci/check-config-schema-drift.sh'
    step "Skill bundles freshness check" "." 'bash scripts/lint/check-skill-bundles-fresh.sh'
    step 'No \${REPO_ROOT}/scripts in skills' "." 'bash scripts/lint/no-repo-root-scripts-in-skills.sh'
    step "Marketplace-readiness lint (no Skill calls, no path escapes, fno declared)" "." 'bash scripts/lint/no-cross-skill-runtime-calls.sh'
    step "No unwrapped lib scripts" "." 'bash scripts/lint/no-unwrapped-lib-scripts.sh'
    step "pin-skill generator self-test (front-door pinning, AC6-FR)" "." 'bash tests/test-pin-skill.sh'
    step "Agent flock-pattern lint" "cli" 'uv run fno-py lint flock-pattern'
    step "Provider stderr-merge lint" "cli" 'uv run fno-py lint provider-stderr-merge'
    step "Repo-root shell-out drift guard (US4, clone-only allowlist)" "cli" 'uv run fno-py lint shellout-drift'
    step "In-N-Out menu-cap ratchet (x-71b6)" "cli" 'uv run fno-py lint menu-caps'
    step "Schema parity self-test" "." 'bash scripts/tests/check-event-schema-parity-selftest.sh'
    step "Schema parity check (Python side)" "." 'bash scripts/check-event-schema-parity.sh'
    step "Registry schema parity selftest (ab-0baecaed)" "." 'bash scripts/ci/check-registry-schema-parity.sh --selftest'
    step "Registry schema parity check (ab-0baecaed)" "." 'bash scripts/ci/check-registry-schema-parity.sh'
    # New (not part of the verbatim yml extraction): self-test the mode
    # machinery of this very script. Hermetic via the SMOKE_REGISTRY_FILE seam.
    step "smoke.sh mode machinery self-test" "." 'bash tests/ci/test_smoke_modes.sh'
    step "preflight.sh orchestration self-test" "." 'bash tests/ci/test_preflight.sh'
}

# ----------------------------------------------------------------------------
# Arg parsing
# ----------------------------------------------------------------------------
MODE="run"            # run | list
KEEP_GOING=0
RETRY_FAILED=0
VERBOSE=0
ONLY_GLOB=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --list) MODE="list" ;;
        --verbose) VERBOSE=1 ;;
        --keep-going) KEEP_GOING=1 ;;
        --retry-failed) RETRY_FAILED=1 ;;
        --only) shift; ONLY_GLOB="${1:-}"; [[ -z "$ONLY_GLOB" ]] && { echo "smoke: --only needs a glob" >&2; exit 2; } ;;
        --only=*) ONLY_GLOB="${1#--only=}" ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "smoke: unknown arg '$1'" >&2; exit 2 ;;
    esac
    shift
done

# SMOKE_REGISTRY_FILE (test seam): source an alternate register_steps so the
# mode machinery (keep-going / retry / only / summary) can be exercised against
# a tiny hermetic step set instead of the real 45.
if [[ -n "${SMOKE_REGISTRY_FILE:-}" ]]; then source "$SMOKE_REGISTRY_FILE"; fi
register_steps

# ----------------------------------------------------------------------------
# Selection: build SELECTED[] (indices into the registry) from the mode.
# ----------------------------------------------------------------------------
_name_matches() { [[ "$1" == $2 ]]; }  # $2 unquoted = glob match

read_failure_record() {
    # Prints recorded step names, one per line. Empty output = no usable record.
    [[ -f "$FAILURE_RECORD" ]] || return 0
    # A record line is usable iff it names a step still in the registry.
    while IFS= read -r rec; do
        [[ -z "$rec" ]] && continue
        local i
        for i in "${!STEP_NAMES[@]}"; do
            [[ "${STEP_NAMES[$i]}" == "$rec" ]] && { printf '%s\n' "$rec"; break; }
        done
    done < "$FAILURE_RECORD"
}

SELECTED=()
RETRY_FELL_BACK=0
if [[ $RETRY_FAILED -eq 1 ]]; then
    recorded="$(read_failure_record)"
    if [[ -n "$recorded" ]]; then
        while IFS= read -r rec; do
            for i in "${!STEP_NAMES[@]}"; do
                [[ "${STEP_NAMES[$i]}" == "$rec" ]] && SELECTED+=("$i")
            done
        done <<< "$recorded"
    else
        RETRY_FELL_BACK=1   # missing or corrupt record -> full run
        for i in "${!STEP_NAMES[@]}"; do SELECTED+=("$i"); done
    fi
elif [[ -n "$ONLY_GLOB" ]]; then
    for i in "${!STEP_NAMES[@]}"; do
        _name_matches "${STEP_NAMES[$i]}" "$ONLY_GLOB" && SELECTED+=("$i")
    done
    if [[ ${#SELECTED[@]} -eq 0 ]]; then
        echo "smoke: --only '$ONLY_GLOB' matched no steps. Available:" >&2
        for n in "${STEP_NAMES[@]}"; do echo "  $n" >&2; done
        exit 1
    fi
else
    for i in "${!STEP_NAMES[@]}"; do SELECTED+=("$i"); done
fi

# ----------------------------------------------------------------------------
# --list
# ----------------------------------------------------------------------------
if [[ "$MODE" == "list" ]]; then
    for i in "${SELECTED[@]}"; do
        if [[ $VERBOSE -eq 1 ]]; then
            printf '%s\n  cwd: %s\n  cmd: %s\n' "${STEP_NAMES[$i]}" "${STEP_CWDS[$i]}" "$(printf '%s' "${STEP_CMDS[$i]}" | tr '\n' ';')"
        else
            printf '%s\n' "${STEP_NAMES[$i]}"
        fi
    done
    exit 0
fi

# ----------------------------------------------------------------------------
# Prerequisites (only assert what the selected steps actually need).
# ----------------------------------------------------------------------------
_selected_cmds() { for i in "${SELECTED[@]}"; do printf '%s\n' "${STEP_CMDS[$i]}"; done; }
# grep, not grep -q: -q exits on first match and SIGPIPEs the upstream, which
# under pipefail makes the pipeline return 141 (false) even on a real match.
need_prereq() { _selected_cmds | grep -- "$1" >/dev/null; }

miss() { echo "smoke: missing prerequisite: $1 ($2)" >&2; exit 2; }
if need_prereq 'uv '; then command -v uv >/dev/null 2>&1 || miss uv "install from https://docs.astral.sh/uv"; fi
if need_prereq 'python3'; then
    command -v python3 >/dev/null 2>&1 || miss python3 "install Python 3"
    # PyYAML from the host interpreter OR from uv. Every yaml-consuming step
    # resolves it the same way (bundler, marketplace lint, events-validate) or
    # uses cli/.venv, which `uv sync` provisions in the first step. A host
    # without pyyaml is the norm, not an error: homebrew python3 is PEP 668
    # externally-managed, so `pip install` refuses there.
    python3 -c 'import yaml' 2>/dev/null || command -v uv >/dev/null 2>&1 \
        || miss "python3-yaml" "install pyyaml for python3, or install uv (https://docs.astral.sh/uv)"
fi
if need_prereq 'cargo'; then command -v cargo >/dev/null 2>&1 || miss cargo "install the Rust toolchain (rustup)"; fi

# ----------------------------------------------------------------------------
# Faithful-ordering guard for @requires_rust parity tests.
# The pre-extraction cli-ci restored the cargo cache AFTER the pytest step, so
# any fno-agents binary a prior run cached was NOT yet on disk when pytest ran -
# the @requires_rust parity tests (which need a compiled fno-agents binary AND a
# provider CLI like codex that CI lacks) were therefore SKIPPED. This extraction
# restores the cache in provisioning (before the script) for build speed, which
# would un-skip them and fail on "codex CLI not on PATH". Remove any pre-restored
# binary before pytest runs so the skip fires exactly as before; the dedicated
# "Build fno-agents debug binary" step recreates it for the later rust steps.
for i in "${SELECTED[@]}"; do
    if [[ "${STEP_NAMES[$i]}" == "Pytest (unit + integration)" ]]; then
        rm -f "$REPO_ROOT"/crates/fno-agents/target/debug/fno-agents \
              "$REPO_ROOT"/crates/fno-agents/target/release/fno-agents 2>/dev/null || true
        break
    fi
done

# ----------------------------------------------------------------------------
# Runner
# ----------------------------------------------------------------------------
in_ci() { [[ -n "${GITHUB_ACTIONS:-}" ]]; }
start_group() { if in_ci; then echo "::group::$1"; else printf '\n=== %s ===\n' "$1"; fi; }
end_group() { if in_ci; then echo "::endgroup::"; fi; }
err_annotate() { if in_ci; then echo "::error::smoke step failed: $1"; fi; }

RESULT_NAMES=(); RESULT_STATUS=(); RESULT_SECS=()
FAILED=0

run_one() {
    local idx="$1" name="${STEP_NAMES[$1]}" cwd="${STEP_CWDS[$1]}" cmd="${STEP_CMDS[$1]}"
    start_group "$name"
    local start="$SECONDS"
    ( cd "$REPO_ROOT/$cwd" && bash -eo pipefail -c "$cmd" )
    local rc=$?
    local dur=$(( SECONDS - start ))
    end_group
    RESULT_NAMES+=("$name"); RESULT_SECS+=("$dur")
    if [[ $rc -eq 0 ]]; then
        RESULT_STATUS+=("pass")
    else
        RESULT_STATUS+=("fail"); FAILED=$(( FAILED + 1 )); err_annotate "$name"
    fi
    return $rc
}

# Header (keep-going / retry / subset) so a partial run never reads as full.
print_header() {
    local n=${#SELECTED[@]} total=${#STEP_NAMES[@]} label="FULL"
    [[ $RETRY_FAILED -eq 1 ]] && label="RETRY SUBSET"
    [[ -n "$ONLY_GLOB" ]] && label="ONLY SUBSET"
    echo "smoke: mode=$label steps=$n/$total$( if [[ $KEEP_GOING -eq 1 ]]; then echo ' keep-going'; fi )"
    [[ $RETRY_FELL_BACK -eq 1 ]] && echo "smoke: no usable failure record - falling back to FULL run"
    if [[ $n -ne $total ]]; then
        echo "smoke: SUBSET run - run 'scripts/ci/smoke.sh' full before the settle-green push"
    fi
}

print_summary() {
    echo ""
    echo "smoke: summary ($( [[ $RETRY_FAILED -eq 1 ]] && echo retry-subset || { [[ -n "$ONLY_GLOB" ]] && echo only-subset || echo full; } ), ${#RESULT_NAMES[@]} steps)"
    local i
    for i in "${!RESULT_NAMES[@]}"; do
        printf '  %-6s %4ss  %s\n' "${RESULT_STATUS[$i]}" "${RESULT_SECS[$i]}" "${RESULT_NAMES[$i]}"
    done
}

record_failures() {
    : > "$FAILURE_RECORD"
    local i
    for i in "${!RESULT_NAMES[@]}"; do
        [[ "${RESULT_STATUS[$i]}" == "fail" ]] && printf '%s\n' "${RESULT_NAMES[$i]}" >> "$FAILURE_RECORD"
    done
}

print_header
mkdir -p "$(dirname "$FAILURE_RECORD")"

if [[ ${#SELECTED[@]} -eq 0 ]]; then
    echo "smoke: zero steps selected - never green" >&2
    exit 1
fi

if [[ $KEEP_GOING -eq 1 ]]; then
    for i in "${SELECTED[@]}"; do run_one "$i" || true; done
    print_summary
    record_failures
    [[ $FAILED -gt 0 ]] && exit 1
    exit 0
else
    # fail-fast: pre-extraction CI semantics.
    for i in "${SELECTED[@]}"; do
        run_one "$i" || { echo "smoke: step failed, stopping (fail-fast): ${STEP_NAMES[$i]}" >&2; exit 1; }
    done
    exit 0
fi
