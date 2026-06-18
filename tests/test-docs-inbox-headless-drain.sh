#!/usr/bin/env bash
# Test: cross-project-inbox guide has headless drain section (Task 6.3)
set -euo pipefail

GUIDE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/docs/guides/cross-project-inbox.md"
PASS=0
FAIL=0

assert_contains() {
    local label="$1"
    local pattern="$2"
    if grep -q "$pattern" "$GUIDE"; then
        echo "PASS: $label"
        ((PASS++)) || true
    else
        echo "FAIL: $label -- pattern not found: $pattern"
        ((FAIL++)) || true
    fi
}

# AC1-HP: Enable headless drain section with four steps
assert_contains "AC1-HP: Enable headless drain heading" "Enable headless drain"
assert_contains "AC1-HP: Step 1 - config flag" "config.inbox.watch.enabled"
assert_contains "AC1-HP: Step 2 - fno watch install" "fno watch install"
assert_contains "AC1-HP: Step 3 - fno watch status" "fno watch status"
assert_contains "AC1-HP: Step 4 - fno watch uninstall" "fno watch uninstall"
assert_contains "AC1-HP: install-drain-prompt.sh step" "install-drain-prompt.sh"

# AC2-ERR: Prerequisites section
assert_contains "AC2-ERR: fswatch prereq" "fswatch"
assert_contains "AC2-ERR: brew install fswatch" "brew install fswatch"
assert_contains "AC2-ERR: macOS-only note" "macOS"
assert_contains "AC2-ERR: launchd mentioned" "launchd"

# Notification policy
assert_contains "Notification policy: notify_on_send config" "notify_on_send"
assert_contains "Notification policy: question_only default" "question_only"
assert_contains "Notification policy: off value" '"off"'

# Active-session bypass
assert_contains "Active-session bypass: documented" "active.*session\|active session\|active target\|Active.*session"
assert_contains "Active-session bypass: IN_PROGRESS" "IN_PROGRESS"

# Disabling for a session: launchctl path
assert_contains "Disable: launchctl unload path" "launchctl unload"

echo ""
echo "Results: $PASS passed, $FAIL failed"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
