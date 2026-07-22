#!/usr/bin/env bash
# Smoke test for the SessionStart hook that injects `fno whoami` output.
#
# Verifies:
#   - Silent skip when fno is not on PATH (no output, rc=0)
#   - Silent skip when no .fno/ dir exists (no output, rc=0)
#   - Emits a fenced block with the orientation header in a real project
#   - Never exceeds the 2s timeout cap even when fno is stubbed to hang
#   - hooks.json registers the new hook in SessionStart and the JSON is valid
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/inject-fno-agent-whoami.sh"
HOOKS_JSON="${REPO_ROOT}/hooks/hooks.json"

log()  { printf '[fno-agent-whoami-hook] %s\n' "$*"; }
fail() { printf '[fno-agent-whoami-hook] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[fno-agent-whoami-hook] PASS: %s\n' "$*"; }
skip() { printf '[fno-agent-whoami-hook] SKIP: %s\n' "$*" >&2; exit 77; }

command -v python3 &>/dev/null || skip "python3 not on PATH"
# Scenario 4 (timeout-cap behavior) requires timeout(1) or gtimeout(1).
# The hook degrades gracefully when neither is available, but then the cap
# itself can't be observed. Detect once and skip scenario 4 specifically.
TIMEOUT_AVAILABLE=0
if command -v timeout &>/dev/null || command -v gtimeout &>/dev/null; then
    TIMEOUT_AVAILABLE=1
fi
[[ -f "$HOOK"       ]] || fail "hook not found at $HOOK"
[[ -x "$HOOK"       ]] || fail "hook not executable at $HOOK"
[[ -f "$HOOKS_JSON" ]] || fail "hooks.json not found at $HOOKS_JSON"

# ── Structural checks ────────────────────────────────────────────────
bash -n "$HOOK" || fail "bash -n rejected $HOOK"

grep -q 'command -v fno' "$HOOK" \
    || fail "hook does not graceful-skip when fno is missing"
grep -q '\[\[ -d ".fno" \]\]' "$HOOK" \
    || fail "hook does not graceful-skip when .fno/ is missing"
grep -q '_with_timeout 2 fno whoami' "$HOOK" \
    || fail "hook does not cap fno whoami at 2s"
grep -q '_with_timeout()' "$HOOK" \
    || fail "hook does not define portable _with_timeout wrapper"

# hooks.json validates as JSON and contains the new entry under SessionStart.
python3 -c "import json,sys; json.load(open('$HOOKS_JSON'))" \
    || fail "hooks.json failed JSON parse"
python3 - "$HOOKS_JSON" <<'PYEOF' || fail "hooks.json does not register inject-fno-agent-whoami.sh under SessionStart"
import json, sys
data = json.load(open(sys.argv[1]))
ss = data.get("hooks", {}).get("SessionStart", [])
for group in ss:
    for h in group.get("hooks", []):
        if "inject-fno-agent-whoami.sh" in h.get("command", ""):
            sys.exit(0)
sys.exit(1)
PYEOF
pass "structural: hook script, skip guards, timeout cap, hooks.json registration"

# ── Behavior checks ──────────────────────────────────────────────────

# Scenario 1: skip when fno is not on PATH. Run in a tempdir with .fno/
# present so the second guard does NOT bail first; PATH stripped of fno.
TMP=$(mktemp -d -t fno-agent-whoami-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/.fno"
set +e
OUT=$(cd "$TMP" && PATH=/usr/bin:/bin bash "$HOOK" 2>&1)
rc=$?
set -e
[[ $rc -eq 0 ]] || fail "scenario 1: hook rc=$rc with no fno on PATH, expected 0"
[[ -z "$OUT" ]] || fail "scenario 1: hook emitted output with no fno: $OUT"
pass "scenario 1: silent skip when fno missing from PATH"

# Scenario 2: skip when .fno/ dir is absent. Use a stub fno shim so the
# first guard passes; the second must bail.
STUB_DIR=$(mktemp -d -t fno-agent-whoami-stub-XXXXXX)
cat >"$STUB_DIR/fno" <<'STUBEOF'
#!/usr/bin/env bash
echo "project: /stub"
STUBEOF
chmod +x "$STUB_DIR/fno"

TMP2=$(mktemp -d -t fno-agent-whoami-no-fno-XXXXXX)
set +e
OUT=$(cd "$TMP2" && PATH="$STUB_DIR:/usr/bin:/bin" bash "$HOOK" 2>&1)
rc=$?
set -e
[[ $rc -eq 0 ]] || fail "scenario 2: hook rc=$rc with no .fno/, expected 0"
[[ -z "$OUT" ]] || fail "scenario 2: hook emitted output with no .fno/: $OUT"
rm -rf "$TMP2"
pass "scenario 2: silent skip when .fno/ dir absent"

# Scenario 3: with both fno (stub) and .fno/ present, the hook emits a
# fenced block carrying the stub's output. We assert on the header text and
# on the presence of the stub's whoami line so we know wiring is end-to-end.
TMP3=$(mktemp -d -t fno-agent-whoami-real-XXXXXX)
mkdir -p "$TMP3/.fno"
set +e
OUT=$(cd "$TMP3" && PATH="$STUB_DIR:/usr/bin:/bin" bash "$HOOK" 2>&1)
rc=$?
set -e
[[ $rc -eq 0 ]] || fail "scenario 3: hook rc=$rc in stubbed project, expected 0"
echo "$OUT" | grep -q "## Agent operating stack" \
    || fail "scenario 3: missing 'Agent operating stack' header in output: $OUT"
echo "$OUT" | grep -q "project: /stub" \
    || fail "scenario 3: stub whoami output not surfaced in injection: $OUT"
echo "$OUT" | grep -q '```' \
    || fail "scenario 3: fenced block markers missing from output: $OUT"
rm -rf "$TMP3"
pass "scenario 3: emits fenced block with whoami output when both conditions met"

# Scenario 4: hung fno must not delay session start beyond ~2s. Stub fno to
# sleep 10s; assert wall-clock <= 5s (generous to account for shell startup).
# Requires timeout(1) or gtimeout(1); the hook degrades gracefully on hosts
# without either, but the cap itself only fires when one is available.
if [[ $TIMEOUT_AVAILABLE -eq 1 ]]; then
    HANG_DIR=$(mktemp -d -t fno-agent-whoami-hang-XXXXXX)
    cat >"$HANG_DIR/fno" <<'HANGEOF'
#!/usr/bin/env bash
sleep 10
HANGEOF
    chmod +x "$HANG_DIR/fno"

    TMP4=$(mktemp -d -t fno-agent-whoami-hang-run-XXXXXX)
    mkdir -p "$TMP4/.fno"

    start_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
    set +e
    OUT=$(cd "$TMP4" && PATH="$HANG_DIR:/usr/bin:/bin" bash "$HOOK" 2>&1)
    rc=$?
    set -e
    end_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
    elapsed=$((end_ms - start_ms))

    [[ $rc -eq 0 ]] || fail "scenario 4: hook rc=$rc with hung fno, expected 0"
    [[ $elapsed -le 5000 ]] \
        || fail "scenario 4: hook took ${elapsed}ms with hung fno, expected <=5000ms (timeout cap is 2s)"
    # Hung fno produces empty output; the hook short-circuits and emits nothing.
    [[ -z "$OUT" ]] || fail "scenario 4: hook emitted output with hung fno: $OUT"

    rm -rf "$TMP4" "$HANG_DIR"
    pass "scenario 4: 2s timeout cap holds when fno hangs (elapsed=${elapsed}ms)"
else
    log "scenario 4: skipped (no timeout/gtimeout on PATH; cap not observable)"
fi

rm -rf "$STUB_DIR"

log "all scenarios passed"
exit 0
