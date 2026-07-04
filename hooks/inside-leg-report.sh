#!/usr/bin/env bash
# hooks/inside-leg-report.sh -- the inside leg (inside-out E3.2).
#
# A per-turn hook that pushes structured agent state OUTWARD so a grid pane badge
# is fact, not a scrape guess. Wired to two Claude Code events in hooks.json:
#   UserPromptSubmit -> state=working   (the turn started)
#   Stop             -> state=done       (the turn finished)
# The desired state is the first argument ($1). `blocked` is in the contract but
# has no natural Claude Code hook trigger yet (no Notification event wired), so
# E3.2 emits working/done only; a future permission/idle trigger can push blocked
# through the same verb.
#
# Chain: this hook -> `fno agents report` (the thin verb) -> agent.report RPC ->
# the daemon STORES the latest state on the matching claude row. The match keys
# on the daemon-pinned session_id (the same uuid Claude Code passes here), so a
# pane reports under the id E1 recorded.
#
# Fire-and-forget by design:
#   - It NEVER blocks or reds a turn (UserPromptSubmit/Stop are non-blocking; this
#     script always exits 0).
#   - The verb sends to an ALREADY-RUNNING daemon and never boots one, so for a
#     plain claude session with no daemon (the common non-grid case) the report
#     is a cheap no-op. When a daemon IS up but this session is not a registered
#     pane, the daemon drops the report as unknown_session -- the daemon is the
#     filter, so this hook needs no "am I a grid pane?" gate.
# ponytail: fires for every claude session; the daemon-presence + unknown-session
# drop is the gate. A per-session opt-in lands if the dropped-report noise ever
# matters.
#
# Turn-block markers (additive, mux panes only): inside a mux pane this hook
# ALSO writes OSC 133 to /dev/tty so the mux block scanner segments the pane's
# history by agent TURNS -- `133;C` when a turn starts (working), `133;D;0`
# when it ends (done). The pane PTY is this process's controlling terminal, so
# the bytes enter the exact stream vt.rs scans; hook-emitted markers are
# indistinguishable from shell-emitted ones (blocks open on C, finalize on D;
# A/B are boundary no-ops for the block store). Emission is gated on FNO_PANE
# (set by pty.rs in every pane child env) so a non-pane terminal never sees
# invisible OSC spray, and it inherits the fire-and-forget contract: a
# marker-write failure is silent and never blocks the turn or the report.
# Exit is always 0 in v1: the Stop payload carries no cheap error signal, and
# the turn's on-screen content is its label -- no custom param vocabulary.
#
# Marker emission is gated TWICE: FNO_PANE presence AND a first-writer
# session-identity pin, so only the claude pty.rs spawned INTO the pane emits. A
# nested `claude -p` inherits FNO_PANE + the ctty but loses the pin race and
# stays silent, so it no longer splits the outer turn. On a blocked Stop (the
# /target loop) the `done` marker also RE-OPENS a block, so every loop leg is
# segmented, not just the first.
#
# Remaining accepted limit (the scanner CONTAINS it -- a stray C finalizes the
# prior block with unknown exit, a D with no open block is a no-op, so
# segmentation degrades, never corrupts): a user interrupt fires no Stop, so
# that turn's block stays open until the next turn's C finalizes it. The
# continuation re-open likewise leaves one empty block open when the loop ends,
# finalized by the next turn's C.

set -uo pipefail

STATE="${1:-working}"
case "$STATE" in
  working | blocked | done) ;;
  *) STATE="working" ;;
esac

# Read the hook payload once. The session id both GATES marker emission (only
# THE pane host emits) and LABELS the state report; the monotonic seq orders the
# report. seq is `time.monotonic_ns()`, NOT wall-clock: the daemon drops a
# report with `seq <= last_seq`, so the working/done pair of one turn MUST be
# strictly increasing. Wall-clock ns would regress if NTP steps the clock
# backward between the two reports (dropping the `done`, pinning the badge at
# `working`); the monotonic clock is host-global across processes and never
# steps back. A missing/garbled session id -> silent exit 0.
INPUT=$(cat)
PARSED=$(python3 -c '
import sys, json, time
try:
    d = json.load(sys.stdin)
    sid = d.get("session_id") or "" if isinstance(d, dict) else ""
except Exception:
    sys.exit(0)
if not sid:
    sys.exit(0)
print(f"{sid}\t{time.monotonic_ns()}")
' <<<"$INPUT" 2>/dev/null) || PARSED=""

# Keep marker emission INDEPENDENT of the parse: on a malformed/empty payload (or
# no python3) SESSION_ID stays empty and the pane host still emits via the
# presence-gate degrade. Only the state report (which needs both fields) is
# skipped, below. A clean parse yields "<session_id>\t<seq>".
SESSION_ID=""
SEQ=""
if [[ "$PARSED" == *$'\t'* ]]; then
  SESSION_ID="${PARSED%%$'\t'*}"
  SEQ="${PARSED##*$'\t'}"
fi

# Turn boundary -> OSC 133 marker, mux panes only, and only from THE pane host.
# The sink is /dev/tty (the pane PTY, this process's controlling terminal);
# FNO_TURN_MARKER_TTY overrides it for tests. Redirect-open fails silently with
# no controlling terminal (headless), hence the stderr silence + || true. A
# write to a pane whose reader stalled can block rather than fail; accepted --
# that pane is frozen anyway, and the hook's own timeout bounds it.
# Append, not truncate: a turn boundary can emit two markers (D then a re-open
# C) and each printf reopens the sink. `>>` is identical to `>` on a tty stream
# but preserves both writes when the sink is a regular file (the test seam).
TTY_SINK="${FNO_TURN_MARKER_TTY:-/dev/tty}"
emit_marker() { { printf '%b' "$1" >>"$TTY_SINK"; } 2>/dev/null || true; }

# First-writer session-identity gate: only the claude pty.rs spawned INTO the
# pane emits. It fires its first C at turn start, BEFORE it can spawn a nested
# `claude -p`, so it always wins the pin; the nested session (which inherits
# FNO_PANE + the ctty) reads a different pinned id and stays silent instead of
# spraying C mid-turn. Degrades to the v1 presence gate when there is no
# recycle-safe key (old server without FNO_PANE_EPOCH, or no session id). The
# key carries FNO_PANE_EPOCH because pane ids recycle across server restarts.
is_pane_host() {
  [[ -z "${FNO_PANE_EPOCH:-}" || -z "$SESSION_ID" ]] && return 0
  # Rendezvous dir must be a real, self-owned directory. Prefer the per-user
  # runtime dir (XDG_RUNTIME_DIR, or macOS's per-user $TMPDIR); the shared /tmp
  # last resort is hardened so a hostile pre-created dir on a multi-user box
  # cannot hijack the pin. Any anomaly degrades to the presence gate (return 0)
  # rather than writing somewhere unsafe.
  local base="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}"
  local dir="${base%/}/fno-turn-pins-${EUID:-0}"
  # Create atomically with restricted perms (-m 700): no loose-perms window and
  # no check-then-create TOCTOU (plain `mkdir`, no -p, fails if the path exists,
  # even as a symlink). On a pre-existing path, trust it only if it is a real,
  # self-owned, non-symlink dir, and force 700 there since we did not create it.
  # The parent (base) always exists, so -p is unnecessary.
  if ! mkdir -m 700 "$dir" 2>/dev/null; then
    [[ -d "$dir" && ! -L "$dir" && -O "$dir" ]] || return 0
    chmod 700 "$dir" 2>/dev/null || true
  fi
  # Every pin path component is env-controlled, so a hostile env could smuggle
  # `/` or `..` to steer the write outside the dir. FNO_PANE/FNO_PANE_EPOCH are
  # numeric by contract (pty.rs), so require digits (degrade-to-emit otherwise);
  # FNO_SESSION is a free-form name, so sanitize its separators. Deterministic,
  # so a host and its nested claude still compute the same pin path.
  [[ "$FNO_PANE" =~ ^[0-9]+$ && "$FNO_PANE_EPOCH" =~ ^[0-9]+$ ]] || return 0
  local safe_session="${FNO_SESSION:-_}"; safe_session="${safe_session//[\/.]/_}"
  local pin="${dir}/${safe_session}-${FNO_PANE}-${FNO_PANE_EPOCH}"
  # noclobber makes the create fail if the pin exists -> exactly one winner.
  if ( set -o noclobber; printf '%s\n' "$SESSION_ID" >"$pin" ) 2>/dev/null; then
    return 0
  fi
  # An empty pin means our own create half-succeeded (e.g. ENOSPC after the
  # O_EXCL create): degrade to the presence gate (emit) instead of latching the
  # host into permanent silence. A real nested claude always wrote a non-empty
  # id, so it still mismatches below and stays silent.
  [[ -s "$pin" ]] || return 0
  # `read` builtin, not a `cat` subprocess: this runs on every turn boundary. An
  # unreadable/corrupt pin (I/O error, non-regular) leaves pinned_id empty ->
  # degrade-to-emit, never latch the host silent on a read failure.
  local pinned_id=""
  read -r pinned_id <"$pin" 2>/dev/null || true
  [[ -n "$pinned_id" ]] || return 0
  [[ "$pinned_id" == "$SESSION_ID" ]]
}

# Repo root anchors the manifest check below (and the binary lookup further
# down): resolve it from the git toplevel so a session launched in a subdir
# still finds `.fno/target-state.md` deterministically, not relative to $PWD.
REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")

if [[ -n "${FNO_PANE:-}" ]] && is_pane_host; then
  case "$STATE" in
    working) emit_marker '\033]133;C\007' ;;
    done)
      emit_marker '\033]133;D;0\007'
      # Continuation re-open: a blocked Stop (the /target loop) keeps the leg
      # going, but Claude Code fires no UserPromptSubmit between legs, so without
      # this the next leg lands outside any block. Re-open here so every loop leg
      # is its own block. Gated on the mere PRESENCE of the manifest, not its
      # liveness: a stale manifest just re-opens a harmless block the scanner
      # absorbs. ponytail: over-emits one trailing empty block when the loop
      # actually ends (the scanner no-ops it, finalized by the next C); precise
      # gating needs the stop hook's block/allow verdict, which races across
      # parallel Stop hooks -- not worth the plumbing.
      [[ -f "$REPO_ROOT/.fno/target-state.md" ]] && emit_marker '\033]133;C\007'
      ;;
  esac
fi

# The state report needs a parsed session id + seq; a malformed/empty payload
# already emitted the marker above, so just skip the report here.
[[ -z "$SESSION_ID" || -z "$SEQ" ]] && exit 0

# Resolve the fno-agents binary, most-local first (mirrors target-stop-hook.sh).
BIN=""
if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
  BIN="$FNO_AGENTS_BIN"
elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/release/fno-agents" ]]; then
  BIN="${REPO_ROOT}/crates/fno-agents/target/release/fno-agents"
elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents" ]]; then
  BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
elif command -v fno-agents >/dev/null 2>&1; then
  BIN=$(command -v fno-agents)
fi
# No binary -> nothing to report to; stay silent (the inside leg is best-effort).
[[ -z "$BIN" ]] && exit 0

"$BIN" report \
  --session-id "$SESSION_ID" \
  --seq "$SEQ" \
  --state "$STATE" \
  >/dev/null 2>&1 || true

exit 0
