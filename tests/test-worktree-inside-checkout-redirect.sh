#!/usr/bin/env bash
# test-worktree-inside-checkout-redirect.sh
#
# worktrees_base migration (x-33e9): the CC WorktreeCreate hook
# (hooks/worktree-setup.sh) resolves the worktree location from
# config.paths.worktrees_base:
#   1. set                              -> relocate to <base>/<repo>/<name>
#   2. else use_conductor_canonical:true -> relocate to ~/conductor/workspaces/<repo>/<name>
#   3. else (unset)                      -> harness-native: LEAVE the worktree in
#                                           place (<repo>/.claude/worktrees/<name>),
#                                           no relocation.
# The old "inside-checkout is always forbidden -> redirect to conductor" rule is
# retired: harness-native .claude/worktrees/ (gitignored, search-clean) is now
# the OSS-neutral default.
#
# HOME is overridden to the sandbox so any conductor/relocation target
# materializes under the tempdir, never the developer's real ~.
#
# Scope: HOOK-ONLY. The /speculate copy keeps its .claude/worktrees/ placement
# (sanctioned exception), so this targets only hooks/worktree-setup.sh.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK="$REPO_ROOT/hooks/worktree-setup.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

if [[ ! -f "$HOOK" ]]; then
    echo "FAIL: hook not found at $HOOK" >&2
    exit 1
fi

# Build a canonical sandbox repo (named so basename is deterministic).
# $1 (optional): YAML body to write into <repo>/.fno/settings.yaml.
setup_canonical() {
    local base name
    base=$(mktemp -d -t wt-base-XXXXXX)
    name="myrepo"
    mkdir -p "$base/$name"
    (
        cd "$base/$name"
        git init -q
        git -c user.email=t@t -c user.name=t commit --allow-empty -m init -q
        git branch -m main 2>/dev/null || true
        mkdir -p .fno
    )
    if [[ -n "${1:-}" ]]; then
        printf '%s\n' "$1" > "$base/$name/.fno/settings.yaml"
    fi
    printf '%s\n%s\n' "$base" "$base/$name"
}

echo "=== test-worktree-base-resolution (x-33e9) ==="

# --- AC1: unset -> harness-native, inside-checkout LEFT in place -------------
echo ""
echo "--- AC1: worktrees_base unset -> inside-checkout left in place (no relocate) ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
INSIDE="$CANON_REPO/.claude/worktrees/feat-x"
git -C "$CANON_REPO" worktree add -q -b feature/feat-x "$INSIDE" 2>/dev/null \
    || fail "AC1" "could not pre-create inside-checkout worktree"
STDIN_JSON=$(printf '{"session_id":"s1","name":"feat-x","path":"%s","hook_event_name":"WorktreeCreate"}' "$INSIDE")
STDOUT=$( cd "$INSIDE" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
RC=$?
if [[ $RC -eq 0 ]]; then pass "AC1: hook exits 0"; else fail "AC1" "hook exit $RC"; fi
if [[ "$STDOUT" == "$INSIDE" ]]; then
    pass "AC1: stdout is the harness-native path (no relocation)"
else
    fail "AC1" "stdout '$STDOUT' != inside path '$INSIDE' (unexpected relocation)"
fi
if [[ ! -d "$SANDBOX/conductor" ]]; then
    pass "AC1: no conductor directory created"
else
    fail "AC1" "conductor dir was created at $SANDBOX/conductor"
fi
rm -rf "$SANDBOX"

# --- AC2: worktrees_base set -> relocate to <base>/<repo>/<name> -------------
echo ""
echo "--- AC2: config.paths.worktrees_base set -> relocate to <base>/<repo>/<name> ---"
OUT=$(setup_canonical $'config:\n  paths:\n    worktrees_base: WTBASE_PLACEHOLDER')
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
# Point the base at a sandbox dir (rewrite the placeholder now that we know SANDBOX).
WTBASE="$SANDBOX/wtroot"
sed -i.bak "s#WTBASE_PLACEHOLDER#$WTBASE#" "$CANON_REPO/.fno/settings.yaml" && rm -f "$CANON_REPO/.fno/settings.yaml.bak"
INSIDE="$CANON_REPO/.claude/worktrees/feat-y"
git -C "$CANON_REPO" worktree add -q -b feature/feat-y "$INSIDE" 2>/dev/null \
    || fail "AC2" "could not pre-create worktree"
STDIN_JSON=$(printf '{"session_id":"s1","name":"feat-y","path":"%s","hook_event_name":"WorktreeCreate"}' "$INSIDE")
STDOUT=$( cd "$INSIDE" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
EXPECTED="$WTBASE/myrepo/feat-y"
if [[ "$STDOUT" == "$EXPECTED" ]]; then
    pass "AC2: relocated to configured base ($EXPECTED)"
else
    fail "AC2" "stdout '$STDOUT' != expected '$EXPECTED'"
fi
if [[ -d "$EXPECTED" ]]; then pass "AC2: worktree dir materialized at the base"; else fail "AC2" "dir missing at $EXPECTED"; fi
rm -rf "$SANDBOX"

# --- AC3: use_conductor_canonical:true (no base) -> conductor back-compat ----
echo ""
echo "--- AC3: legacy use_conductor_canonical -> ~/conductor/workspaces/<repo>/<name> ---"
# Real settings store the worktree block top-level (read by the hook's wt_config).
OUT=$(setup_canonical $'worktree:\n  use_conductor_canonical: true')
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
INSIDE="$CANON_REPO/.claude/worktrees/feat-z"
git -C "$CANON_REPO" worktree add -q -b feature/feat-z "$INSIDE" 2>/dev/null \
    || fail "AC3" "could not pre-create worktree"
STDIN_JSON=$(printf '{"session_id":"s1","name":"feat-z","path":"%s","hook_event_name":"WorktreeCreate"}' "$INSIDE")
STDOUT=$( cd "$INSIDE" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
EXPECTED="$SANDBOX/conductor/workspaces/myrepo/feat-z"
if [[ "$STDOUT" == "$EXPECTED" ]]; then
    pass "AC3: legacy flag relocates to conductor ($EXPECTED)"
else
    fail "AC3" "stdout '$STDOUT' != expected '$EXPECTED'"
fi
rm -rf "$SANDBOX"

# --- AC4: outside path, unset -> left in place ------------------------------
echo ""
echo "--- AC4: outside-checkout path with no config left in place ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
OUTSIDE="$SANDBOX/sibling-wt"
git -C "$CANON_REPO" worktree add -q -b feature/sibling "$OUTSIDE" 2>/dev/null \
    || fail "AC4" "could not pre-create sibling worktree"
STDIN_JSON=$(printf '{"session_id":"s1","name":"sibling-wt","path":"%s","hook_event_name":"WorktreeCreate"}' "$OUTSIDE")
STDOUT=$( cd "$OUTSIDE" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
if [[ "$STDOUT" == "$OUTSIDE" ]]; then
    pass "AC4: outside path unchanged (no relocation when unset)"
else
    fail "AC4" "stdout '$STDOUT' != original '$OUTSIDE'"
fi
rm -rf "$SANDBOX"

# --- AC5: worktree.policy = "never" -> REFUSE creation, do not relocate ------
# A project whose working tree IS the product (an Obsidian vault) wants no
# worktree at all. The autonomous path (`fno worktree ensure`) already honored
# this; the hook did not, so `claude --worktree` created one anyway.
echo ""
echo "--- AC5: policy=never refuses creation (not relocation) ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
printf '[worktree]\npolicy = "never"\n' > "$CANON_REPO/.fno/config.toml"
TARGET="$CANON_REPO/.claude/worktrees/feat-never"
STDIN_JSON=$(printf '{"session_id":"s5","name":"feat-never","path":"%s","hook_event_name":"WorktreeCreate"}' "$TARGET")
STDERR=$( cd "$CANON_REPO" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>&1 >/dev/null )
RC=$?
if [[ $RC -ne 0 ]]; then
    pass "AC5: hook exits non-zero (aborts creation)"
else
    fail "AC5" "hook exited 0; creation would proceed despite policy=never"
fi
if [[ "$STDERR" == *"never"* ]]; then
    pass "AC5: refusal names the policy"
else
    fail "AC5" "refusal did not mention the policy: $STDERR"
fi
if [[ ! -d "$TARGET" ]]; then
    pass "AC5: no worktree materialized"
else
    fail "AC5" "worktree was created at $TARGET"
fi
rm -rf "$SANDBOX"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
