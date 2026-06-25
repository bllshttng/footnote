#!/usr/bin/env bash
# spawn.sh - the honest-receipt core of /fno:agent (spawn verb).
#
# Runs the GENUINE `fno agents spawn|host <name> "<message>" --provider <p>`
# launch and reports ONLY a real captured receipt (short-id or one-shot reply). This is the cardinal guard (AC5-FR):
# the motivating session was a non-execution that *looked* like a command and
# was reported as success. Here the success/FAILED decision is deterministic
# shell, not model judgment: no 8-hex short-id in the real stdout => FAILED with
# the real stderr, never a fabricated uuid.
#
# The SKILL.md owns the CONFIRM gate; spawn.sh runs only after a yes. It still
# re-checks for a live duplicate right before the spawn (atomic, closing the
# window between the SKILL's read-only pre-check and here).
#
# Self-contained skill script. External deps: bash + `fno` (agents, claim) + jq.
#
# Usage:
#   spawn.sh --name <n> --provider <p> --message "<msg>" [--node <ab-XXXX>] [--cwd <dir>]
#
# Outcome (one line on stdout; NEVER silent, NEVER a fabricated uuid):
#   result=launched short_id=<hex> name=<n> hint="fno agents logs <n>" trace="fno agents trace <n>"
#   result=already-running name=<n> reason="<why>"
#   result=failed reason="<real captured error>"
# Exit: 0 launched | 0 already-running | 1 failed.

set -uo pipefail

NAME=""
PROVIDER=""
MESSAGE=""
NODE=""
CWD=""
# Routing inputs (codex/gemini first-class dispatch, ab-417ab20f). Defaults keep
# the legacy claude path equivalent: exec + build -> claude resolves to `spawn`
# (Group 1 ab-8b3e4fe0: ask never creates), so an old caller that passes none
# of these still launches a persistent claude bg peer.
MODE="exec"            # exec | interactive  (-i routes codex/gemini -> host)
PAYLOAD_MODE="build"   # build | ask | passthrough (ask -> spawn --once)
YOLO=0                 # 1 appends --yolo to the spawn/host argv
FRESH=0                # 1 appends --fresh (canonical-root cwd) to the spawn argv
HERE=0                 # 1 appends --here (opt out of --fresh) to the spawn argv

# Dispatcher reservation state (Guard 2). Initialized up-front so fail() can
# reference it safely under set -u even before the reservation is acquired.
RES_KEY=""
RES_HOLDER="dispatch-skill:$$"
RES_HELD=0
release_reservation() {
  [[ "$RES_HELD" -eq 1 && -n "$RES_KEY" ]] \
    && fno claim release "$RES_KEY" --holder "$RES_HOLDER" >/dev/null 2>&1
  RES_HELD=0
  return 0
}

# fail() is only reached on non-launch paths, so always release any reservation
# we hold (keeps the node re-dispatchable). A successful launch never calls fail.
fail() { release_reservation; printf 'result=failed reason="%s"\n' "$1"; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)         NAME="${2:-}"; shift 2 ;;
    --provider)     PROVIDER="${2:-}"; shift 2 ;;
    --message)      MESSAGE="${2:-}"; shift 2 ;;
    --node)         NODE="${2:-}"; shift 2 ;;
    --cwd)          CWD="${2:-}"; shift 2 ;;
    --mode)         MODE="${2:-}"; shift 2 ;;
    --payload-mode) PAYLOAD_MODE="${2:-}"; shift 2 ;;
    --yolo)         YOLO=1; shift ;;
    # Pass-through cwd flags (ab-77b691dc): forwarded to `fno agents spawn` so a
    # target-class dispatcher can request canonical-root cwd. NOT defaulted here:
    # plain interactive ask/host/spawn keep caller cwd unless asked (AC3).
    --fresh)        FRESH=1; shift ;;
    --here|--in-place) HERE=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

command -v fno >/dev/null 2>&1 || fail "fno not on PATH"
command -v jq  >/dev/null 2>&1 || fail "jq not on PATH"
[[ -n "$NAME" ]]     || fail "missing --name"
[[ -n "$PROVIDER" ]] || fail "missing --provider"

# ---- Verb selection (Locked Decisions 1 + 2; Group 1 ab-8b3e4fe0) -------
# `ask` never creates (bus epic Group 1): creation is always spawn/host. claude
# routes to `spawn` (client-side `claude --bg --name`, subscription lane, JSON
# receipt); codex/gemini build dispatch routes to `spawn` (exec, autonomous) by
# default and `host` (interactive, human-driven) under `-i`; ask-mode is the
# one-shot exchange and routes to `spawn --once` (the old ask-create lineage,
# reply on stdout + teardown receipt on stderr). A codex/gemini passthrough
# never reaches here (normalize.sh refuses it); a claude passthrough runs the
# slash command via `spawn`.
ONCE=0
if [[ "$PROVIDER" == "claude" ]]; then
  VERB="spawn"
elif [[ "$PAYLOAD_MODE" == "ask" ]]; then
  VERB="spawn"; ONCE=1
elif [[ "$MODE" == "interactive" ]]; then
  VERB="host"
else
  VERB="spawn"
fi

# A non-host launch needs a task; only a bare interactive host may be idle
# (AC2-EDGE). Reject an empty exec/ask payload before any billed launch.
if [[ -z "$MESSAGE" && "$VERB" != "host" ]]; then
  fail "missing --message (only a host/interactive launch may have an empty task)"
fi

sanitize() { printf '%s' "$1" | tr '\n\r' '  ' | sed 's/"/'"'"'/g' | cut -c1-300; }

# ---- Guards 1+2 via the shared spawn-guard verb (x-73cc) -----------------
# The race-critical node:<id> claim probe (Guard 1) + create-only dispatch:<id>
# reservation (Guard 2) live in `fno agents spawn-guard` so this path and the
# /target bg path (skills/target/scripts/dispatch-node.sh) can never drift on
# the part that matters. Only a NODE dispatch is guarded; a free-text / handoff
# / discuss launch (no --node) skips the guard exactly as before. The verb does
# Guard 1 then Guard 2 in one process: a `dispatchable` verdict means it has
# acquired dispatch:<node> for $RES_HOLDER (released on every non-launch path
# via fail(); left to TTL-expire on a launch). Fail CLOSED: a stale `fno`
# without the verb (or any non-clean/unparseable verdict) refuses to launch.
if [[ -n "$NODE" ]]; then
  RES_KEY="dispatch:$NODE"
  # Pin to the Python runtime: spawn-guard is a Python-only verb, so an operator
  # with FNO_AGENTS_RUNTIME=rust exported would otherwise route it to the Rust
  # binary (which lacks it -> 127 -> fail-closed, breaking node dispatch). The
  # inline override is scoped to this call only; the real spawn below routes
  # normally (codex P2 parity with dispatch-node.sh).
  guard_out="$(FNO_AGENTS_RUNTIME=python fno agents spawn-guard "$NODE" --holder "$RES_HOLDER" --ttl 3m --json 2>/dev/null)"; guard_rc=$?
  guard_json="$(printf '%s\n' "$guard_out" | grep -F '"verdict"' | head -1)"
  verdict="$(printf '%s' "$guard_json" | jq -r '.verdict // empty' 2>/dev/null)"
  case "$verdict" in
    already-running)
      reason="$(printf '%s' "$guard_json" | jq -r '.reason // empty' 2>/dev/null)"
      if [[ "$reason" == "live-claim" ]]; then
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null)"
        printf 'result=already-running name=%s reason="live worker holds node:%s (%s)"\n' "$NAME" "$NODE" "$holder"
      else
        printf 'result=already-running name=%s reason="a peer dispatcher holds %s (racing launch)"\n' "$NAME" "$RES_KEY"
      fi
      exit 0 ;;
    corrupted)
      fail "node:$NODE claim is corrupted; force-release or repair before dispatching" ;;
    dispatchable)
      RES_HELD=1 ;;
    *)
      # verdict=error, OR empty/unparseable (a stale fno WITHOUT the verb prints
      # Typer "No such command" + exits non-zero; or a probe crash): fail CLOSED.
      detail="$(printf '%s' "$guard_json" | jq -r '.detail // empty' 2>/dev/null)"
      fail "${detail:-spawn-guard unavailable (rc=$guard_rc); not dispatching to avoid a double-launch}" ;;
  esac
fi

# ---- Guard 3: the agents registry ---------------------------------------
# Any NON-terminal status means a worker is present - report already-running
# rather than `fno agents rm`-ing it (codex P2: ready/idle/busy are drive-
# eligible workers, not just `live`; removing one then respawning double-launches).
# Only a clearly-dead row is removed so `spawn` creates fresh instead of
# colliding with it. An unknown/unexpected status is treated as present (fail-safe).
# Capture the probe output AND its exit code separately so a crashed/truncated
# `fno agents list` (daemon down, jq parse error) fails CLOSED instead of
# collapsing to "no agent row" and double-launching. For a free-form / --name-only
# dispatch (no --node) this is the ONLY duplicate guard, so a swallowed probe
# error here is the highest-value gap (silent-failure-hunter on PR #433, cv-dddd8ae5).
# Mirrors the claim-probe fail-closed pattern above.
agents_json="$(fno agents list 2>/dev/null)"; list_rc=$?
if [[ "$list_rc" -ne 0 ]]; then
  fail "agents-list probe failed (rc=$list_rc); not dispatching to avoid a double-launch"
fi
if ! printf '%s' "$agents_json" | jq -e 'has("agents")' >/dev/null 2>&1; then
  fail "agents-list probe returned no parseable {agents:[...]}; not dispatching to avoid a double-launch"
fi
existing_status="$(printf '%s' "$agents_json" \
  | jq -r --arg n "$NAME" '.agents[]? | select(.name==$n) | .status' 2>/dev/null | head -1)"
case "$existing_status" in
  "")
    : ;;  # no agent row -> proceed to spawn
  exited|orphaned|dead|stopped|failed|terminated|killed)
    fno agents rm "$NAME" >/dev/null 2>&1 || true ;;  # dead -> clear, then spawn fresh
  *)
    # live|ready|idle|busy|spawning|restarting|<unknown> -> worker present.
    release_reservation
    printf 'result=already-running name=%s reason="an agent named %s exists (status=%s)"\n' "$NAME" "$NAME" "$existing_status"
    exit 0 ;;
esac

# ---- Auto-worktree for code-implementing payloads (x-9c4c) --------------
# A bg /target|/do|/fix launched into a repo's MAIN checkout lands on the
# canonical (often protected) branch and relies on the soft skill instruction
# "a bg /target self-creates its worktree before building." Do it
# deterministically here instead: create ~/conductor/workspaces/<repo>/<name>
# on a fresh feature branch and launch THERE, so the worker is born isolated
# (location verdict ok from line one) regardless of whether the cwd came from
# -P, a node's _resolved_cwd, or the caller sitting on canonical main.
#
# Scope: code payloads only -- a born-with-why /think dispatch writes a design
# doc, not commits, so it stays in repo root (failure mode 1). Fail-safe: any
# error keeps the original cwd so the prior self-worktree path still applies;
# the launch is NEVER blocked (failure mode 2). The <repo>/<name> path is
# deterministic per dispatch, so a re-spawn of the same node reuses it rather
# than colliding (failure mode 3).
AUTO_WT=""
maybe_auto_worktree() {
  case "$MESSAGE" in
    /target|/target\ *|/do|/do\ *|/fix|/fix\ *) : ;;
    *) return 0 ;;  # not a code-implementing payload (e.g. /think) -> no worktree
  esac
  command -v git >/dev/null 2>&1 || return 0
  local base="${CWD:-$PWD}"
  [[ -d "$base" ]] || return 0
  local top common gdir
  top="$(git -C "$base" rev-parse --show-toplevel 2>/dev/null)" || return 0
  [[ -n "$top" ]] || return 0
  # git-dir / git-common-dir may be returned relative to $base (e.g. a subdir cwd
  # gives `../../.git`). cd into each and `pwd -P` to canonicalize -> subdir- and
  # symlink-proof. A linked worktree's git-dir is .../.git/worktrees/<x> != the
  # common dir; only re-isolate a MAIN checkout (the two resolve equal).
  common="$(cd "$base" 2>/dev/null && cd "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd -P)" || return 0
  gdir="$(cd "$base" 2>/dev/null && cd "$(git rev-parse --git-dir 2>/dev/null)" 2>/dev/null && pwd -P)" || return 0
  [[ -n "$common" && "$gdir" == "$common" ]] || return 0

  local repo="${top##*/}"
  local wt="$HOME/conductor/workspaces/$repo/$NAME"
  # Re-spawn of the same node -> same path; reuse it if it is already a worktree
  # rooted at $wt (`-ef` compares inode, so a /var<->/private/var symlink can't
  # fool it). Never clobber a stray same-named dir that is not a worktree.
  if [[ -d "$wt" ]] && git -C "$wt" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
     && [[ "$(cd "$wt" && git rev-parse --show-toplevel 2>/dev/null)" -ef "$wt" ]]; then
    CWD="$wt"; AUTO_WT="$wt"; printf 'auto-worktree: reusing %s\n' "$wt" >&2; return 0
  fi
  [[ -e "$wt" ]] && { printf 'auto-worktree: %s exists but is not a worktree; launching in %s\n' "$wt" "$top" >&2; return 0; }
  mkdir -p "$(dirname "$wt")" 2>/dev/null || return 0

  local branch="feature/$NAME"
  if git -C "$top" show-ref --verify --quiet "refs/heads/$branch"; then
    git -C "$top" worktree add "$wt" "$branch" >/dev/null 2>&1 \
      || { printf 'auto-worktree: git worktree add failed; launching in %s\n' "$top" >&2; return 0; }
  else
    git -C "$top" worktree add "$wt" -b "$branch" >/dev/null 2>&1 \
      || { printf 'auto-worktree: git worktree add failed; launching in %s\n' "$top" >&2; return 0; }
  fi
  # Link gitignored shared state (footnote-ecosystem only; absent -> skip).
  local setup="$top/scripts/setup/setup-worktree.sh"
  [[ -f "$setup" ]] && CANONICAL="$top" WORKTREE="$wt" bash "$setup" >/dev/null 2>&1
  CWD="$wt"; AUTO_WT="$wt"
  printf 'auto-worktree: created %s on %s\n' "$wt" "$branch" >&2
}
maybe_auto_worktree   # self-gating: no-op unless code payload + main checkout

# ---- Spawn (subscription lane only) -------------------------------------
# Run the GENUINE verb. claude `spawn` builds `claude --bg --name <name> <msg>`
# client-side (Group 1 ab-8b3e4fe0 moved the create off `ask`); codex/gemini
# `spawn`/`host` are daemon-managed PTY workers (Locked Decision 1) and
# `spawn --once` is the ephemeral one-shot. Name is POSITIONAL (Locked
# Decision 8). NEVER -p/--bare. --yolo is appended only when the user
# explicitly passed it (normalize.sh strips it for claude). A bare interactive
# host omits the message positional (a valid idle session). The cmd array
# always carries at least `agents <verb> --provider <p> <name>`, so
# "${cmd[@]}" is never an empty expansion (bash 3.2 set -u safe).
#
# stdout and stderr are captured SEPARATELY (via a temp file) so the receipt
# parse only ever sees stdout: a stderr warning (incl. the --once teardown
# receipt) can never be mistaken for a short-id, and the failure reason still
# carries the real stderr.
cmd=(agents "$VERB" --provider "$PROVIDER")
[[ -n "$CWD" ]] && cmd+=(--cwd "$CWD")
[[ "$FRESH" -eq 1 ]] && cmd+=(--fresh)
[[ "$HERE" -eq 1 ]] && cmd+=(--here)
[[ "$YOLO" -eq 1 ]] && cmd+=(--yolo)
[[ "$ONCE" -eq 1 ]] && cmd+=(--once)
cmd+=("$NAME")
[[ -n "$MESSAGE" ]] && cmd+=("$MESSAGE")

err_file="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/agents-spawn-$$.err")"
# EXIT trap guarantees cleanup even if interrupted (SIGINT) mid-spawn; the manual
# rm below covers the normal path so the file is gone before any later use.
trap 'rm -f "$err_file"' EXIT
spawn_out="$(fno "${cmd[@]}" 2>"$err_file")"; spawn_rc=$?
spawn_err="$(cat "$err_file" 2>/dev/null)"; rm -f "$err_file"

if [[ "$spawn_rc" -ne 0 ]]; then
  fail "dispatch failed (rc=$spawn_rc): $(sanitize "${spawn_err:-$spawn_out}")"
fi

# ---- Honest receipt (the cardinal guard) --------------------------------
# Receipt family is keyed by the VERB/mode the skill ran (never by sniffing
# the output - Locked Decision 3):
#   spawn --once         -> a CLIENT-SIDE one-shot (`codex exec` / `gemini -p`,
#                          the old ask-create lineage, Group 1 ab-8b3e4fe0):
#                          stdout is the model REPLY verbatim, NOT a short-id
#                          (the teardown receipt rides stderr). Success = rc==0
#                          (checked above) AND a non-empty reply; the reply IS
#                          the deliverable. Empty reply -> FAILED, never faked.
#   spawn|host           -> stdout is JSON carrying {"short_id",...} (pretty
#                          multi-line from the daemon; one compact line from
#                          the client-side claude spawn). Parse `.short_id`
#                          with jq, validate whole-string 8-hex.
#                          Empty/missing/non-8-hex (even on exit 0) is FAILED.
if [[ "$ONCE" -eq 1 ]]; then
  # spawn --once reply receipt. Trim whitespace for the empty check only;
  # the full reply is relayed verbatim.
  reply_trimmed="${spawn_out//[$' \t\r\n']/}"
  if [[ -z "$reply_trimmed" ]]; then
    fail "empty reply (spawn --once returned no content on exit 0): $(sanitize "${spawn_err:-(no stderr)}")"
  fi
  # The reply IS the deliverable (no lasting peer for a one-shot). Outcome line
  # first, then the full reply verbatim for the skill to preview.
  printf 'result=replied name=%s reply_chars=%s hint="fno agents logs %s"\n' \
    "$NAME" "${#spawn_out}" "$NAME"
  printf '%s\n' "$spawn_out"
  exit 0
else
  short_id="$(printf '%s' "$spawn_out" | jq -r '.short_id // empty' 2>/dev/null)"
  # WHOLE-string match (not `grep -qx`, which matches ANY line): a multi-line
  # `.short_id` value - e.g. `{"short_id":"junk\ndeadbeef"}` or a banner leaking
  # into the value - must NOT pass on one of its lines being 8-hex. `[[ =~ ]]`
  # anchors `^...$` to the whole string, so any embedded newline or stray byte
  # fails (parity with the ask path's single-line requirement). bash 3.2 safe.
  if [[ ! "$short_id" =~ ^[0-9a-f]{8}$ ]]; then
    fail "no valid short-id receipt ($VERB JSON .short_id empty/missing/not-8-hex): $(sanitize "${spawn_out:-$spawn_err}")"
  fi
fi

# ---- Report (mode-aware) ------------------------------------------------
# host is interactive: STAGED, not running yet - the user drives it later. Every
# other verb here is a BACKGROUND launch (claude/codex/gemini spawn) whose
# progress streams to logs. (spawn --once already returned above.)
if [[ "$VERB" == "host" ]]; then
  printf 'result=launched short_id=%s name=%s mode=interactive staged="not running yet" drive="fno agents grid %s"\n' \
    "$short_id" "$NAME" "$NAME"
else
  # Surface the auto-worktree cwd so an isolated launch is never silent.
  wt_field=""; [[ -n "$AUTO_WT" ]] && wt_field=" cwd=$AUTO_WT"
  printf 'result=launched short_id=%s name=%s mode=exec%s hint="fno agents logs %s" trace="fno agents trace %s"\n' \
    "$short_id" "$NAME" "$wt_field" "$NAME" "$NAME"
fi
exit 0
