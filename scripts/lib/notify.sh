#!/usr/bin/env bash
# OS notification helper for target
# Supports macOS (osascript) with Linux fallback (notify-send)
# Usage: source this file, then call notify

notify() {
    local title="${1:-target}"
    local message="${2:-Complete}"
    # Parity with fno/notify/_impl.py: a hermetic run never reaches the
    # operator's screen. fno.hermetic stamps this on every child it builds.
    if [[ "${FNO_TEST_HERMETIC:-}" == "1" ]]; then
        return 0
    fi
    if [[ "$(uname)" == "Darwin" ]]; then
        osascript -e "display notification \"$message\" with title \"$title\"" 2>/dev/null || true
    elif command -v notify-send &>/dev/null; then
        notify-send "$title" "$message" 2>/dev/null || true
    fi
}
