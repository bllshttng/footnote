#!/usr/bin/env bash
# Tests for hooks/worktree-harness-guard.sh (x-193d Wave 5).
#
# The hook shells `fno claim worktree-guard --json` and blocks ONLY on a parsed
# verdict=foreign. We stub `fno` on PATH to return controlled verdicts so the
# test needs no real claims state and no deployed fno.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GUARD="$REPO_ROOT/hooks/worktree-harness-guard.sh"

TMP_BASE="$(mktemp -d -t worktree-harness-guard-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

PASS=0
FAIL=0
pass() { printf '  PASS: %s\n' "$*"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL: %s\n' "$*" >&2; FAIL=$((FAIL + 1)); }

# Build a bindir with a stub `fno` that prints $STUB_JSON and exits $STUB_RC.
# Real tools (bash, cat, jq, printf) stay reachable via the fallback PATH.
make_fno_stub() {
    local bindir="$1" json="$2" rc="${3:-0}"
    mkdir -p "$bindir"
    cat >"$bindir/fno" <<EOF
#!/usr/bin/env bash
printf '%s\n' '$json'
exit $rc
EOF
    chmod +x "$bindir/fno"
}

run_guard() {  # run_guard <bindir-with-fno-or-empty> <payload>
    local bindir="$1" payload="$2"
    if [[ -n "$bindir" ]]; then
        printf '%s' "$payload" | PATH="$bindir:$PATH" bash "$GUARD"
    else
        printf '%s' "$payload" | PATH="$TMP_BASE/nofno:$PATH" bash "$GUARD"
    fi
}

assert_allow() {
    local name="$1" out="$2"
    if printf '%s' "$out" | jq -e 'type == "object" and length == 0' >/dev/null 2>&1; then
        pass "$name"
    else
        fail "$name (expected empty-object allow, got: $out)"
    fi
}

assert_block() {
    local name="$1" out="$2"
    if printf '%s' "$out" | jq -e '
        .decision == "block"
        and .hookSpecificOutput.permissionDecision == "deny"
        and (.reason | type == "string" and length > 0)
    ' >/dev/null 2>&1; then
        pass "$name"
    else
        fail "$name (expected deny, got: $out)"
    fi
}

echo "=== worktree harness guard ==="
[[ -f "$GUARD" ]] || { echo "FAIL: guard not found at $GUARD" >&2; exit 1; }

CWD="$TMP_BASE/wt"
mkdir -p "$CWD"
PAYLOAD="$(jq -nc --arg cwd "$CWD" '{cwd: $cwd, tool_name: "Edit", tool_input: {}}')"

# foreign -> block, and the owner harness name reaches the reason.
FOREIGN_BIN="$TMP_BASE/foreign"
make_fno_stub "$FOREIGN_BIN" '{"verdict":"foreign","worktree":"'"$CWD"'","my_harness":"codex","owner_harness":"claude","owner_holder":"claude-worktree:s1"}' 1
FOREIGN_OUT="$(run_guard "$FOREIGN_BIN" "$PAYLOAD")"
assert_block "foreign verdict blocks" "$FOREIGN_OUT"
if printf '%s' "$FOREIGN_OUT" | jq -r '.reason' | grep "claude" >/dev/null; then
    pass "block reason names the owning harness"
else
    fail "block reason omits the owner: $FOREIGN_OUT"
fi

# every non-foreign verdict approves.
for v in acquired ok override; do
    B="$TMP_BASE/$v"
    make_fno_stub "$B" '{"verdict":"'"$v"'","worktree":"'"$CWD"'","my_harness":"claude"}' 0
    assert_allow "$v verdict approves" "$(run_guard "$B" "$PAYLOAD")"
done

# no-worktree approves.
NOWT="$TMP_BASE/nowt"
make_fno_stub "$NOWT" '{"verdict":"no-worktree","my_harness":"claude"}' 0
assert_allow "no-worktree approves" "$(run_guard "$NOWT" "$PAYLOAD")"

# fail-open: an old fno without the subcommand (nonzero, no JSON) approves.
OLD="$TMP_BASE/old"
make_fno_stub "$OLD" '' 2
assert_allow "old fno without verb approves (fail-open)" "$(run_guard "$OLD" "$PAYLOAD")"

# fail-open: fno emits non-JSON garbage.
GARBAGE="$TMP_BASE/garbage"
make_fno_stub "$GARBAGE" 'No such command worktree-guard' 2
assert_allow "garbage output approves (fail-open)" "$(run_guard "$GARBAGE" "$PAYLOAD")"

# fail-open: no fno on PATH at all (real tools present, fno absent).
NOFNO_BIN="$TMP_BASE/nofno"
mkdir -p "$NOFNO_BIN"
for t in bash cat jq printf sed dirname; do
    p="$(command -v "$t" 2>/dev/null)" && ln -sf "$p" "$NOFNO_BIN/$t"
done
NOFNO_OUT="$(printf '%s' "$PAYLOAD" | PATH="$NOFNO_BIN" bash "$GUARD")"
assert_allow "missing fno on PATH approves" "$NOFNO_OUT"

# malformed payload does not crash: falls back to $PWD, consults fno. With a
# non-foreign owner that approves (a foreign stub would correctly still block).
assert_allow "malformed payload approves under non-foreign fno" "$(run_guard "$NOWT" 'not-json')"

# ── target gating (x-b18e) ───────────────────────────────────────────────────
# The verdict a real `fno claim worktree-guard` returns depends on WHERE it is
# run, so a stub that ignores its cwd cannot exercise target gating at all. This
# one answers foreign only inside the owned tree, which is the property under
# test: the hook must ask about the target, not the session cwd.
make_fno_path_stub() {  # <bindir> <owned-worktree>
    local bindir="$1" owned="$2"
    mkdir -p "$bindir"
    cat >"$bindir/fno" <<EOF
#!/usr/bin/env bash
OWNED='$owned'
EOF
    cat >>"$bindir/fno" <<'EOF'
here="$(pwd -P)"
owned="$(cd "$OWNED" 2>/dev/null && pwd -P)"
if [[ "$here" == "$owned" || "$here" == "$owned"/* ]]; then
    printf '{"verdict":"foreign","worktree":"%s","my_harness":"codex","owner_harness":"claude","owner_holder":"claude-worktree:s1"}\n' "$owned"
    exit 1
fi
printf '%s\n' '{"verdict":"no-worktree","my_harness":"claude"}'
exit 0
EOF
    chmod +x "$bindir/fno"
}

payload() {  # payload <cwd> <tool> <file_path> <command>
    jq -nc --arg cwd "$1" --arg tool "$2" --arg fp "$3" --arg cmd "$4" \
        '{cwd: $cwd, tool_name: $tool, tool_input: {file_path: $fp, command: $cmd}}'
}

OWNED="$TMP_BASE/owned"
OUTSIDE="$TMP_BASE/outside"
mkdir -p "$OWNED/sub" "$OUTSIDE"
OWNED_P="$(cd "$OWNED" && pwd -P)"
PATHSTUB="$TMP_BASE/pathstub"
make_fno_path_stub "$PATHSTUB" "$OWNED"

# The defect: a write to an unrelated tree, refused only because cwd drifted.
assert_allow "Edit outside the owned worktree approves (cwd inside it)" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Edit "$OUTSIDE/notes.md" "")")"
assert_allow "Write outside the owned worktree approves (cwd inside it)" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Write "$OUTSIDE/notes.md" "")")"

# A target whose parent does not exist yet anchors to the nearest existing
# ancestor rather than failing the cd and falling through to cwd.
assert_allow "Write to a not-yet-created directory outside approves" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Write "$OUTSIDE/new/deep/plan.md" "")")"

# Stricter in the other direction: cwd-gating missed this one entirely.
assert_block "Write INTO the owned worktree blocks from an outside cwd" \
    "$(run_guard "$PATHSTUB" "$(payload "$OUTSIDE" Write "$OWNED/sub/f.txt" "")")"
assert_block "Edit INTO the owned worktree still blocks" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Edit "$OWNED/sub/f.txt" "")")"

# ── the escape (x-b18e) ──────────────────────────────────────────────────────
# A refused session must have at least one in-session command that succeeds and
# moves it out. Without this the guard is a trap, not a guard.
assert_allow "bare cd out approves from a foreign cwd" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "cd $OUTSIDE")")"
assert_allow "bare cd with surrounding whitespace approves" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "   cd $OUTSIDE  ")")"
assert_block "cd chained to another command does not ride the exemption" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "cd $OUTSIDE && rm -rf .")")"
assert_block "an ordinary command in a foreign worktree still blocks" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "git commit -m x")")"

# The inline grant is honoured HERE because the hook is the only layer that sees
# the command text; by the time `fno claim` runs it is a subprocess whose
# environment never saw the assignment.
assert_allow "inline FNO_WORKTREE_GRANT naming the worktree approves" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "FNO_WORKTREE_GRANT=$OWNED_P git commit -m x")")"
assert_allow "inline quoted FNO_WORKTREE_GRANT approves" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "FNO_WORKTREE_GRANT=\"$OWNED_P\" git commit -m x")")"
assert_allow "inline FNO_WORKTREE_OK=1 approves" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "FNO_WORKTREE_OK=1 git commit -m x")")"
assert_block "a grant naming a DIFFERENT worktree does not approve" \
    "$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "FNO_WORKTREE_GRANT=/some/other/tree git commit -m x")")"

# The refusal must advertise only escapes that work. Naming an unreachable one
# is what cost this guard three days of trapped sessions.
ESCAPE_OUT="$(run_guard "$PATHSTUB" "$(payload "$OWNED" Bash "" "git commit -m x")")"
ESCAPE_REASON="$(printf '%s' "$ESCAPE_OUT" | jq -r '.reason')"
if printf '%s' "$ESCAPE_REASON" | grep -q 'bare cd' \
   && printf '%s' "$ESCAPE_REASON" | grep -q "FNO_WORKTREE_GRANT=$OWNED_P"; then
    pass "refusal names the cd escape and a grant with the real worktree path"
else
    fail "refusal omits a working escape: $ESCAPE_REASON"
fi

printf '\n=== Results: %d passed, %d failed ===\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
