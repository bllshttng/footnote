#!/usr/bin/env bash
# archive-artifacts.sh — resolve plan_dir for artifact archival.
#
# For a target session's plan_path, resolve where COMPLETION.md /
# scratchpad-archive/ should live:
#   - folder plan (plan_path is a directory): artifacts live inside the folder
#   - file plan (plan_path is a file): artifacts live in a `{plan_path}.artifacts/`
#     sidecar folder, created on demand
#
# Session-state files (HANDOFF.md, SUMMARY.md, STATE.md, target-state.md) are
# transient and are NOT archived here. The plan frontmatter stamp, COMPLETION.md,
# ledger.json, and git history are the durable record.
#
# This is sourced by hooks/target-stop-hook.sh at runtime and by
# tests/helpers/drive-target-archive.sh for surgical testing.
#
# Requires the following to be set in the caller's scope:
#   REPO_ROOT  - repository root (for .fno/ artifacts)
#   LOG_FILE   - path to write warnings (optional; falls back to stderr)
#   log()      - logging function (optional; falls back to echo to $LOG_FILE)

# Defensive fallback for log() when the archive function is used outside the
# target stop-hook context (e.g. test drivers that intentionally stay minimal).
# Using `declare -F` here is deliberate: `command -v log` on macOS matches the
# system `/usr/bin/log` binary and would hide that the caller never supplied
# a shell function, silently routing our messages to macOS's syslog CLI.
if ! declare -F log >/dev/null 2>&1; then
    log() {
        if [[ -n "${LOG_FILE:-}" ]]; then
            echo "[archive-artifacts] $*" >>"$LOG_FILE" 2>/dev/null || true
        else
            echo "[archive-artifacts] $*" >&2
        fi
    }
fi

# Resolve plan_dir for a given plan_input.
# Echoes the resolved plan_dir on stdout. Empty string means "do not archive".
# Exit code: always 0. Warnings logged via log().
resolve_plan_dir() {
    local plan_input="$1"
    local plan_dir=""

    if [[ -z "$plan_input" ]]; then
        return 0
    fi

    if [[ -d "$plan_input" ]]; then
        plan_dir="$plan_input"
    elif [[ -f "$plan_input" ]]; then
        plan_dir="${plan_input}.artifacts"
        if ! mkdir -p "$plan_dir" 2>>"${LOG_FILE:-/dev/null}"; then
            log "WARNING: failed to create sidecar $plan_dir; skipping archival"
            plan_dir=""
        fi
    else
        log "WARNING: plan_path does not exist as file or directory: $plan_input"
    fi

    echo "$plan_dir"
}

# _archive_artifacts state_file
# Resolves plan_dir and archives the scratchpad (if present) to scratchpad-archive/.
# Session-state files (HANDOFF.md, SUMMARY.md, STATE.md, target-state.md) are NOT
# copied - they are transient.
# Sets the caller-visible global TARGET_PLAN_DIR to the resolved plan_dir so
# downstream logic (completion summary) can reuse it.
_archive_artifacts() {
    local state_file="$1"

    if [[ ! -f "$state_file" ]]; then
        log "WARNING: state file not found: $state_file"
        return 0
    fi

    local input_type plan_input
    input_type=$(sed -n 's/^input_type:[[:space:]]*//p' "$state_file" | head -1 | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    plan_input=$(sed -n 's/^plan_path:[[:space:]]*//p' "$state_file" | head -1 | tr -d '"' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    if [[ -z "$plan_input" && "$input_type" == "plan" ]]; then
        plan_input=$(sed -n 's/^input:[[:space:]]*//p' "$state_file" | head -1 | tr -d '"' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    fi

    local plan_dir
    plan_dir=$(resolve_plan_dir "$plan_input")
    TARGET_PLAN_DIR="$plan_dir"

    if [[ -z "$plan_dir" || ! -d "$plan_dir" ]]; then
        return 0
    fi

    # Archive scratchpad to plan_dir (sidecar for file plans, folder itself for folder plans).
    local scratchpad_dir="${REPO_ROOT}/.fno/scratchpad"
    TARGET_SCRATCHPAD_ARCHIVED=false
    if [[ -d "$scratchpad_dir" ]]; then
        local session_start final_status iteration
        session_start=$(sed -n 's/^created_at:[[:space:]]*//p' "$state_file" | head -1 | tr -d '"')
        final_status=$(sed -n 's/^status:[[:space:]]*//p' "$state_file" | head -1 | tr -d '"')
        iteration=$(sed -n 's/^iteration:[[:space:]]*//p' "$state_file" | head -1)
        cat > "${scratchpad_dir}/session-metadata.yaml" << SMEOF
session_completed: $(date -u +%Y-%m-%dT%H:%M:%SZ)
session_started: ${session_start:-unknown}
final_status: ${final_status:-unknown}
total_iterations: ${iteration:-1}
SMEOF
        # Split rm and cp so a partial rm failure doesn't silently short-
        # circuit the cp (bash parses `rm -rf X && cp ... 2>>LOG` as only
        # binding the stderr redirect to cp, dropping rm's diagnostic).
        if ! rm -rf "${plan_dir}/scratchpad-archive" 2>>"${LOG_FILE:-/dev/null}"; then
            log "WARNING: failed to clear ${plan_dir}/scratchpad-archive before scratchpad archival; skipping"
        elif cp -r "$scratchpad_dir" "${plan_dir}/scratchpad-archive" 2>>"${LOG_FILE:-/dev/null}"; then
            log "Archived scratchpad to ${plan_dir}/scratchpad-archive"
            TARGET_SCRATCHPAD_ARCHIVED=true
        else
            log "WARNING: failed to copy scratchpad to ${plan_dir}/scratchpad-archive"
        fi
    fi

    # Session-aware archival of stale gate artifacts (Phase 2 task 2.3 of
    # loop-correctness-sweep, ab-83be25ea). Cross-session contamination:
    # if `.fno/artifacts/` carries gate artifacts whose session_id
    # frontmatter does NOT match the live state file's session_id, those
    # artifacts are stranded from a prior session. Move them to
    # `${plan_dir}/artifacts-archive/` so the directory only carries
    # current-session attestations - the gate verifier already filters by
    # session_id, but a janitor pass keeps the disk clean and preserves
    # the prior session's evidence near the plan.
    local artifacts_dir="${REPO_ROOT}/.fno/artifacts"
    local current_sid
    current_sid=$(sed -n 's/^session_id:[[:space:]]*//p' "$state_file" | head -1 | tr -d '"' \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    if [[ -n "$current_sid" && -d "$artifacts_dir" ]]; then
        local archive_dir="${plan_dir}/artifacts-archive"
        mkdir -p "$archive_dir" 2>>"${LOG_FILE:-/dev/null}" || true
        local archived=0 manifest="${archive_dir}/.manifest"
        # Iterate every gate artifact (filename matches phase-sid.md).
        # Skip files whose frontmatter session_id matches current_sid;
        # archive everything else with prior-session markers.
        local artifact basename_no_ext file_sid
        for artifact in "$artifacts_dir"/*-*.md; do
            [[ -f "$artifact" ]] || continue
            file_sid=$(grep -E '^session_id:[[:space:]]*' "$artifact" 2>/dev/null \
                | head -1 | sed -E 's/^session_id:[[:space:]]*//' | tr -d '"' \
                | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
            # Artifacts without session_id are pre-upgrade and ignored
            # (we cannot tell which session they belong to).
            [[ -z "$file_sid" ]] && continue
            # Same-session artifacts stay in place.
            [[ "$file_sid" == "$current_sid" ]] && continue
            basename_no_ext=$(basename "$artifact")
            if mv "$artifact" "${archive_dir}/${basename_no_ext}" 2>>"${LOG_FILE:-/dev/null}"; then
                printf '%s\tprior_session=%s\tarchived_at=%s\n' \
                    "$basename_no_ext" "$file_sid" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
                    >> "$manifest" 2>/dev/null || true
                archived=$((archived + 1))
            else
                log "WARNING: failed to archive stale artifact $artifact"
            fi
        done
        if (( archived > 0 )); then
            log "Archived $archived stale gate artifact(s) (prior-session) to $archive_dir"
        fi
    fi
}
