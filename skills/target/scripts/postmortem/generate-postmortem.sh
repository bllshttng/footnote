#!/usr/bin/env bash
# generate-postmortem.sh - produce a structured postmortem when target BLOCKED.
#
# Reads target-state.md plus available phase artifacts and writes a single
# markdown file at ~/.fno/postmortems/{YYYY-MM-DD}-{session_id_short}.md.
# The format is fixed by references/postmortem-format.md.
#
# Contract:
#   - Never crashes on partial input. Missing fields become "unknown".
#   - Append-only. On filename collision, appends .2, .3 suffix.
#   - Best-effort. Non-zero exit only on catastrophic failure (cannot
#     create output directory, cannot write any file at all). Even then,
#     writes a diagnostic line to stderr so the calling hook can log it.
#   - Stdout prints the path of the generated postmortem (or empty on
#     catastrophic failure). The calling hook captures this for its
#     handoff message.
#
# Usage:
#   generate-postmortem.sh [--state-file PATH] [--output-dir PATH] [--session-id SID]
#
# Defaults:
#   --state-file:   .fno/target-state.md (relative to git root)
#   --output-dir:   ~/.fno/postmortems/
#   --session-id:   read from state-file's session_id: field

set -uo pipefail
# Intentionally NOT set -e: partial-input tolerance is the whole point.

STATE_FILE_ARG=""
OUTPUT_DIR_ARG=""
SESSION_ID_ARG=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --state-file)
            STATE_FILE_ARG="${2:-}"; shift 2 ;;
        --output-dir)
            OUTPUT_DIR_ARG="${2:-}"; shift 2 ;;
        --session-id)
            SESSION_ID_ARG="${2:-}"; shift 2 ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *)
            echo "generate-postmortem: unknown arg: $1" >&2
            exit 2 ;;
    esac
done

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
STATE_FILE="${STATE_FILE_ARG:-$REPO_ROOT/.fno/target-state.md}"
# Use POSTMORTEMS_DIR from paths.sh if available; fall back to hardcoded default.
if [[ -z "${POSTMORTEMS_DIR:-}" ]] && command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
OUTPUT_DIR="${OUTPUT_DIR_ARG:-${POSTMORTEMS_DIR:-$HOME/.fno/postmortems}}"
CORRECTIONS_LOG="${POSTMORTEM_CORRECTIONS_LOG:-$HOME/.claude/corrections.log}"

# ── Helpers ─────────────────────────────────────────────────────────────

# yaml_field <key>: extract a top-level YAML key from the state file's
# frontmatter. Returns empty string when missing. Handles quoted and
# unquoted values, strips surrounding whitespace, drops a trailing
# `# comment`. Does NOT handle nested keys - state file is flat-ish.
yaml_field() {
    local key="$1"
    [[ -f "$STATE_FILE" ]] || return 0
    awk -v key="$key" '
        NR == 1 && $0 == "---" { in_fm = 1; next }
        in_fm && $0 == "---" { exit }
        in_fm && $0 ~ "^" key ":" {
            sub("^" key ":[[:space:]]*", "", $0)
            sub("[[:space:]]*#.*$", "", $0)
            gsub("^\"|\"$", "", $0)
            gsub("^'\''|'\''$", "", $0)
            print
            exit
        }
    ' "$STATE_FILE"
}

# iso_now: current UTC timestamp in seconds-precision ISO-8601.
iso_now() {
    date -u +%Y-%m-%dT%H:%M:%SZ
}

# epoch_now: current epoch seconds. Defensive against missing date(1).
epoch_now() {
    date -u +%s 2>/dev/null || echo 0
}

# iso_to_epoch <iso>: convert ISO-8601 to epoch seconds. Returns 0 on
# parse failure. Handles both `+00:00` and `Z` zone suffixes.
# BSD date -j -f doesn't honor zone info in the trimmed form, so set
# TZ=UTC for the parse - otherwise duration math is off by the local
# UTC offset (8 hours during PST).
iso_to_epoch() {
    local iso="$1"
    [[ -z "$iso" ]] && { echo 0; return; }
    if date -d "$iso" +%s 2>/dev/null; then
        return
    fi
    local trimmed="${iso%Z}"
    trimmed="${trimmed%%+*}"
    trimmed="${trimmed%%.*}"
    if TZ=UTC date -j -f "%Y-%m-%dT%H:%M:%S" "$trimmed" +%s 2>/dev/null; then
        return
    fi
    echo 0
}

# session_short <sid>: first 8 chars of a session_id.
session_short() {
    local sid="${1:-}"
    [[ -z "$sid" ]] && { echo "unknown-$(epoch_now)"; return; }
    echo "${sid:0:8}"
}

# log_stderr <msg>: write a diagnostic line. Always best-effort.
log_stderr() {
    echo "generate-postmortem: $*" >&2
}

# hypothesis_for_kind <kind>: print the 1-3 numbered hypothesis paragraphs
# for a given blocked_reason kind. Lookup table mirrors
# references/postmortem-format.md.
hypothesis_for_kind() {
    local kind="${1:-other}"
    case "$kind" in
        test_failure)
            cat <<'EOF'
1. Test setup may have drifted from production: fixture or seed data could be stale after a recent migration.
2. A recently changed dependency may have broken the suite: check the lockfile and recent dependency updates.
3. Flake risk: rerun once before deeper investigation; intermittent failures often resolve on retry.
EOF
            ;;
        build_failure)
            cat <<'EOF'
1. Compile error from a recent edit: re-run the build locally and inspect the first error.
2. Dependency version drift: confirm lockfile matches installed versions; reinstall if needed.
3. Generated file (proto/codegen/types) out of date: run the project's regeneration step.
EOF
            ;;
        auth_failure)
            cat <<'EOF'
1. gh CLI session expired: re-run `gh auth login` and verify with `gh auth status`.
2. Missing scope on the token (e.g. `repo` for pushes): re-authenticate with the required scopes.
3. OAuth redirect URI mismatch on a non-default base: confirm the GitHub App or token config.
EOF
            ;;
        plan_outdated)
            cat <<'EOF'
1. Files moved or renamed since the plan was authored: re-read the plan and update file paths.
2. A blocking dependency was reordered: re-check the graph dependencies for this node.
3. The plan's expected post-conditions no longer match repo state: revise the plan before retrying.
EOF
            ;;
        review_blocked)
            cat <<'EOF'
1. sigma-review surfaced a blocking issue not yet addressed: read the review artifact for details.
2. sigma-review found a regression in code we just shipped: roll back or fix the regression first.
3. sigma-review surfaced a missing test: add the test and rerun.
EOF
            ;;
        external_review_pending)
            cat <<'EOF'
1. Gemini reviewer is still processing (median 7m, max ~4h): wait and re-poll rather than re-triggering.
2. PR has changed since review started; the reviewer is waiting for a stable state.
3. External reviewer is rate-limited: check `gh api rate_limit` for the bot account.
EOF
            ;;
        scope_creep)
            cat <<'EOF'
1. Implementation drifted from plan: list files outside plan_path and consider reverting unrelated changes.
2. Refactor temptation: the work expanded beyond the original ask; defer adjacent cleanup to a follow-up.
3. Missing planning step for an adjacent feature: file a new backlog node and stop here.
EOF
            ;;
        cost_exceeded)
            cat <<'EOF'
1. Too many iterations on the same problem; the root cause is unclear; rotate strategy or involve a human.
2. The plan was under-scoped for the model used; consider splitting the task into smaller pieces.
3. Consider rotating to a more capable model for the remainder of the work.
EOF
            ;;
        iteration_ceiling|thrash)
            cat <<'EOF'
1. Thrash detector tripped; planning is likely wrong for the current task size.
2. The same fingerprint repeated 5+ times; the LLM is stuck on a single approach.
3. Split the task into smaller backlog nodes; current scope appears too large for one target run.
EOF
            ;;
        no_gate_progress)
            cat <<'EOF'
1. Cosmetic-commit churn detected; commits advanced but no completion gate flipped.
2. The LLM is making changes without satisfying any phase contract; check that gate writers are firing.
3. A phase invocation may be silently no-op'ing; inspect the most recent phase artifact for emptiness.
EOF
            ;;
        repeated_help_no_progress)
            cat <<'EOF'
1. LLM emitted <help> N consecutive times without intervening progress; the help-escalation detector tripped.
2. The reasons cited in the help emissions need direct user action; address them and rerun.
3. The session pattern suggests the task itself is unworkable in the current shape; consider rescoping.
EOF
            ;;
        model_fallback_exhausted)
            cat <<'EOF'
1. All configured providers returned errors; check provider rotation queue health.
2. Verify API keys and quota for each provider in `~/.fno/settings.yaml`.
3. The error class may be unrecoverable on every provider (e.g. invalid request shape); check error normalization.
EOF
            ;;
        verifier_failure)
            cat <<'EOF'
1. A phase verifier caught silent under-delivery; inspect the verifier reason JSON for the missing artifact.
2. Phase artifact present but content invalid; the producer skill drifted from the contract.
3. Phase contract violated; the responsible skill needs an update or the plan was misinterpreted.
EOF
            ;;
        user_cancel)
            cat <<'EOF'
1. User actively cancelled via the sentinel file or TARGET_CANCEL env var; intentional pause.
2. No autonomous follow-up needed; wait for the user to re-engage.
EOF
            ;;
        circuit_breaker)
            cat <<'EOF'
1. Circuit breaker tripped after 3 same-error failures; the current approach is wrong.
2. Rotate strategy; consider involving a fresh agent or a different tool path.
3. The error class may indicate a structural issue not addressable by retry.
EOF
            ;;
        rollback_exhausted)
            cat <<'EOF'
1. Validation rolled back 3+ times without finding a working checkpoint.
2. The plan needs to be redesigned; the current task decomposition is fragile.
3. Consider a more incremental story breakdown so each rollback target is closer.
EOF
            ;;
        environment)
            cat <<'EOF'
1. Shell or tool failure unrelated to code: investigate the environment precondition that failed.
2. Disk space, permission, or auth precondition missing; the preflight checks surface these explicitly.
EOF
            ;;
        *)
            cat <<'EOF'
1. Failure mode not in the BLOCKED taxonomy; check `target-state.md` and `.fno/target-stop-hook.log` for context.
2. The blocked_reason was not set or was set to a custom value; the calling skill may need to adopt the taxonomy.
EOF
            ;;
    esac
}

# ── Read state ──────────────────────────────────────────────────────────

if [[ ! -f "$STATE_FILE" ]]; then
    log_stderr "state file not found at $STATE_FILE; writing minimum-info postmortem"
fi

SESSION_ID="${SESSION_ID_ARG:-$(yaml_field session_id)}"
[[ -z "$SESSION_ID" ]] && SESSION_ID="unknown-$(epoch_now)"
# Validate against path traversal - SESSION_ID flows into OUT_PATH via
# session_short. A malicious --session-id like "../../etc" would escape
# the output dir. Allow only alphanumerics, dot, underscore, hyphen.
# Memory: feedback_project_name_path_traversal.md.
if ! [[ "$SESSION_ID" =~ ^[A-Za-z0-9._-]+$ ]]; then
    log_stderr "session_id contains unsafe characters; falling back to unknown-<epoch>"
    SESSION_ID="unknown-$(epoch_now)"
fi

CURRENT_PHASE="$(yaml_field current_phase)"
[[ -z "$CURRENT_PHASE" ]] && CURRENT_PHASE="unknown"

ITERATION_RAW="$(yaml_field iteration)"
ITERATION="${ITERATION_RAW:-unknown}"

PLAN_PATH="$(yaml_field plan_path)"
INPUT_VAL="$(yaml_field input)"
CREATED_AT="$(yaml_field created_at)"

# blocked_reason can be either a plain scalar (e.g. "stuck:thrash") or
# a structured kind. Strip "stuck:" prefix when present so kind aligns
# with the blocked-taxonomy.sh enum.
BR_RAW="$(yaml_field blocked_reason)"
BR_DETAILS="$(yaml_field blocked_reason_details)"
BR_AXIS="$(yaml_field blocked_reason_axis)"
BR_KIND="${BR_RAW#stuck:}"
BR_KIND="${BR_KIND%%:*}"
case "$BR_KIND" in
    thrash|no_gate_progress|repeated_help_no_progress|budget_exceeded)
        # Stuck-detector kinds keep their original kind in BR_RAW for
        # details, and only budget_exceeded normalizes to a taxonomy
        # alias (cost_exceeded). thrash, no_gate_progress, and
        # repeated_help_no_progress have their own hypothesis arms
        # in hypothesis_for_kind so the autocorrect signal is preserved.
        [[ -z "$BR_DETAILS" ]] && BR_DETAILS="stuck-detector: $BR_RAW"
        case "$BR_KIND" in
            budget_exceeded) BR_KIND="cost_exceeded" ;;
        esac
        ;;
    user)
        # user:env_cancel, user:sentinel -> user_cancel
        BR_KIND="user_cancel"
        [[ -z "$BR_DETAILS" ]] && BR_DETAILS="$BR_RAW"
        ;;
    "")
        BR_KIND="other"
        ;;
esac

TRIP_SIGNAL="null"
case "$BR_KIND" in
    user_cancel|circuit_breaker|rollback_exhausted)
        TRIP_SIGNAL="$BR_KIND" ;;
esac

# cost + duration
COST_RAW="$(yaml_field total_cost_usd)"
COST="${COST_RAW:-unknown}"

NOW_ISO="$(iso_now)"
NOW_EPOCH="$(epoch_now)"
CREATED_EPOCH="$(iso_to_epoch "$CREATED_AT")"
if [[ -n "$CREATED_AT" && "$CREATED_EPOCH" -gt 0 ]]; then
    DURATION_MIN=$(( (NOW_EPOCH - CREATED_EPOCH) / 60 ))
    [[ "$DURATION_MIN" -lt 0 ]] && DURATION_MIN="unknown"
else
    DURATION_MIN="unknown"
fi

# restart_count: archived state files for the same input (best-effort).
# Use find rather than glob+ls so unmatched globs don't get counted as
# literal-pattern "files" (the old shellcheck-suppressed ls form was
# off-by-one on no-match).
RESTART_COUNT=0
if [[ -n "$INPUT_VAL" ]]; then
    STATE_DIR="$(dirname "$STATE_FILE")"
    if [[ -d "$STATE_DIR" ]]; then
        RESTART_COUNT=$(find "$STATE_DIR" -maxdepth 1 \
            \( -name 'target-state.prior-*' -o -name 'target-state.archived-*' \) \
            2>/dev/null | wc -l | tr -d ' ')
        [[ -z "$RESTART_COUNT" ]] && RESTART_COUNT=0
    fi
fi

# Reconstruct invocation hint (best-effort).
SIZE_HINT="$(grep -E "^size:" "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^size:[[:space:]]*//')"
case "$SIZE_HINT" in
    S|s) MODE="small" ;;
    M|m) MODE="medium" ;;
    L|l) MODE="large" ;;
    *)   MODE="" ;;
esac

INVOCATION=""
if [[ -n "$INPUT_VAL" ]]; then
    if [[ -n "$SIZE_HINT" ]]; then
        INVOCATION="/target ${SIZE_HINT} ${INPUT_VAL}"
    else
        INVOCATION="/target ${INPUT_VAL}"
    fi
fi

# ── Build phase timeline from handoff artifacts ─────────────────────────

HANDOFF_DIR="$REPO_ROOT/.fno/artifacts/handoff"
PHASE_ARTIFACT_DIR="$REPO_ROOT/.fno/artifacts"

# Canonical phase order. Phases not present get skipped.
ALL_PHASES="think plan do clean review validate ship external docs"

# Build a temp file of one row per phase that has an artifact for this session_id.
# Format: phase|mtime_epoch|status|notes
TIMELINE_TMP="$(mktemp 2>/dev/null)" || TIMELINE_TMP=""
if [[ -n "$TIMELINE_TMP" ]]; then
    trap 'rm -f "$TIMELINE_TMP"' EXIT
else
    log_stderr "mktemp failed for timeline scratch; phase timeline will be reported as empty"
fi

found_any=0
for phase in $ALL_PHASES; do
    artifact_path=""
    # Handoff dir is the canonical source per target-reliability-core
    candidate="$HANDOFF_DIR/${phase}-${SESSION_ID}.md"
    if [[ -f "$candidate" ]]; then
        artifact_path="$candidate"
    fi
    # Fall back to the gate-artifact dir for phases without a handoff
    # (some phases use the gate path directly: validate, review, ship).
    if [[ -z "$artifact_path" ]]; then
        candidate="$PHASE_ARTIFACT_DIR/${phase}-${SESSION_ID}.md"
        [[ -f "$candidate" ]] && artifact_path="$candidate"
    fi
    [[ -z "$artifact_path" ]] && continue

    if mtime_epoch=$(stat -f "%m" "$artifact_path" 2>/dev/null); then
        :
    elif mtime_epoch=$(stat -c "%Y" "$artifact_path" 2>/dev/null); then
        :
    else
        mtime_epoch=0
    fi
    [[ -z "$mtime_epoch" ]] && mtime_epoch=0

    status="complete"
    [[ "$phase" == "$CURRENT_PHASE" ]] && status="BLOCKED"

    # Pull a one-line note from the artifact body.
    note=$(awk '
        BEGIN { in_fm = 0; closed = 0 }
        NR == 1 && $0 == "---" { in_fm = 1; next }
        in_fm && $0 == "---" { in_fm = 0; closed = 1; next }
        !closed { next }
        /^(stories_completed|verdict|notes_for_next_phase|notes_for_operator|notes_for_review|review_status|pr_number|blocking_issues):/ {
            sub(":[[:space:]]*", ": ", $0)
            print
            exit
        }
    ' "$artifact_path" 2>/dev/null | head -c 200)
    [[ -z "$note" ]] && note="-"

    if [[ -n "$TIMELINE_TMP" ]]; then
        echo "${phase}|${mtime_epoch}|${status}|${note}" >> "$TIMELINE_TMP"
        found_any=1
    fi
done

# ── Capture last 50 lines of failed phase output ────────────────────────

FAILED_OUTPUT=""

# Source priority 1: a body section of the failing phase's artifact (if any).
if [[ -n "$CURRENT_PHASE" && "$CURRENT_PHASE" != "unknown" ]]; then
    for d in "$HANDOFF_DIR" "$PHASE_ARTIFACT_DIR"; do
        p="$d/${CURRENT_PHASE}-${SESSION_ID}.md"
        if [[ -f "$p" ]]; then
            FAILED_OUTPUT=$(awk '/^---$/ { fm_count++; next } fm_count >= 2 { print }' "$p" 2>/dev/null | tail -50)
            [[ -n "$FAILED_OUTPUT" ]] && break
        fi
    done
fi

# Source priority 2: target-stop-hook.log tail.
HOOK_LOG="$REPO_ROOT/.fno/target-stop-hook.log"
if [[ -z "$FAILED_OUTPUT" && -f "$HOOK_LOG" ]]; then
    FAILED_OUTPUT=$(tail -50 "$HOOK_LOG" 2>/dev/null)
fi

# Detect truncation. We don't know how many lines were elided unless we
# can compare to source line count - approximate via the source.
TRUNCATED=""
if [[ -n "$FAILED_OUTPUT" ]] && [[ -f "$HOOK_LOG" ]]; then
    SRC_LINES=$(wc -l < "$HOOK_LOG" 2>/dev/null | tr -d ' ')
    if [[ -n "$SRC_LINES" && "$SRC_LINES" -gt 50 ]]; then
        ELIDED=$((SRC_LINES - 50))
        TRUNCATED="... ($ELIDED lines elided) ..."
    fi
fi

[[ -z "$FAILED_OUTPUT" ]] && FAILED_OUTPUT="(no captured output)"

# ── Resolve output path with collision handling ─────────────────────────

if ! mkdir -p "$OUTPUT_DIR" 2>/dev/null; then
    log_stderr "cannot create output dir $OUTPUT_DIR; aborting"
    exit 1
fi

DATE_PART="${NOW_ISO%T*}"
SHORT_SID="$(session_short "$SESSION_ID")"
OUT_PATH="$OUTPUT_DIR/${DATE_PART}-${SHORT_SID}.md"

SUFFIX=1
while [[ -e "$OUT_PATH" ]]; do
    SUFFIX=$((SUFFIX + 1))
    OUT_PATH="$OUTPUT_DIR/${DATE_PART}-${SHORT_SID}.${SUFFIX}.md"
    # Hard cap so a runaway can't loop forever.
    if [[ "$SUFFIX" -gt 99 ]]; then
        log_stderr "filename collision exceeded 99 retries at $OUT_PATH; aborting"
        exit 1
    fi
done

# ── Write postmortem ────────────────────────────────────────────────────

# Atomic temp+mv pattern: write to a sibling temp file, verify the
# trailing marker landed, then rename into place. Guards against
# half-written postmortems when the disk fills mid-write or the
# process is signaled.
PM_TMP="${OUT_PATH}.partial.$$"
STATE_FILE_MISSING="false"
[[ ! -f "$STATE_FILE" ]] && STATE_FILE_MISSING="true"

{
    echo "---"
    echo "type: target-postmortem"
    echo "session_id: $SESSION_ID"
    echo "generated_at: $NOW_ISO"
    if [[ -n "$INVOCATION" ]]; then
        echo "target_invocation: \"$INVOCATION\""
    fi
    if [[ -n "$PLAN_PATH" ]]; then
        echo "plan_path: $PLAN_PATH"
    fi
    if [[ -n "$MODE" ]]; then
        echo "mode: $MODE"
    fi
    if [[ "$STATE_FILE_MISSING" == "true" ]]; then
        # Surface "state file vanished" as a distinct signal so the
        # autocorrect reviewer can dedupe from "session never started".
        echo "state_file_missing: true"
    fi
    echo "blocked_phase: ${CURRENT_PHASE:-unknown}"
    echo "blocked_reason:"
    echo "  kind: $BR_KIND"
    echo "  trip_signal: $TRIP_SIGNAL"
    if [[ -n "$BR_DETAILS" ]]; then
        # YAML-safe quote: double-quote and escape any internal quotes.
        ESC_DETAILS="${BR_DETAILS//\"/\\\"}"
        echo "  details: \"$ESC_DETAILS\""
    fi
    if [[ -n "$BR_AXIS" ]]; then
        echo "  axis: $BR_AXIS"
    fi
    echo "  source_phase: ${CURRENT_PHASE:-unknown}"
    echo "  iteration: $ITERATION"
    echo "iteration_count: $ITERATION"
    echo "restart_count: $RESTART_COUNT"
    echo "cost_usd: $COST"
    echo "duration_minutes: $DURATION_MIN"
    echo "---"
    echo ""
    echo "# Postmortem: $SHORT_SID"
    echo ""
    echo "## Phase timeline"
    echo ""
    echo "| Phase | Status | Duration | Notes |"
    echo "|---|---|---|---|"
    if [[ "$found_any" -eq 1 ]]; then
        # Sort by mtime, compute deltas, print rows.
        sort -t'|' -k2,2n "$TIMELINE_TMP" | awk -F'|' -v prev=0 '
            {
                phase = $1; mtime = $2; status = $3; notes = $4
                if (prev == 0 || mtime <= prev) {
                    dur = "unknown"
                } else {
                    secs = mtime - prev
                    mins = int(secs / 60)
                    if (mins == 0) dur = secs "s"
                    else dur = mins "m"
                }
                printf "| %s | %s | %s | %s |\n", phase, status, dur, notes
                prev = mtime
            }
        '
    else
        echo "| init | BLOCKED | unknown | no phase artifacts found |"
    fi
    echo ""
    echo "## Last output of failed phase"
    echo ""
    echo '```'
    if [[ -n "$TRUNCATED" ]]; then
        echo "$TRUNCATED"
    fi
    echo "$FAILED_OUTPUT"
    echo '```'
    echo ""
    echo "## Hypotheses"
    echo ""
    hypothesis_for_kind "$BR_KIND"
    # Trailing sentinel - lets the post-write validation confirm the
    # write completed instead of catching only the empty-file case.
    echo ""
    echo "<!-- postmortem-end -->"
} > "$PM_TMP" 2>>"${POSTMORTEM_ERR_LOG:-/dev/null}" || true
# Disk-full or write-failure shows up as empty file OR missing sentinel.
# Both checks together cover the failure modes ($? after a brace group
# refers to the last command, not the redirect - so we test the artifact
# directly instead).
if [[ ! -s "$PM_TMP" ]]; then
    log_stderr "postmortem write produced empty file at $PM_TMP; aborting"
    rm -f "$PM_TMP"
    exit 1
fi

if ! tail -1 "$PM_TMP" | grep -qF '<!-- postmortem-end -->'; then
    log_stderr "postmortem $PM_TMP missing trailing sentinel; aborting (likely truncated write)"
    rm -f "$PM_TMP"
    exit 1
fi

if ! mv "$PM_TMP" "$OUT_PATH" 2>>"${POSTMORTEM_ERR_LOG:-/dev/null}"; then
    log_stderr "could not rename $PM_TMP to $OUT_PATH; aborting"
    rm -f "$PM_TMP"
    exit 1
fi

# ── Append to corrections.log (autocorrect link) ────────────────────────

# Best-effort: only if corrections.log exists (autocorrect feature creates it).
# Format: {timestamp} | S1 | target-postmortem | {path} | {kind}: {details_truncated}
# Don't redirect stderr - if the append fails, the autocorrect feedback loop
# (the whole reason this feature exists) silently breaks. The error message
# must reach the caller's $LOG_FILE so postmortem-vs-no-postmortem audits
# can find the cause.
if [[ -f "$CORRECTIONS_LOG" ]]; then
    DETAILS_TRUNC="${BR_DETAILS:0:80}"
    [[ -z "$DETAILS_TRUNC" ]] && DETAILS_TRUNC="-"
    if ! printf "%s | S1 | target-postmortem | %s | %s: %s\n" \
            "$NOW_ISO" "$OUT_PATH" "$BR_KIND" "$DETAILS_TRUNC" \
            >> "$CORRECTIONS_LOG"; then
        log_stderr "could not append to $CORRECTIONS_LOG (rc=$?); autocorrect link missing"
    fi
fi

echo "$OUT_PATH"
exit 0
