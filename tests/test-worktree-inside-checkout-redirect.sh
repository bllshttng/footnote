#!/usr/bin/env bash
# test-worktree-inside-checkout-redirect.sh
#
# Worktree Scope Hygiene, US3 / AC3-HP, AC3-EDGE:
# The CC WorktreeCreate hook (hooks/worktree-setup.sh) must redirect a
# worktree path that resolves INSIDE the canonical checkout
# (<repo>/.claude/worktrees/<name>) to ~/conductor/workspaces/<repo>/<name>
# EVEN WHEN worktree.use_conductor_canonical is unset. A path outside the
# checkout with the flag unset is left in place (no redirect).
#
# HOME is overridden to the sandbox so the conductor worktree materializes
# under the tempdir, never the developer's real ~/conductor.
#
# Scope: this guard is HOOK-ONLY. The /speculate copy intentionally keeps
# its .claude/worktrees/ placement (sanctioned exception), so this test
# targets only hooks/worktree-setup.sh, not the duplicate.

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
setup_canonical() {
    local base name
    base=$(mktemp -d -t wt-inside-XXXXXX)
    name="myrepo"
    mkdir -p "$base/$name"
    (
        cd "$base/$name"
        git init -q
        git -c user.email=t@t -c user.name=t commit --allow-empty -m init -q
        git branch -m main 2>/dev/null || true
        mkdir -p .fno
    )
    # Echo the sandbox base and the repo dir.
    printf '%s\n%s\n' "$base" "$base/$name"
}

echo "=== test-worktree-inside-checkout-redirect (US3 / AC3-HP) ==="

# --- AC3-HP: inside-checkout path redirected to conductor, flag UNSET -------
echo ""
echo "--- AC3-HP: inside-checkout redirected even with use_conductor_canonical unset ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
INSIDE="$CANON_REPO/.claude/worktrees/feat-x"
# Pre-create the inside-checkout worktree (simulates CC's default placement).
git -C "$CANON_REPO" worktree add -q -b feature/feat-x "$INSIDE" 2>/dev/null \
    || fail "AC3-HP" "could not pre-create inside-checkout worktree"
STDIN_JSON=$(printf '{"session_id":"s1","name":"feat-x","path":"%s","hook_event_name":"WorktreeCreate"}' "$INSIDE")
# Invoke from the sandbox root, HOME=sandbox so conductor lands under tempdir.
STDOUT=$( cd "$SANDBOX" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
RC=$?
EXPECTED="$SANDBOX/conductor/workspaces/myrepo/feat-x"
if [[ $RC -eq 0 ]]; then
    pass "AC3-HP: hook exits 0"
else
    fail "AC3-HP" "hook exit $RC (expected 0)"
fi
if [[ "$STDOUT" == "$EXPECTED" ]]; then
    pass "AC3-HP: stdout is the conductor path ($EXPECTED)"
else
    fail "AC3-HP" "stdout '$STDOUT' != expected conductor path '$EXPECTED'"
fi
if [[ -d "$EXPECTED" ]]; then
    pass "AC3-HP: conductor worktree directory materialized"
else
    fail "AC3-HP" "conductor worktree dir missing at $EXPECTED"
fi
# The inside-checkout worktree must no longer be registered.
if ! git -C "$CANON_REPO" worktree list --porcelain 2>/dev/null | grep -qF "worktree $INSIDE"; then
    pass "AC3-HP: inside-checkout worktree removed from registry"
else
    fail "AC3-HP" "inside-checkout worktree still registered at $INSIDE"
fi
rm -rf "$SANDBOX"

# --- AC3-HP-b: inside-checkout with NO name in stdin -> basename derivation -
echo ""
echo "--- AC3-HP-b: name derived from basename when stdin omits it ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
INSIDE="$CANON_REPO/.claude/worktrees/derived-name"
git -C "$CANON_REPO" worktree add -q -b feature/derived-name "$INSIDE" 2>/dev/null \
    || fail "AC3-HP-b" "could not pre-create inside-checkout worktree"
# stdin payload carries .path but NO .name -> hook derives from basename.
STDIN_JSON=$(printf '{"session_id":"s1","path":"%s","hook_event_name":"WorktreeCreate"}' "$INSIDE")
STDOUT=$( cd "$SANDBOX" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
EXPECTED="$SANDBOX/conductor/workspaces/myrepo/derived-name"
if [[ "$STDOUT" == "$EXPECTED" ]]; then
    pass "AC3-HP-b: basename-derived redirect target ($EXPECTED)"
else
    fail "AC3-HP-b" "stdout '$STDOUT' != expected '$EXPECTED'"
fi
rm -rf "$SANDBOX"

# --- Branch-reuse collision: nested worktree already on `worktree-<name>` ---
# Regression for the redirect silently failing (and leaving the forbidden
# nested worktree) when the conductor branch `worktree-<name>` is already
# checked out by the worktree being redirected away.
echo ""
echo "--- branch-reuse: nested already on worktree-<name> still redirects ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
INSIDE="$CANON_REPO/.claude/worktrees/collide"
# Pre-create the nested worktree ON the exact branch the redirect will target.
git -C "$CANON_REPO" worktree add -q -b worktree-collide "$INSIDE" 2>/dev/null \
    || fail "branch-reuse" "could not pre-create nested worktree on worktree-collide"
STDIN_JSON=$(printf '{"session_id":"s1","name":"collide","path":"%s","hook_event_name":"WorktreeCreate"}' "$INSIDE")
STDOUT=$( cd "$SANDBOX" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
EXPECTED="$SANDBOX/conductor/workspaces/myrepo/collide"
if [[ "$STDOUT" == "$EXPECTED" ]]; then
    pass "branch-reuse: redirect succeeded despite branch already checked out by the nested worktree"
else
    fail "branch-reuse" "stdout '$STDOUT' != expected '$EXPECTED' (redirect left the nested worktree)"
fi
if [[ -d "$EXPECTED" ]]; then
    pass "branch-reuse: conductor worktree materialized on the freed branch"
else
    fail "branch-reuse" "conductor worktree dir missing at $EXPECTED"
fi
rm -rf "$SANDBOX"

# --- AC3-EDGE: a NON-inside-checkout path with the flag unset is NOT moved --
echo ""
echo "--- AC3-EDGE: outside-checkout path left in place (no redirect) ---"
OUT=$(setup_canonical)
SANDBOX=$(echo "$OUT" | sed -n '1p')
CANON_REPO=$(echo "$OUT" | sed -n '2p')
OUTSIDE="$SANDBOX/sibling-wt"
git -C "$CANON_REPO" worktree add -q -b feature/sibling "$OUTSIDE" 2>/dev/null \
    || fail "AC3-EDGE" "could not pre-create sibling worktree"
STDIN_JSON=$(printf '{"session_id":"s1","name":"sibling-wt","path":"%s","hook_event_name":"WorktreeCreate"}' "$OUTSIDE")
STDOUT=$( cd "$OUTSIDE" && HOME="$SANDBOX" bash "$HOOK" <<<"$STDIN_JSON" 2>/dev/null )
if [[ "$STDOUT" == "$OUTSIDE" ]]; then
    pass "AC3-EDGE: outside path unchanged (no redirect when flag unset)"
else
    fail "AC3-EDGE" "stdout '$STDOUT' != original '$OUTSIDE' (unexpected redirect)"
fi
rm -rf "$SANDBOX"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
