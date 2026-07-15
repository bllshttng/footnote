#!/usr/bin/env bash
# Cost estimation for target metrics
#
# Pricing lives in ONE place: the in-package fno.cost.cost_tracker module
# (cli/src/fno/cost/cost_tracker.py). estimate_cost delegates to its
# `estimate` CLI via `python3 -m` so shell and Python can never disagree (the
# previous inline rate table here drifted from the Python source of truth and
# overstated opus ~3x).
#
# Usage:
#   source cost-tracker.sh
#   estimate_cost opus 50000 10000    # → dollar amount
#   format_cost 2.3500                # → $2.35

# Only set strict mode when executed directly, not when sourced
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    set -euo pipefail
fi

# Resolve the package source once at source time (bash 3.2 safe). In a checkout
# point PYTHONPATH at cli/src so `python3 -m fno.cost.cost_tracker` runs
# pre-install; otherwise rely on the installed `fno`.
_COST_PKG_SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." 2>/dev/null && pwd)/cli/src"
if [[ -f "${_COST_PKG_SRC}/fno/cost/cost_tracker.py" ]]; then
    export PYTHONPATH="${_COST_PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
fi

# Echo the command PREFIX that runs the `estimate` CLI under an interpreter that
# satisfies fno (>=3.11 AND its full dependency set). Both live in the project
# venv, which `uv run` guarantees; bare `python3` may be an old system build
# (e.g. macOS Xcode 3.9) that can't even import fno (fno.events uses 3.10+
# typing.TypeAlias) and would silently zero every estimate. --no-sync uses the
# existing venv without a network round-trip. Echoes nothing (rc 1) when uv is
# unavailable, so estimate_cost degrades cleanly - and an empty PATH (no uv)
# degrades exactly as a python3-less host used to.
_cost_runner() {
    command -v uv &>/dev/null || return 1
    echo "uv run --no-sync --project ${_COST_PKG_SRC%/src} python"
}

# Estimate cost in USD from model name and token counts
# Args: model input_tokens output_tokens [cache_read_tokens] [cache_create_tokens]
# Returns: decimal USD amount (4 decimal places)
estimate_cost() {
    local model="${1:?model required}"  # full ID or bare family (opus, sonnet, haiku)
    local input_tokens="${2:-0}"
    local output_tokens="${3:-0}"
    local cache_read_tokens="${4:-0}"
    local cache_create_tokens="${5:-0}"

    # Validate token counts are numeric
    if ! [[ "$input_tokens" =~ ^[0-9]+$ ]]; then input_tokens=0; fi
    if ! [[ "$output_tokens" =~ ^[0-9]+$ ]]; then output_tokens=0; fi
    if ! [[ "$cache_read_tokens" =~ ^[0-9]+$ ]]; then cache_read_tokens=0; fi
    if ! [[ "$cache_create_tokens" =~ ^[0-9]+$ ]]; then cache_create_tokens=0; fi

    local runner
    if ! runner=$(_cost_runner); then
        echo "WARNING: uv not available - cost estimation disabled" >&2
        echo "0"
        return 1
    fi
    # stderr deliberately passes through: the one-time unknown-model
    # fallback warning and any Python traceback are the diagnostics an
    # operator needs; swallowing them here would make a fallback-priced
    # number indistinguishable from a table-priced one.
    # shellcheck disable=SC2086  # $runner is a deliberate multi-word prefix
    local result
    if ! result=$($runner -m fno.cost.cost_tracker estimate "$model" \
        "$input_tokens" "$output_tokens" \
        "$cache_read_tokens" "$cache_create_tokens"); then
        echo "WARNING: cost estimation failed (see fno.cost.cost_tracker)" >&2
        echo "0"
        return 1
    fi
    echo "$result"
}

# Format cost as $X.XX for display
format_cost() {
    local cost="${1:-0}"
    printf '$%.2f' "$cost"
}

# Add cost to running total in state file
# Args: state_file cost_usd
update_total_cost() {
    local state_file="${1:?state_file required}"
    local new_cost="${2:?cost required}"

    if [[ ! -f "$state_file" ]]; then
        return 1
    fi

    local current_total
    current_total=$(grep -E '^total_cost_usd:' "$state_file" 2>/dev/null | sed 's/^total_cost_usd:[[:space:]]*//' || echo "0")
    if [[ -z "$current_total" || "$current_total" == "null" ]]; then
        current_total="0"
    fi
    # Validate both values are numeric before passing to bc
    if ! [[ "$current_total" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then current_total="0"; fi
    if ! [[ "$new_cost" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
        echo "WARNING: invalid cost value '$new_cost'" >&2
        return 1
    fi

    local new_total
    if ! command -v bc &>/dev/null; then
        echo "WARNING: bc not installed - cost tracking disabled" >&2
        return 1
    fi
    new_total=$(echo "scale=4; $current_total + $new_cost" | bc 2>/dev/null || echo "$current_total")
    # Ensure leading zero for values like .0750 (bc omits it)
    [[ "$new_total" == .* ]] && new_total="0${new_total}"

    local temp_file
    temp_file=$(mktemp "${state_file}.tmp.XXXXXX")
    if grep -q '^total_cost_usd:' "$state_file" 2>/dev/null; then
        sed "s/^total_cost_usd:.*/total_cost_usd: $new_total/" "$state_file" > "$temp_file"
    else
        # Add after iteration line
        awk -v cost="$new_total" '/^iteration:/{print; print "total_cost_usd: " cost; next}1' "$state_file" > "$temp_file"
    fi

    if [[ -s "$temp_file" ]]; then
        if ! mv "$temp_file" "$state_file"; then
            echo "WARNING: failed to update cost in state file" >&2
            rm -f "$temp_file"
            return 1
        fi
    else
        echo "WARNING: cost update produced empty temp file — state unchanged" >&2
        rm -f "$temp_file"
        return 1
    fi
}
