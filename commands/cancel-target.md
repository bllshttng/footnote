---
name: cancel-target
description: Cancel an active target pipeline
---

# Cancel Target

Cancels a target pipeline. Behavior depends on whether a state file exists:

- **Live session (`target-state.md` present):** assert the `.target-cancelled`
  signal and let the stop hook author `status: BLOCKED` on the next stop. This
  is the sanctioned cancel path (`has_external_cancel_signal` in
  `scripts/lib/cancel-signal.sh`) and is independent of the transcript-id match
  and the "latest user turn" check, so it works whether a human types the
  command or the assistant invokes the skill. The state file is NOT removed:
  removing it re-arms the orphan detector (the transcript permanently records
  the `/fno:target` invocation), which re-blocks exit on every stop.
  Leaving it lets the hook write `BLOCKED` as a durable terminal record
  (postmortem + ledger + the backlog node returning to `ready`); the next
  `fno target init` archives that terminal state cleanly.

- **Orphan (no state file):** the session was driven off-ceremony (init
  skipped) or a prior cancel removed the file. Clearing the orphan block
  requires a genuine human-typed `/fno:target cancel` (the anti-forgery
  factor the assistant cannot satisfy by invoking this skill itself). The skill
  writes a session-keyed tombstone so a human's command is honored; the
  orphan block is bounded (it self-terminates after a few stops and records the
  bypass) so an unattended loop cannot burn credits indefinitely.

```bash
STATE_DIR=".fno"
STATE_FILE="$STATE_DIR/target-state.md"
SENTINEL="$STATE_DIR/.target-cancelled"
TOMBSTONE="$STATE_DIR/.target-cancelled-final"

if [[ -f "$STATE_FILE" ]]; then
  echo "Current target state:"
  grep -E "^(status|current_phase|iteration):" "$STATE_FILE" || true
  echo ""
  # Only an IN_PROGRESS session is cancellable. The hook's cancel writer runs
  # BEFORE the terminal-state case and rewrites any non-BLOCKED status to
  # BLOCKED, so touching the sentinel on a COMPLETE/ABORTED session would
  # downgrade a finished run and re-run its claim/backlog handling as a cancel
  # (Codex P2 on PR #391). Gate on status, not mere file existence.
  _cs_status=$(grep -E '^status:' "$STATE_FILE" | head -1 \
    | sed -e 's/^status:[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'\$//")
  if [[ "$_cs_status" != "IN_PROGRESS" ]]; then
    echo "Target session is already '${_cs_status:-unknown}' (terminal) - nothing to cancel."
  else
    # Assert the sanctioned cancel signal. has_external_cancel_signal() honors a
    # .target-cancelled whose mtime is at or after the state file's created_at.
    # The hook then writes status: BLOCKED itself and exits cleanly on the next
    # stop. We deliberately keep the state file.
    touch "$SENTINEL"
    echo "✓ Cancel signal asserted ($SENTINEL)."
    echo "  The stop hook will write status: BLOCKED and exit on the next stop."
  fi
else
  # No state file: orphan / off-ceremony session. Write the session-keyed
  # tombstone so that, when a HUMAN types /fno:target cancel, the orphan
  # detector honors it (the assistant invoking this skill cannot self-clear an
  # orphan - that is the anti-forgery factor). Key the tombstone to
  # CLAUDE_CODE_SESSION_ID, which equals the stop hook's HOOK_INPUT.session_id
  # and is robust even though no state file survives to read
  # claude_transcript_id from.
  TID="${CLAUDE_CODE_SESSION_ID:-}"
  # In an off-ceremony orphan, .fno/ may not exist yet; without this the
  # redirect below fails ("No such file or directory") and the cancel silently
  # writes nothing (Gemini MEDIUM on PR #391).
  mkdir -p "$STATE_DIR"
  {
    echo "claude_transcript_id=${TID}"
    echo "cancelled_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } > "$TOMBSTONE"
  if [[ -n "$TID" ]]; then
    echo "✓ Cancel tombstone written ($TOMBSTONE), keyed to this session."
    echo "  When a human types /fno:target cancel, the orphan detector"
    echo "  honors it and allows a clean exit. The assistant cannot self-clear"
    echo "  an orphan block; if no human is watching, the orphan block"
    echo "  self-terminates after a few stops and the gate-bypass is recorded."
  else
    echo "⚠ CLAUDE_CODE_SESSION_ID is unset; cannot key the tombstone to this" >&2
    echo "  session. A human can close a genuinely-shipped off-ceremony session" >&2
    echo "  as COMPLETE with /fno:target override <reason>." >&2
  fi
fi
```

On the next stop, for a live session the hook reads `.target-cancelled` via
`has_external_cancel_signal`, writes `status: BLOCKED`, generates a postmortem,
returns the backlog node to `ready`, and allows a clean exit. For an orphan, a
human-typed `/fno:target cancel` is honored via the tombstone;
`init-target-state.sh` clears both `.target-cancelled` and the tombstone the
next time a target session starts in this worktree.
