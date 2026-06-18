#!/usr/bin/env bash
# verify-self-short-id-propagation.sh
#
# MANUAL end-to-end verifier for backlog node ab-1e86b88e: confirms that
# FNO_AGENTS_SELF_SHORT_ID, stamped onto the PTY child by the worker
# (crates/fno-agents/src/worker.rs), propagates through the claude/codex child
# to the bash Stop / SessionStart hooks that child spawns -- the leg that makes
# drive-authority LD3 (refuse an operator-typed promise on a driven session)
# actually effective. The worker->child leg is a Rust stdlib guarantee locked by
# unit tests in worker.rs; THIS script exercises the child->hook leg that no
# unit test can reach.
#
# Mechanism: launch `claude -p` with FNO_AGENTS_SELF_SHORT_ID set in the
# environment and a capture hook injected via `--settings` (an explicit CLI
# settings file is honored without a project-trust prompt, unlike an arbitrary
# project dir). If claude passes the env var through to the hook subprocess it
# spawns, the hook records the sentinel; we then assert the recorded value
# equals the sentinel.
#
# NOT wired into CI: it spawns a real `claude -p` session (API cost + auth), so
# it must be opted into. It is named `verify-*.sh` (not `test_*.sh`) so the
# cli-ci hook-test glob never picks it up, AND it self-skips (exit 0) unless
# ALLOW_MANUAL_CLAUDE_TESTS is set -- defense in depth against a future glob.
#
# Requirements: bash 3.2+, `claude` on PATH, valid auth. No `timeout` needed
# (a background watchdog bounds the run; macOS has no coreutils `timeout`).
# Usage:  ALLOW_MANUAL_CLAUDE_TESTS=1 bash tests/hooks/verify-self-short-id-propagation.sh
#
# Provenance: drive-authority LD3 (PR #396, ab-1e86b88e). worker.rs stamps the
# env var; scripts/lib/drive-authority.sh reads ${FNO_AGENTS_SELF_SHORT_ID:-}.
set -uo pipefail

# Opt-in guard: this spends a real claude API call, so it must never run by
# accident (e.g. a future `find tests/hooks -name '*.sh' -exec` runner). Skips
# cleanly unless explicitly enabled.
if [[ -z "${ALLOW_MANUAL_CLAUDE_TESTS:-}" ]]; then
  echo "skipped: manual test (spawns a real claude session)."
  echo "  run with: ALLOW_MANUAL_CLAUDE_TESTS=1 bash $0"
  exit 0
fi

command -v claude >/dev/null 2>&1 || { echo "FAIL: claude not on PATH"; exit 4; }

SENTINEL="cl-verify-$$"
WORK="$(mktemp -d)"
[[ -n "$WORK" && -d "$WORK" ]] || { echo "FAIL: could not create temp dir"; exit 4; }
SETTINGS="$WORK/settings.json"
CAPTURE="$WORK/capture.txt"
trap '[[ -n "${WORK:-}" ]] && rm -rf "$WORK"' EXIT
: > "$CAPTURE"

cat > "$SETTINGS" <<EOF
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "printf 'START_SELF=%s\\n' \"\${FNO_AGENTS_SELF_SHORT_ID:-UNSET}\" >> $CAPTURE" } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "printf 'STOP_SELF=%s\\n' \"\${FNO_AGENTS_SELF_SHORT_ID:-UNSET}\" >> $CAPTURE" } ] }
    ]
  }
}
EOF

echo "Running: FNO_AGENTS_SELF_SHORT_ID=$SENTINEL claude -p (capture hook via --settings)"

# Run claude in the background and bound it with a watchdog (no `timeout` dep).
claude_rc=0
( cd "$WORK" && FNO_AGENTS_SELF_SHORT_ID="$SENTINEL" \
    claude -p "Reply with exactly the word: ok" \
      --settings "$SETTINGS" --setting-sources user,project,local \
      >/dev/null 2>"$WORK/stderr.txt" ) &
cpid=$!
# Run the timeout as a tracked child and kill it from a signal/EXIT trap, so the
# normal-case `kill "$wpid"` below (claude finished early) reaps BOTH the watchdog
# subshell and its `sleep`. Without the trap, killing the subshell orphans the
# `sleep 150`, which lingers in the background for the full 150s after every run.
( sleep 150 & _sleep_pid=$!
  trap 'kill "$_sleep_pid" 2>/dev/null' EXIT INT TERM
  wait "$_sleep_pid"; kill -9 "$cpid" 2>/dev/null ) &
wpid=$!
wait "$cpid" || claude_rc=$?
kill "$wpid" 2>/dev/null && wait "$wpid" 2>/dev/null || true

if [[ ! -s "$CAPTURE" ]]; then
  # Separate a setup problem (claude could not run) from a real regression so a
  # future debugger is not misled.
  if [[ "$claude_rc" -ne 0 ]]; then
    echo "INCONCLUSIVE: claude exited $claude_rc (auth/network/PATH setup, not a propagation regression)"
    tail -3 "$WORK/stderr.txt" 2>/dev/null
  else
    echo "INCONCLUSIVE: claude ran (exit 0) but no hooks fired"
  fi
  exit 3
fi

if grep -q "STOP_SELF=$SENTINEL$" "$CAPTURE" && grep -q "START_SELF=$SENTINEL$" "$CAPTURE"; then
  echo "PASS: SessionStart and Stop hooks both observed FNO_AGENTS_SELF_SHORT_ID=$SENTINEL"
  exit 0
elif grep -q "=$SENTINEL$" "$CAPTURE"; then
  echo "PARTIAL: only one hook observed the sentinel:"
  cat "$CAPTURE"
  exit 1
else
  echo "FAIL: hooks fired but did NOT see the sentinel (claude scrubbed the env):"
  cat "$CAPTURE"
  exit 1
fi
