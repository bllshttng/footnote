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
# Where a coreutils timeout(1)/gtimeout(1) lives, if this host has one. Used to
# run the cap scenario a SECOND time with that dir visible to the hook. It never
# gates a scenario: the cap is dependency-free, so both paths always assert.
TIMEOUT_DIR=""
_timeout_bin="$(command -v timeout || command -v gtimeout || true)"
[[ -n "$_timeout_bin" ]] && TIMEOUT_DIR="$(cd "$(dirname "$_timeout_bin")" && pwd)"

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

# A directory to hang a PATH off that resolves NO timeout(1)/gtimeout(1), on any
# host. Stripping to /usr/bin:/bin is coreutils-free on macOS only - Linux ships
# /usr/bin/timeout, so CI would silently measure the coreutils path while the
# case claimed otherwise. Link in exactly what the hook and its stub need.
make_nocoreutils_dir() {
    local d b p
    d=$(mktemp -d -t fno-nocoreutils-XXXXXX)
    for b in bash sleep; do
        p="$(command -v "$b")" || fail "cannot build a coreutils-free PATH: $b not found"
        ln -s "$p" "$d/$b"
    done
    printf '%s' "$d"
}

[[ -f "$HOOK"       ]] || fail "hook not found at $HOOK"
[[ -x "$HOOK"       ]] || fail "hook not executable at $HOOK"
[[ -f "$HOOKS_JSON" ]] || fail "hooks.json not found at $HOOKS_JSON"

# ── Structural checks ────────────────────────────────────────────────
bash -n "$HOOK" || fail "bash -n rejected $HOOK"

grep -q 'command -v fno' "$HOOK" \
    || fail "hook does not graceful-skip when fno is missing"
grep -q '\[\[ -d ".fno" \]\]' "$HOOK" \
    || fail "hook does not graceful-skip when .fno/ is missing"
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
pass "structural: hook script, skip guards, hooks.json registration"

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
start_ms=$(now_ms)
set +e
OUT=$(cd "$TMP3" && PATH="$STUB_DIR:/usr/bin:/bin" bash "$HOOK" 2>&1)
rc=$?
set -e
fast_ms=$(( $(now_ms) - start_ms ))
[[ $rc -eq 0 ]] || fail "scenario 3: hook rc=$rc in stubbed project, expected 0"
echo "$OUT" | grep -q "## Agent operating stack" \
    || fail "scenario 3: missing 'Agent operating stack' header in output: $OUT"
echo "$OUT" | grep -q "project: /stub" \
    || fail "scenario 3: stub whoami output not surfaced in injection: $OUT"
echo "$OUT" | grep -q '```' \
    || fail "scenario 3: fenced block markers missing from output: $OUT"
# The cap must not tax the happy path. A watchdog that keeps the command
# substitution's pipe open would silently add the full 2s to every fast call.
[[ $fast_ms -lt 1000 ]] \
    || fail "scenario 3: fast path took ${fast_ms}ms, expected <1000ms (watchdog is delaying success)"
rm -rf "$TMP3"
pass "scenario 3: emits fenced block with whoami output when both conditions met (elapsed=${fast_ms}ms)"

# Scenario 4: a hung fno must not delay session start beyond ~2s, on ANY host.
# Runs twice against the same 10s stub, differing only in whether a coreutils
# timeout(1) is visible to the hook. The stripped-PATH run is the load-bearing
# one: stock macOS ships no timeout(1), so that is the path real users take, and
# it is the path that measured 10s while this scenario was gated behind a probe
# computed under the test's own (coreutils-bearing) PATH.
HANG_DIR=$(mktemp -d -t fno-agent-whoami-hang-XXXXXX)
cat >"$HANG_DIR/fno" <<'HANGEOF'
#!/usr/bin/env bash
sleep 10
HANGEOF
chmod +x "$HANG_DIR/fno"

hang_case() {
    local label="$1" hook_path="$2"
    local tmp start elapsed out rc

    tmp=$(mktemp -d -t fno-agent-whoami-hang-run-XXXXXX)
    mkdir -p "$tmp/.fno"

    start=$(now_ms)
    set +e
    out=$(cd "$tmp" && PATH="$hook_path" bash "$HOOK" 2>&1)
    rc=$?
    set -e
    elapsed=$(( $(now_ms) - start ))
    rm -rf "$tmp"

    [[ $rc -eq 0 ]] || fail "scenario 4 ($label): hook rc=$rc with hung fno, expected 0"
    [[ $elapsed -le 5000 ]] \
        || fail "scenario 4 ($label): hook took ${elapsed}ms with hung fno, expected <=5000ms (cap is 2s)"
    # Hung fno produces empty output; the hook short-circuits and emits nothing.
    [[ -z "$out" ]] || fail "scenario 4 ($label): hook emitted output with hung fno: $out"
    pass "scenario 4 ($label): 2s cap holds when fno hangs (elapsed=${elapsed}ms)"
}

NOCU_DIR=$(make_nocoreutils_dir)
# Positive control for the exclusion: an assertion about a binary being absent
# is only worth anything if something checked that it is actually absent.
if ( PATH="$HANG_DIR:$NOCU_DIR"; command -v timeout || command -v gtimeout ) >/dev/null 2>&1; then
    fail "scenario 4: the coreutils-free PATH still resolves a timeout binary; that case would assert nothing"
fi

hang_case "no coreutils on PATH" "$HANG_DIR:$NOCU_DIR"
if [[ -n "$TIMEOUT_DIR" ]]; then
    hang_case "coreutils at $TIMEOUT_DIR" "$HANG_DIR:$TIMEOUT_DIR:/usr/bin:/bin"
else
    # Not a skip: the cap needs no external binary, so the run above already
    # proved it. This host simply has no second PATH shape to prove it under.
    log "scenario 4: host has no timeout(1)/gtimeout(1); coreutils-present variant not applicable"
fi

rm -rf "$HANG_DIR" "$NOCU_DIR" "$STUB_DIR"

log "all scenarios passed"
exit 0
