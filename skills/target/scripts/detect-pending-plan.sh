#!/usr/bin/env bash
# detect-pending-plan.sh - /target-side detection, precedence, body extraction,
# and atomic consume of the Plan Mode sidecar (.fno/.pending-plan.md).
# Self-contained (skill script); the only external dependency is `fno` (for the
# race-guarding claim, optional). See skills/target/references/plan-mode-backfill.md.
#
# Subcommands:
#   detect [--arg "<explicit /target argument>"] [--sidecar PATH]
#       Resolve what /target should do. Prints key=value lines:
#         result=none|pending|superseded_by_arg|malformed|expired
#         slug=<slug>           (pending / superseded_by_arg)
#         age_seconds=<n>       (pending)
#         age_human=<Nm|Nh>     (pending)
#         sidecar=<path>        (pending / superseded_by_arg)
#         reason=<why>          (malformed)
#       A malformed or expired sidecar is logged to hook-events.jsonl and
#       treated as absent (never fatal, never blocks an explicit argument).
#
#   body <out-file> [--sidecar PATH]
#       Write the sidecar's native plan body (frontmatter stripped) VERBATIM to
#       <out-file>, for use as backfill-plan.sh skeleton input.
#
#   consume [--sidecar PATH] [--holder ID]
#       Atomically flip status: pending -> consumed so two racing /target runs
#       collapse to a single execution. Serialized by a local atomic mkdir lock
#       (race-safe even without fno) plus an `fno claim` for cross-session
#       coordination; the claim is released once the flip lands. Call ONLY after
#       the confirm-yes. Exit 0 = consumed by us; exit 3 = already consumed or
#       being consumed by another run (caller must NOT proceed as the owner).

set -uo pipefail

_TTL_DEFAULT=14400

_state_dir() {
  # Resolve .fno relative to the repo root (worktree-safe).
  local root
  root="$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
  echo "$root/.fno"
}

_log_event() {
  local sd="$1" ev="$2" reason="${3:-}"
  mkdir -p "$sd" 2>/dev/null || true
  printf '{"event":"%s","reason":"%s","ts":"%s"}\n' \
    "$ev" "$reason" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    >> "$sd/hook-events.jsonl" 2>/dev/null || true
}

# Read a scalar frontmatter field from the sidecar. Strips a trailing CR so a
# CRLF sidecar (Windows / core.autocrlf) doesn't leave "pending\r" failing the
# exact string comparisons below.
_fm() {
  local file="$1" key="$2"
  grep -m1 "^${key}:" "$file" 2>/dev/null | sed -e "s/^${key}:[[:space:]]*//" -e 's/\r$//'
}

_mtime() {
  local f="$1"
  # GNU-first: `stat -c` fails on BSD/macOS (no -c flag) and falls through to
  # `-f %m`, correct on every BSD incl. FreeBSD. BSD-first would be WRONG -
  # GNU's `stat -f` is --file-system and SUCCEEDS with garbage, so its fallback
  # never runs (see reference_stat_f_m_gnu_filesystem_trap).
  stat -c "%Y" "$f" 2>/dev/null || stat -f "%m" "$f" 2>/dev/null || echo 0
}

_human_age() {
  local s="$1"
  if   [[ "$s" -lt 60 ]];   then echo "${s}s"
  elif [[ "$s" -lt 3600 ]]; then echo "$(( s / 60 ))m"
  else echo "$(( s / 3600 ))h"; fi
}

_resolve_sidecar() {
  local explicit="$1"
  if [[ -n "$explicit" ]]; then echo "$explicit"; else echo "$(_state_dir)/.pending-plan.md"; fi
}

cmd_detect() {
  local arg="" sidecar=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --arg) arg="$2"; shift 2 ;;
      --sidecar) sidecar="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  sidecar="$(_resolve_sidecar "$sidecar")"
  local sd; sd="$(dirname "$sidecar")"

  if [[ ! -f "$sidecar" ]]; then
    echo "result=none"
    return 0
  fi

  # Integrity: required fields + correct source/status.
  local src status slug captured
  src="$(_fm "$sidecar" source)"
  status="$(_fm "$sidecar" status)"
  slug="$(_fm "$sidecar" slug)"
  captured="$(_fm "$sidecar" captured_at)"
  if [[ "$src" != "claude-plan-mode" || -z "$status" || -z "$slug" || -z "$captured" ]]; then
    _log_event "$sd" "plan_mode_sidecar_malformed" "missing_or_wrong_fields"
    echo "result=malformed"
    echo "reason=missing_or_wrong_fields"
    return 0
  fi
  # Only a pending sidecar is offerable; a consumed one is inert.
  if [[ "$status" != "pending" ]]; then
    echo "result=none"
    return 0
  fi

  # TTL (defensive; init's session-start wipe is the primary guard).
  local ttl age
  ttl="${PENDING_PLAN_TTL_SECONDS:-$_TTL_DEFAULT}"
  age=$(( $(date -u +%s) - $(_mtime "$sidecar") ))
  if [[ "$age" -gt "$ttl" ]]; then
    _log_event "$sd" "plan_mode_sidecar_expired" "age_${age}s_over_ttl_${ttl}s"
    echo "result=expired"
    return 0
  fi

  # Precedence: an explicit /target argument always wins (sidecar stays pending).
  if [[ -n "${arg//[[:space:]]/}" ]]; then
    echo "result=superseded_by_arg"
    echo "slug=$slug"
    echo "sidecar=$sidecar"
    return 0
  fi

  echo "result=pending"
  echo "slug=$slug"
  echo "age_seconds=$age"
  echo "age_human=$(_human_age "$age")"
  echo "sidecar=$sidecar"
}

cmd_body() {
  local out="" sidecar=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --sidecar) sidecar="$2"; shift 2 ;;
      *) if [[ -z "$out" ]]; then out="$1"; fi; shift ;;
    esac
  done
  [[ -n "$out" ]] || { echo "detect-pending-plan: usage: body <out-file>" >&2; exit 2; }
  sidecar="$(_resolve_sidecar "$sidecar")"
  [[ -r "$sidecar" ]] || { echo "detect-pending-plan: sidecar not readable: $sidecar" >&2; exit 2; }

  # Strip the leading frontmatter block; emit the body VERBATIM.
  # Body = everything after the 2nd '---'; drop the single separator blank line.
  awk 'c>=2{print} /^---$/{c++}' "$sidecar" | sed '1{/^$/d;}' > "$out"
  # A torn/malformed sidecar (fewer than two '---' separators) yields an empty
  # body and exit 0 - which would silently flow an empty plan into the backfill.
  # Guard it: a present sidecar with no extractable body is a hard error.
  if [[ ! -s "$out" ]]; then
    echo "detect-pending-plan: extracted an empty plan body from $sidecar (malformed/torn sidecar?)" >&2
    _log_event "$(dirname "$sidecar")" "plan_mode_body_empty" "malformed_or_torn_sidecar"
    rm -f "$out" 2>/dev/null || true
    exit 2
  fi
}

cmd_consume() {
  local sidecar="" holder=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --sidecar) sidecar="$2"; shift 2 ;;
      --holder)  holder="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  sidecar="$(_resolve_sidecar "$sidecar")"
  [[ -f "$sidecar" ]] || { echo "detect-pending-plan: no sidecar to consume" >&2; exit 3; }
  [[ -n "$holder" ]] || holder="target-session:$$"
  local sd; sd="$(dirname "$sidecar")"

  local slug; slug="$(_fm "$sidecar" slug)"
  local claim_key="pending-plan:${slug:-unknown}"

  # Race guard, layer 1: a LOCAL atomic critical section. `mkdir` is atomic on
  # POSIX, so this serializes two racing consumes on the same host even when
  # `fno` is absent (the read-status-then-flip below is otherwise a TOCTOU, not
  # a real CAS). The fno claim (layer 2) adds cross-session/host coordination.
  local lock="$sidecar.consume.lock"
  if ! mkdir "$lock" 2>/dev/null; then
    # Held by a concurrent consume. The section is sub-second, so a lock older
    # than 30s means the holder died mid-flip; steal it. Otherwise we lost the
    # race -> another run owns this plan; do NOT double-consume.
    local lock_age; lock_age=$(( $(date -u +%s) - $(_mtime "$lock") ))
    if [[ "$lock_age" -lt 30 ]]; then
      echo "detect-pending-plan: pending plan is being consumed by another run" >&2
      exit 3
    fi
    rmdir "$lock" 2>/dev/null || true
    mkdir "$lock" 2>/dev/null || { echo "detect-pending-plan: could not acquire consume lock" >&2; exit 3; }
  fi
  trap 'rmdir "$lock" 2>/dev/null || true' EXIT

  # Race guard, layer 2: fno claim (cross-session). Non-fatal if fno is
  # unavailable or errors transiently (the mkdir lock still protects locally);
  # only a definite held-by-other (rc 1) aborts.
  local claimed=""
  if command -v fno >/dev/null 2>&1; then
    if FNO_CLAIMS_ROOT="${FNO_CLAIMS_ROOT:-$HOME}" fno claim acquire "$claim_key" \
         --holder "$holder" --ttl 30m >/dev/null 2>&1; then
      claimed="yes"
    else
      local rc=$?
      if [[ "$rc" -eq 1 ]]; then
        echo "detect-pending-plan: pending plan already claimed by another run" >&2
        exit 3
      fi
      # transient/unknown: proceed under the mkdir lock; log for observability.
      _log_event "$sd" "plan_mode_claim_skipped" "fno_acquire_rc_${rc}"
    fi
  fi

  # Compare-and-set under the lock: only flip if still pending.
  local status; status="$(_fm "$sidecar" status)"
  if [[ "$status" != "pending" ]]; then
    [[ -n "$claimed" ]] && FNO_CLAIMS_ROOT="${FNO_CLAIMS_ROOT:-$HOME}" fno claim release "$claim_key" --holder "$holder" >/dev/null 2>&1 || true
    echo "detect-pending-plan: sidecar no longer pending (status=$status); not consuming" >&2
    exit 3
  fi

  local tmp="$sidecar.consume.$$"
  # Flip the first 'status: pending' (the frontmatter one) to consumed.
  awk '!done && /^status: pending[[:space:]]*$/ {print "status: consumed"; done=1; next} {print}' \
    "$sidecar" > "$tmp" && mv -f "$tmp" "$sidecar" || {
      rm -f "$tmp" 2>/dev/null || true
      echo "detect-pending-plan: failed to mark sidecar consumed" >&2
      exit 3
    }

  # Release the claim now: the section it protected (the flip) is done, and the
  # sidecar is now `consumed` so detect returns none to other runs. Holding the
  # 30m-TTL claim would falsely block a later same-slug re-approval.
  [[ -n "$claimed" ]] && FNO_CLAIMS_ROOT="${FNO_CLAIMS_ROOT:-$HOME}" fno claim release "$claim_key" --holder "$holder" >/dev/null 2>&1 || true

  echo "consumed slug=$slug"
  exit 0
}

main() {
  local sub="${1:-}"
  [[ -n "$sub" ]] || { echo "usage: detect-pending-plan.sh <detect|body|consume> ..." >&2; exit 2; }
  shift
  case "$sub" in
    detect)  cmd_detect "$@" ;;
    body)    cmd_body "$@" ;;
    consume) cmd_consume "$@" ;;
    *) echo "detect-pending-plan: unknown subcommand: $sub" >&2; exit 2 ;;
  esac
}

main "$@"
