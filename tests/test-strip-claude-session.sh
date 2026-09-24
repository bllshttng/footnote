#!/usr/bin/env bash
# Session-URL strip commit-msg hook.
#
# The harness tells every session to end commits with a `Claude-Session: <url>`
# line; scripts/ci/check-no-session-urls.sh refuses such commits only AFTER
# they are pushed, where a force-push does not retract them. The commit-msg
# hook installed by scripts/setup/setup-worktree.sh is the commit-time strip.
#
# This drives a real scratch repo + linked worktree with the ACTUAL install
# snippet extracted from setup-worktree.sh (same extraction contract as
# tests/test-worktree-salvage-ref.sh) and a real `git commit` carrying the
# trailer - no mocked git output. The scratch repo pins core.hooksPath to its
# own hooks dir so a machine-level global hooksPath dispatcher cannot mask a
# broken shim with its own strip policy.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STRIP_HOOK="$REPO_ROOT/hooks/strip-claude-session.sh"
SETUP_WORKTREE_SH="$REPO_ROOT/scripts/setup/setup-worktree.sh"
SCRATCH="$(mktemp -d)"
SCRATCH="$(cd "$SCRATCH" && pwd -P)"
trap 'rm -rf "$SCRATCH"' EXIT

fail() { echo "FAIL: $1" >&2; exit 1; }

extract_install_snippet() {
  # Extracts the REAL session-strip install block, bounded by two markers
  # that each occur exactly once; the end marker line is excluded.
  sed -n '/^_strip_marker="strip-claude-session\.sh"$/,/^# Salvage remote mirror/p' \
    "$SETUP_WORKTREE_SH" | sed '$d'
}

run_install_snippet() {
  local wt="$1"
  local snippet
  snippet="$(extract_install_snippet)"
  [[ -n "$snippet" ]] || fail "could not extract the session-strip install snippet from setup-worktree.sh - markers moved?"
  WORKTREE="$wt" bash -c "$snippet"
}

assert_no_session_url() {
  local msg="$1" label="$2"
  if printf '%s\n' "$msg" | grep -qE 'claude\.ai/code/[A-Za-z0-9]'; then
    fail "$label: session URL survived in the committed message"
  fi
  if printf '%s\n' "$msg" | grep -qiE '^[[:space:]]*claude-session:'; then
    fail "$label: Claude-Session trailer survived in the committed message"
  fi
}

canonical="$SCRATCH/canonical"
git init -q -b trunk "$canonical"
git -C "$canonical" config user.email t@t.co
git -C "$canonical" config user.name t
echo x > "$canonical/f.txt"
git -C "$canonical" add f.txt
git -C "$canonical" commit -q -m init

# A linked worktree, mirroring the real topology: the committing worktree's
# hooks live in the SHARED common dir, and the shim must resolve the
# committing worktree's own checked-out copy at runtime.
wt="$SCRATCH/wt"
git -C "$canonical" worktree add -q -b wtb "$wt"

# Hermeticity: route git straight at the shared hooks dir, bypassing any
# global hooksPath dispatcher this machine may have (which would strip the
# trailer itself and could green-light a broken shim).
common_hooks="$canonical/.git/hooks"

# --- AC1: a commit carrying the trailer lands without it, rest unchanged ---

run_install_snippet "$wt"
[[ -x "$common_hooks/commit-msg" ]] || fail "install snippet did not create the shared commit-msg hook"

mkdir -p "$wt/hooks"
cp "$STRIP_HOOK" "$wt/hooks/strip-claude-session.sh"
chmod +x "$wt/hooks/strip-claude-session.sh"

msg_file="$SCRATCH/msg1.txt"
cat > "$msg_file" <<'EOF'
feat: add widget

Body line one.
Claude-Session: https://claude.ai/code/session_abc123def456
Signed-off-by: t <t@t.co>
EOF
echo y > "$wt/g.txt"
git -C "$wt" add g.txt
git -C "$wt" commit -q -F "$msg_file"

landed="$(git -C "$wt" log -1 --format=%B)"
assert_no_session_url "$landed" "AC1"
expected=$'feat: add widget\n\nBody line one.\nSigned-off-by: t <t@t.co>'
[[ "$landed" == "$expected" ]] || fail "AC1: rest of the message changed

got: $landed"

# --- AC2: a bare-pasted session URL line is stripped too ---

msg_file="$SCRATCH/msg2.txt"
cat > "$msg_file" <<'EOF'
fix: rewrite handler

Pasted from my browser:
https://claude.ai/code/session_987xyz654
Ends here.
EOF
echo z > "$wt/h.txt"
git -C "$wt" add h.txt
git -C "$wt" commit -q -F "$msg_file"

landed="$(git -C "$wt" log -1 --format=%B)"
assert_no_session_url "$landed" "AC2"
[[ "$landed" == *$'Ends here.'* ]] || fail "AC2: lines after the bare URL were lost"

# --- AC3: a pre-existing third-party commit-msg hook still runs (chained
# ahead of its bare exit), and our strip still lands ---

third_party="$SCRATCH/third-party"
mkdir -p "$third_party"
rm -rf "$common_hooks/commit-msg"
cat > "$common_hooks/commit-msg" <<EOF
#!/bin/sh
echo third-party-ran >> "$third_party/log"
exit 0
EOF
chmod +x "$common_hooks/commit-msg"

run_install_snippet "$wt"

msg_file="$SCRATCH/msg3.txt"
cat > "$msg_file" <<'EOF'
feat: chained hook check

Claude-Session: https://claude.ai/code/session_chain000
Tail line.
EOF
echo w > "$wt/i.txt"
git -C "$wt" add i.txt
git -C "$wt" commit -q -F "$msg_file"

landed="$(git -C "$wt" log -1 --format=%B)"
assert_no_session_url "$landed" "AC3 (strip)"
[[ "$landed" == *$'Tail line.'* ]] || fail "AC3: message tail lost"
[[ "$(cat "$third_party/log" 2>/dev/null)" == *"third-party-ran"* ]] \
  || fail "AC3: pre-existing commit-msg hook never ran (prepended body unreachable after its exit?)"

# --- AC4: re-running setup is idempotent - no double prepend ---

run_install_snippet "$wt"
count="$(grep -c '_fno_strip_hook=' "$common_hooks/commit-msg")"
[[ "$count" -eq 1 ]] || fail "AC4: expected exactly one dispatcher body, found $count"

# --- AC5: prose that merely names the concept passes byte-identical ---

msg_file="$SCRATCH/msg4.txt"
cat > "$msg_file" <<'EOF'
docs: explain the session-URL gate

The gate matches a claude.ai/code path only when a token char follows,
so prose naming the concept is not a violation.
EOF
echo v > "$wt/j.txt"
git -C "$wt" add j.txt
git -C "$wt" commit -q -F "$msg_file"

landed="$(git -C "$wt" log -1 --format=%B)"
expected_prose=$'docs: explain the session-URL gate\n\nThe gate matches a claude.ai/code path only when a token char follows,\nso prose naming the concept is not a violation.'
[[ "$landed" == "$expected_prose" ]] || fail "AC5: prose naming the concept was mangled

got: $landed"

# --- AC6: a worktree whose checkout predates the strip script must still be
# able to commit - the shim's guard-for-absent-hook branch exits 0, never
# aborting the commit (the salvage test caught exactly this regression).

old_wt="$SCRATCH/old-wt"
git -C "$canonical" worktree add -q -b old-branch "$old_wt"
git -C "$old_wt" config user.email t@t.co
git -C "$old_wt" config user.name t
echo old > "$old_wt/k.txt"
git -C "$old_wt" add k.txt
git -C "$old_wt" commit -q -m "older worktree commit" \
  || fail "AC6: commit aborted in a worktree lacking the checked-out strip script"

echo "PASS: session-URL strip commit-msg hook (6 checks)"
