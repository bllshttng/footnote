#!/usr/bin/env bash
# End-to-end smoke test: target loop under hermes-agent.
#
# Verifies the loop wrapper + sentinel + hermes integration. Uses a trivial
# hello-world prompt that completes in one iteration so the test is fast
# and model-stable.
#
# Exit codes:
#   0  loop completed with promise (one iteration, sentinel written)
#   1  test failed (see stderr)
#   77 skipped - hermes-agent not on PATH or checkout not present

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
FIXTURE="${SCRIPT_DIR}/fixtures/hello-world-prompt.txt"

log() { printf '[hermes-smoke] %s\n' "$*"; }
fail() { printf '[hermes-smoke] FAIL: %s\n' "$*" >&2; exit 1; }
skip() { printf '[hermes-smoke] SKIP: %s\n' "$*" >&2; exit 77; }

# 0. Prereqs
command -v hermes-agent &>/dev/null || skip "hermes-agent not on PATH"
command -v git &>/dev/null || fail "git required"
[[ -f "$FIXTURE" ]] || fail "missing fixture at $FIXTURE"

HERMES_REPO="${HERMES_REPO:-$HOME/code/tools/bots/hermes-agent}"
[[ -d "$HERMES_REPO" ]] || skip "hermes-agent checkout not found at $HERMES_REPO"

# 1. Throwaway worktree of hermes-agent
WORKTREE=$(mktemp -d -t hermes-smoke-XXXXXX)
BRANCH="fno-smoke-$(date +%s)"
(cd "$HERMES_REPO" && git worktree add -b "$BRANCH" "$WORKTREE" 2>/dev/null) || {
  rm -rf "$WORKTREE"
  fail "could not create hermes worktree"
}

cleanup() {
  (cd "$HERMES_REPO" && git worktree remove "$WORKTREE" --force 2>/dev/null || true)
  (cd "$HERMES_REPO" && git branch -D "$BRANCH" 2>/dev/null || true)
  [[ -L "${SKILL_LINK:-}" && "${CREATED_SKILL_LINK:-0}" == "1" ]] && rm -f "${SKILL_LINK}"
  [[ -L "${PLUGIN_LINK:-}" && "${CREATED_PLUGIN_LINK:-0}" == "1" ]] && rm -f "${PLUGIN_LINK}"
}
trap cleanup EXIT

cd "$WORKTREE"
log "worktree=$WORKTREE"

# 2. Symlink fno skills into ~/.hermes/skills/fno (idempotent).
# Verify an existing path points at the same tree - otherwise the test would
# silently exercise a different install.
SKILL_LINK="$HOME/.hermes/skills/fno"
EXPECTED_SKILL_TARGET="${REPO_ROOT}/skills"
CREATED_SKILL_LINK=0
if [[ ! -e "$SKILL_LINK" ]]; then
  mkdir -p "$(dirname "$SKILL_LINK")"
  ln -sfn "$EXPECTED_SKILL_TARGET" "$SKILL_LINK"
  CREATED_SKILL_LINK=1
else
  actual=$(readlink "$SKILL_LINK" 2>/dev/null || echo "$SKILL_LINK")
  [[ "$actual" == "$EXPECTED_SKILL_TARGET" ]] || \
    skip "$SKILL_LINK already exists but points to $actual (expected $EXPECTED_SKILL_TARGET)"
fi

# 3. Symlink promise-tag reader plugin (idempotent)
PLUGIN_LINK="$HOME/.hermes/plugins/promise-tag-reader"
EXPECTED_PLUGIN_TARGET="${REPO_ROOT}/plugins/hermes/promise-tag-reader"
CREATED_PLUGIN_LINK=0
if [[ ! -e "$PLUGIN_LINK" ]]; then
  mkdir -p "$(dirname "$PLUGIN_LINK")"
  ln -sfn "$EXPECTED_PLUGIN_TARGET" "$PLUGIN_LINK"
  CREATED_PLUGIN_LINK=1
else
  actual=$(readlink "$PLUGIN_LINK" 2>/dev/null || echo "$PLUGIN_LINK")
  [[ "$actual" == "$EXPECTED_PLUGIN_TARGET" ]] || \
    skip "$PLUGIN_LINK already exists but points to $actual (expected $EXPECTED_PLUGIN_TARGET)"
fi

# 4. Initialize a minimal target-state.md so the wrapper has state to read
mkdir -p .fno
cat > .fno/target-state.md <<'EOF'
---
status: IN_PROGRESS
current_phase: smoke
iteration: 1
max_iterations: 3
input: "hermes smoke test"
multi_plan_mode: false
---
EOF

# 5. Invoke the wrapper with the hello-world prompt.
# NOTE: do NOT use `if ! cmd; then EXIT=$?`. `!` negates the exit code, so
# `$?` inside the then-branch is always 0 and the 77-skip path never fires.
# Run the command directly, capture $?, then branch.
log "invoking wrapper..."
set +e
bash "${REPO_ROOT}/scripts/run-target-loop.sh" \
  --driver hermes --max-iter 3 \
  --prompt-file "$FIXTURE" > .fno/smoke-output.log 2>&1
EXIT=$?
set -e
if [[ "$EXIT" != "0" ]]; then
  if [[ "$EXIT" == "77" ]]; then skip "wrapper reported driver not available"; fi
  log "wrapper output:"
  sed -e 's/^/  /' .fno/smoke-output.log >&2
  fail "wrapper exited $EXIT"
fi

# 6. Assertions
[[ -f .fno/target-promise.signal ]] || fail "sentinel file not written"
grep -q 'MISSION COMPLETE' .fno/target-promise.signal || \
  fail "sentinel did not contain MISSION COMPLETE"
grep -q 'iteration 1' .fno/target-loop.log || \
  fail "log did not show iteration 1"

log "PASS: hermes smoke test complete"
