#!/usr/bin/env bash
# budget-display.sh - Budget cap check + COST_DISPLAY string assembly.
#
# Lifted from hooks/target-stop-hook.sh (Phase 12 of stop-hook refactor).
# Pure display logic - no gates, no kills. The actual budget enforcement
# (the trip that flips status to BLOCKED with stuck:budget_exceeded) lives
# in scripts/lib/thrash-detector.sh under check_budget_exceeded. This lib
# only computes the cosmetic suffix the resume system message includes
# (e.g. " | Cost: $1.42/$5.00 (approaching cap)").
#
# Reads total_cost_usd (or legacy total_cost) and budget_cap_usd from
# STATE_FILE, falls back to get_config("budget_cap", "") when the state
# file lacks an explicit cap, then formats COST_DISPLAY based on whether
# the cost is over budget, near (>80%) the cap, or under.
#
# May emit a `budget_exceeded` event (informational) when total > cap.
# This is hook-only telemetry; it does NOT block exit. The thrash
# detector emits its own typed `budget_exceeded` when the wall-clock or
# cost cap from settings.yaml is exceeded - that one DOES trip BLOCKED.
#
# Side effects:
#   - Sets COST_DISPLAY global (read by the resume system-message builder)
#   - May emit_event "budget_exceeded" with {total_cost, budget_cap}
#   - Sources cost-tracker.sh (for format_cost) at function-call time
#   - Defines fallback format_cost / get_config if source fails
#
# Requires (set by caller):
#   STATE_FILE   - path to target-state.md
#   SCRIPT_DIR   - hook's resolved repo root (for cost-tracker.sh source)
#   COST_DISPLAY - global the function writes into
#   log()        - from the hook
#   emit_event() - from events.sh (defaulted to no-op shim at hook source)

compute_budget_display() {
    # Source cost-tracker.sh at function-call time; it's only needed here.
    # config.sh is already sourced at the top of the hook, so we rely on
    # that import and the top-level fallback below covers source failure.
    # shellcheck source=../metrics/cost-tracker.sh
    source "${SCRIPT_DIR}/scripts/metrics/cost-tracker.sh" 2>/dev/null || log "WARNING: cost-tracker.sh failed to load"
    # Define no-op fallbacks if source failed (cost-tracker.sh) or if
    # config.sh failed to load at top-of-hook source time.
    if ! declare -F format_cost >/dev/null; then
        format_cost() { echo "\$${1:-0}"; }
    fi
    if ! declare -F get_config >/dev/null; then
        get_config() { echo "${2:-}"; }
    fi

    # Validate numeric values from state file to prevent injection
    local TOTAL_COST BUDGET_CAP OVER_BUDGET NEAR_BUDGET
    TOTAL_COST=$(grep -E '^total_cost_usd:' "$STATE_FILE" 2>/dev/null | sed 's/^total_cost_usd:[[:space:]]*//' | grep -E '^[0-9]+(\.[0-9]+)?$' || echo "0")
    # Fallback: check old field name (total_cost) for backward compatibility
    if [[ "$TOTAL_COST" == "0" ]]; then
        TOTAL_COST=$(grep -E '^total_cost:' "$STATE_FILE" 2>/dev/null | sed 's/^total_cost:[[:space:]]*//' | grep -E '^[0-9]+(\.[0-9]+)?$' || echo "0")
    fi
    BUDGET_CAP=$(grep -E '^budget_cap_usd:' "$STATE_FILE" 2>/dev/null | sed 's/^budget_cap_usd:[[:space:]]*//' | grep -E '^[0-9]+(\.[0-9]+)?$' || echo "")
    if [[ -z "$BUDGET_CAP" ]]; then
        BUDGET_CAP=$(get_config "budget_cap" "" 2>/dev/null | grep -E '^[0-9]+(\.[0-9]+)?$' || echo "")
    fi

    COST_DISPLAY=""
    if [[ -n "$TOTAL_COST" && "$TOTAL_COST" != "0" ]]; then
        COST_DISPLAY=" | Cost: $(format_cost "$TOTAL_COST" 2>/dev/null || echo "\$$TOTAL_COST")"
    fi

    if [[ -n "$BUDGET_CAP" && "$BUDGET_CAP" != "0" ]]; then
        if ! command -v bc &>/dev/null; then
            log "WARNING: bc not installed - budget enforcement disabled"
            COST_DISPLAY=" | Cost: \$$TOTAL_COST/\$$BUDGET_CAP (budget enforcement disabled - install bc)"
        else
            OVER_BUDGET=$(echo "$TOTAL_COST > $BUDGET_CAP" | bc 2>/dev/null || echo "0")
            if [[ "$OVER_BUDGET" == "1" ]]; then
                log "BUDGET EXCEEDED (informational): $TOTAL_COST > $BUDGET_CAP"
                emit_event "stop-hook" "budget_exceeded" \
                    "$(jq -nc --arg total "$TOTAL_COST" --arg cap "$BUDGET_CAP" \
                       '{"total_cost":$total,"budget_cap":$cap}')"
                COST_DISPLAY=" | Cost: $(format_cost "$TOTAL_COST" 2>/dev/null || echo "\$$TOTAL_COST")/$(format_cost "$BUDGET_CAP" 2>/dev/null || echo "\$$BUDGET_CAP") (over budget)"
            else
                NEAR_BUDGET=$(echo "$TOTAL_COST > ($BUDGET_CAP * 0.8)" | bc 2>/dev/null || echo "0")
                if [[ "$NEAR_BUDGET" == "1" ]]; then
                    COST_DISPLAY=" | Cost: $(format_cost "$TOTAL_COST" 2>/dev/null || echo "\$$TOTAL_COST")/$(format_cost "$BUDGET_CAP" 2>/dev/null || echo "\$$BUDGET_CAP") (approaching cap)"
                else
                    COST_DISPLAY=" | Cost: $(format_cost "$TOTAL_COST" 2>/dev/null || echo "\$$TOTAL_COST")/$(format_cost "$BUDGET_CAP" 2>/dev/null || echo "\$$BUDGET_CAP")"
                fi
            fi
        fi
    fi
}
