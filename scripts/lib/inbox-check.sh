#!/usr/bin/env bash
# inbox-check.sh - block (or warn on) session COMPLETE when there are
# unread inbox threads waiting.
#
# Lifted from hooks/target-stop-hook.sh (Phase 1 of stop-hook refactor).
# Behavior is identical to the inline definition.
#
# Structural detector: globs the recipient project's inbox/ for thread
# files lacking `read_at:` frontmatter. Default policy is notify-only;
# blocking is opt-in via config.inbox.block_complete_on_unread: true.
# The LLM never has to remember to drain - the hook surfaces it.
#
# Requires (set by caller):
#   CLAUDE_PLUGIN_ROOT (optional) - plugin install root
#   REPO_ROOT - project repo root
#   STATE_DIR - .fno/ directory for the project
#   log()       - logging function from the hook
#   emit_block() - from platform-io.sh

# check_unread_inbox_on_complete
#   Runs the inbox unread scanner. On unread > 0:
#     - always logs + writes hook-events.jsonl + fires notify
#     - if config.inbox.block_complete_on_unread is true, emits a block
#       JSON on stdout and exits 2 directly (caller's COMPLETE branch
#       must not continue past this point).
#   No-op on count=0. On scanner failure, fails closed when blocking is
#   requested, otherwise logs and proceeds.
check_unread_inbox_on_complete() {
    # Resolve cli_dir reliably across deployments. Try in order:
    #   1) CLAUDE_PLUGIN_ROOT/cli (when target runs from the abilities plugin)
    #   2) Project root cli/ relative to this lib (scripts/lib/, so ../../cli)
    #   3) REPO_ROOT/cli (legacy fallback for the abilities repo itself).
    local cli_dir=""
    if [[ -n "${CLAUDE_PLUGIN_ROOT:-}" && -d "${CLAUDE_PLUGIN_ROOT}/cli" ]]; then
        cli_dir="${CLAUDE_PLUGIN_ROOT}/cli"
    else
        local lib_dir
        # BASH_SOURCE[0] is this file (scripts/lib/inbox-check.sh) regardless
        # of who sourced it, so ../../cli is the project root cli/ directory.
        lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
        if [[ -d "$lib_dir/../../cli" ]]; then
            cli_dir="$(cd "$lib_dir/../../cli" && pwd)"
        elif [[ -d "${REPO_ROOT}/cli" ]]; then
            cli_dir="${REPO_ROOT}/cli"
        fi
    fi
    if [[ -z "$cli_dir" ]]; then
        log "check_unread_inbox: could not resolve cli/ directory; skipping"
        return 0
    fi

    # One subprocess call returns count + should_block + thread list as a
    # single JSON object. Avoids paying uv-run startup overhead twice
    # (gemini-code-assist MEDIUM finding on PR #225). Capture stderr
    # separately so a failed scan does NOT silently look like "0 unread".
    local scan_err scan_rc combined
    scan_err=$(mktemp)
    combined=$(uv run --project "$cli_dir" python3 -m fno.inbox.unread_scan combined 2>"$scan_err")
    scan_rc=$?

    local unread_count block_setting
    if [[ "$scan_rc" == "0" ]]; then
        unread_count=$(echo "$combined" | python3 -c "import json, sys; print(json.loads(sys.stdin.read())['count'])" 2>/dev/null)
        block_setting=$(echo "$combined" | python3 -c "import json, sys; print('true' if json.loads(sys.stdin.read())['should_block'] else 'false')" 2>/dev/null)
    fi

    if [[ "$scan_rc" != "0" || ! "$unread_count" =~ ^[0-9]+$ ]]; then
        log "check_unread_inbox: scanner failed (rc=$scan_rc): $(head -1 "$scan_err" 2>/dev/null)"
        rm -f "$scan_err"
        # Fail-closed when blocking is requested. Otherwise log and proceed.
        if [[ "$block_setting" == "true" ]]; then
            local reason="unread_inbox_scan_failed: cannot verify unread mail before COMPLETE"
            local system_msg="Inbox unread scanner failed; block_complete_on_unread is true so refusing to honor COMPLETE. Investigate $cli_dir then drain."
            echo "target: BLOCKING COMPLETE: $reason" >&2
            emit_block "$reason" "$system_msg"
            exit 2
        fi
        return 0
    fi
    rm -f "$scan_err"

    [[ "$unread_count" -gt 0 ]] || return 0

    mkdir -p "$STATE_DIR" 2>/dev/null
    printf '{"event":"unread_inbox_messages","count":%s,"ts":"%s"}\n' \
        "$unread_count" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        >> "$STATE_DIR/hook-events.jsonl" 2>/dev/null || true

    log "Unread inbox messages at COMPLETE: count=$unread_count"
    if command -v notify >/dev/null 2>&1; then
        notify "$unread_count unread inbox message(s); drain before exit" 2>/dev/null || true
    fi

    if [[ "$block_setting" == "true" ]]; then
        local reason="unread_inbox_messages: $unread_count thread(s) unread - drain before honoring COMPLETE"
        local system_msg="Run 'fno mail drain' to handle the $unread_count unread thread(s), then re-promise."
        echo "target: BLOCKING COMPLETE: $reason" >&2
        emit_block "$reason" "$system_msg"
        exit 2
    fi
}
