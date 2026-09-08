#!/usr/bin/env bash
# Shared SessionStart carrier for the worktree peer overlap advisory.
#
# One path for both harness manifests: Claude registers this carrier directly
# (hooks.json); the Codex wrapper (hooks/session-start.sh) calls it with
# --body-only and folds the result into its combined context.
#
# The shared predicate (hooks/helpers/worktree-live-peers.sh) is evaluated ONCE
# in --machine mode. The advisory text is rendered from that result, then the
# same observation object is best-effort recorded via `fno agents workspace worktree
# overlap-record` and the recurrence window folded. Advisory-only and always
# exits 0: a missing CLI, a rejected payload, lock contention, an unwritable
# journal, or a degraded fold never blocks the session and never refuses a tool
# call - a failure surfaces a visible marker instead ([fno-overlap-unrecorded]
# or [fno-overlap-count-unavailable]).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BODY_ONLY=0
[[ "${1:-}" == "--body-only" ]] && BODY_ONLY=1

# Bound shelling out so a hung CLI or a slow fold over a huge journal can never
# block SessionStart. The helper ships beside this hook; if absent (should not
# happen in a real install), calls fall back to unbounded best-effort.
WT_HELPER="$SCRIPT_DIR/../scripts/lib/with-timeout.sh"
if [[ -f "$WT_HELPER" ]]; then
  # shellcheck source=/dev/null
  source "$WT_HELPER"
fi

# Evaluate the predicate once in machine mode. Empty output means no fresh peer
# (self-only, stale, missing identity, missing/malformed dir).
obs="$(bash "$SCRIPT_DIR/helpers/worktree-live-peers.sh" --machine 2>/dev/null || true)"

peer_lines=()
if [[ -n "$obs" ]]; then
peer_lines=('- Another session is working in this worktree. [fno-overlap-observed]')

# Best-effort record + recurrence fold. A missing/old fno, a rejected payload,
# lock contention, an unreadable journal, OR a hung CLI never changes the
# advisory and never makes this hook exit nonzero. The fno call is wall-clock
# bounded (with_timeout, 30s) so a slow fold cannot block SessionStart; a timeout
# surfaces as unrecorded. jq is a SessionStart dependency (the platform wrappers
# already require it); if it is somehow absent we cannot confirm the recording
# and surface unrecorded rather than guessing.
record_status=""
if command -v fno >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  raw=""
  raw_rc=0
  if command -v with_timeout >/dev/null 2>&1; then
    raw="$(printf '%s' "$obs" | with_timeout 30 fno agents workspace worktree overlap-record --stdin --since 28 2>/dev/null || printf '%s' "$obs" | with_timeout 30 fno workspace worktree overlap-record --stdin --since 28 2>/dev/null)"; raw_rc=$?
  else
    # No wall-clock bound available: never make an unbounded call that could
    # block SessionStart. Surface unrecorded; the advisory still prints.
    record_status="unrecorded"
  fi
  if [[ -z "$record_status" ]]; then
    if (( raw_rc == 124 )); then
      # The bound fired: treat as unrecorded, never block SessionStart.
      record_status="unrecorded"
    else
      recorded="$(printf '%s' "$raw" | jq -r 'if (.recorded|type)=="boolean" then .recorded else empty end' 2>/dev/null || true)"
      if [[ "$recorded" == "true" ]]; then
        fold_state="$(printf '%s' "$raw" | jq -r '.fold.state // empty' 2>/dev/null || true)"
        if [[ "$fold_state" != "complete" && "$fold_state" != "no_data" ]]; then
          # Recorded, but the recurrence fold could not be trusted.
          record_status="count-unavailable"
        else
          met="$(printf '%s' "$raw" | jq -r '.fold.recurrence_threshold_met // false' 2>/dev/null || true)"
          if [[ "$met" == "true" ]]; then
            distinct="$(printf '%s' "$raw" | jq -r '.fold.distinct_observations // 0' 2>/dev/null || echo 0)"
            thresh="$(printf '%s' "$raw" | jq -r '.fold.recurrence_threshold // 3' 2>/dev/null || echo 3)"
            peer_lines+=("- recurrence reached ${distinct}/${thresh} in 28 days; run \`fno agents workspace worktree overlaps --since 28\`. A Stage 3 worktree-write-lock design node is now warranted (not filed automatically).")
          fi
        fi
      else
        record_status="unrecorded"
      fi
    fi
  fi
else
  record_status="unrecorded"
fi

case "$record_status" in
  unrecorded) peer_lines+=('- [fno-overlap-unrecorded]') ;;
  count-unavailable) peer_lines+=('- [fno-overlap-count-unavailable]') ;;
esac
fi

# Stranded-worktree residual report. Recovery is automatic - the
# pr-watch tick leg pushes and files every STRANDED row unattended - so this
# prints only what automation refused to touch on its own: UNKNOWN rows
# (fail-open, never acted on) and ABANDONED rows (deletion is the one
# destructive act and stays with a human; `fno agents workspace worktree cleanup --merged`
# turns the confirm into a one-liner instead of an investigation). Silent
# when both sets are empty. A timeout - or any other reason the sweep did
# not complete - says so rather than staying quiet, so an incomplete sweep
# never reads as a clean one.
#
# A full sweep is a `git fetch --all --prune` plus a rev-list and a
# last-commit read per worktree - measured at ~100s on this checkout's 68
# worktrees, an order of magnitude over any bound an interactive SessionStart
# hook can afford. So this NEVER runs the sweep inline: it reads whatever a
# PRIOR sweep already cached, and kicks a background refresh (detached, never
# awaited) when that cache is missing or stale. The first session after a
# fresh checkout sees nothing to report yet - the same as a clean sweep, by
# design - rather than block on the one sweep that would tell it otherwise.
CACHE_MAX_AGE_S=900
# Generous over the ~100s measured sweep cost above: a claimed window with
# still no cache file past this grace period means the last background
# sweep hung, crashed, or was killed - not that it is still running.
_SWEEP_GRACE_S=300
# Where the two files below live. `$SCRIPT_DIR/../.fno` is right for a real
# session: the cache is per-checkout. It is wrong under a test runner, where
# executing this hook writes the checkout's own state root, which is live state
# on a developer box and a hand-built `<root>/.fno/<file>` path either way.
#
# `FNO_TEST_HERMETIC=1` declares a sandboxed process root, and the pytest
# conftest chain stamps it with `HOME` inside the sandbox, so honouring it here
# redirects every test caller at once. Three test files execute this hook
# through `session-start.sh`; fixing the hook covers them and the next one.
# `0` is ambient on purpose and keeps the checkout path.
_STRANDED_DIR="$SCRIPT_DIR/../.fno"
if [[ "${FNO_TEST_HERMETIC:-}" == "1" ]]; then
  # Through the emitted stub, never a bare `$HOME/.fno`:
  # scripts/ci/check-no-hardcoded-paths.sh bars that spelling in hooks/, and it
  # caught this exact line.
  #
  # Read in a SUBSHELL, so only STATE_DIR crosses back. The stub exports
  # STATE_DIR, LATCHES_DIR, GRAPH_JSON_PATH, WORKTREES_BASE and CONFIG_FILE,
  # and its own header says it is config-blind. Sourcing it here put those
  # built-in defaults into the environment of every child this hook spawns,
  # and other hooks do read them: context-nudge.sh reads ${STATE_DIR} and
  # ${LATCHES_DIR} straight from the environment.
  _PATHS_STUB="$SCRIPT_DIR/../scripts/lib/paths.sh"
  _STRANDED_DIR=""
  if [[ -f "$_PATHS_STUB" ]]; then
    _STRANDED_DIR="$(
      REPO_ROOT="${REPO_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
      # shellcheck source=/dev/null
      source "$_PATHS_STUB" >/dev/null 2>&1
      printf '%s' "${STATE_DIR:-}"
    )"
  fi
  if [[ -z "$_STRANDED_DIR" ]]; then
    # NEVER fall back to the checkout path. That is the write this branch
    # exists to prevent, and a harness that copies only hooks/ into a sandbox
    # would silently reintroduce it while believing it was hermetic. A temp
    # dir loses the cache between runs, which costs a sweep and nothing else.
    #
    # Keyed by the sandbox HOME, not by uid alone. A uid-only path is stable
    # across every repository and every run for one user, so its cache and its
    # 15-minute refresh stamp outlive the run that wrote them: one run then
    # shows another repository's stranded rows, or suppresses its own refresh
    # against a stamp it never wrote.
    #
    # The WHOLE path, not its leaf. Sandbox roots are named by their purpose
    # under a per-run parent, so leaves repeat across runs while the parent
    # does not: two concurrent runs both holding a HOME that ends `/home`
    # would share one key again, which is the collision this key exists to
    # end. Substitution, not a subprocess, because this is a hot hook path.
    _STRANDED_KEY="${HOME//\//_}"
    _STRANDED_DIR="${TMPDIR:-/tmp}/fno-stranded-$(id -u)-${_STRANDED_KEY:-nohome}"
    echo "worktree-peers: $_PATHS_STUB is missing under FNO_TEST_HERMETIC=1; using $_STRANDED_DIR rather than the checkout state root" >&2
  fi
  mkdir -p "$_STRANDED_DIR" 2>/dev/null || true
fi
_CACHE_FILE="$_STRANDED_DIR/.worktree-stranded-cache.json"
# A dedicated stamp, not the cache file's own mtime: the window must be
# claimed (stamp touched) BEFORE the background sweep launches, the same
# up-front-claim pattern reconcile-throttle.sh uses, so a second SessionStart
# hook firing in the same instant sees a fresh stamp and skips instead of
# also kicking its own ~100s sweep. Touching the CACHE FILE itself to claim
# would truncate the stale-but-valid JSON a concurrent session's read (lines
# below) might be mid-parse of.
_STAMP_FILE="$_STRANDED_DIR/.worktree-stranded-refresh-stamp"
# _reconcile_mtime, not a third hand-rolled `stat -f || stat -c`: GNU `stat -f`
# means --file-system, not a format flag, so it SUCCEEDS on Linux and prints
# garbage instead of failing - the `||` fallback to `stat -c %Y` never fires,
# and the caller's arithmetic dies "unbound variable" under `set -u`. This
# exact crash already happened and was fixed twice in this repo (see
# scripts/lib/reconcile-throttle.sh); reuse that fix rather than reintroduce it.
_RT_HELPER="$SCRIPT_DIR/../scripts/lib/reconcile-throttle.sh"
[[ -f "$_RT_HELPER" ]] && source "$_RT_HELPER"
_cache_age() {
  if ! command -v _reconcile_mtime >/dev/null 2>&1; then
    echo 0  # no trustworthy mtime read: disable refresh, keep cached reporting
    return
  fi
  if [[ ! -f "$_STAMP_FILE" ]]; then
    echo 999999  # never claimed: maximally stale, start the first refresh
    return
  fi
  echo $(( $(date +%s) - $(_reconcile_mtime "$_STAMP_FILE") ))
}

stranded_lines=()
if command -v fno >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  if [[ -f "$_CACHE_FILE" ]]; then
    while IFS=$'\t' read -r klass node path; do
      [[ -n "$klass" ]] || continue
      case "$klass" in
        UNKNOWN)
          stranded_lines+=("- ${node:-?} at ${path}: could not classify (UNKNOWN, never acted on). [fno-stranded-unknown]")
          ;;
        ABANDONED)
          stranded_lines+=("- ${node:-?} at ${path}: abandoned work, confirm with \`fno agents workspace worktree cleanup --merged\`. [fno-stranded-abandoned]")
          ;;
      esac
    done < <(jq -r '.rows[]? | select(.class=="UNKNOWN" or .class=="ABANDONED") | [.class, (.node // "?"), .path] | @tsv' "$_CACHE_FILE" 2>/dev/null)
  elif [[ -f "$_STAMP_FILE" ]] && (( $(_cache_age) > _SWEEP_GRACE_S )); then
    stranded_lines+=('- previous stranded sweep did not complete (timed out, crashed, or was killed). [fno-stranded-sweep-incomplete]')
  fi

  if (( $(_cache_age) > CACHE_MAX_AGE_S )); then
    mkdir -p "$(dirname "$_STAMP_FILE")" 2>/dev/null
    : > "$_STAMP_FILE" 2>/dev/null || touch "$_STAMP_FILE" 2>/dev/null || true
    (
      mkdir -p "$(dirname "$_CACHE_FILE")" 2>/dev/null
      tmp="$(mktemp "${_CACHE_FILE}.XXXXXX" 2>/dev/null)" || exit 0
      if fno agents workspace worktree stranded --json > "$tmp" 2>/dev/null || fno workspace worktree stranded --json > "$tmp" 2>/dev/null; then
        mv -f "$tmp" "$_CACHE_FILE"
      else
        rm -f "$tmp"
      fi
    ) >/dev/null 2>&1 &
    disown 2>/dev/null || true
  fi
else
  stranded_lines+=('- stranded sweep skipped: fno or jq unavailable. [fno-stranded-sweep-incomplete]')
fi

# The `${arr[@]+"${arr[@]}"}` form, not a bare `"${arr[@]}"`: bash 3.2 (the
# macOS system bash this hook runs under) treats element-expansion of an
# EMPTY array as an unbound variable under `set -u`, and peer_lines is
# routinely empty when there is no live peer.
lines=("${peer_lines[@]+"${peer_lines[@]}"}" "${stranded_lines[@]+"${stranded_lines[@]}"}")
(( ${#lines[@]} > 0 )) || exit 0

if (( BODY_ONLY )); then
  printf '%s\n' "${lines[@]}"
else
  printf '## Worktree hygiene\n'
  printf '%s\n' "${lines[@]}"
fi
exit 0
