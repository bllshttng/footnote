#!/usr/bin/env bash
# confirm-decision.sh - deterministic confirm-posture decision for /fno:agent (spawn verb).
#
# The agents SKILL derives whether to show the billed-launch confirm prompt
# from normalize.sh's emitted fields plus the config knob `config.agents.confirm`.
# This helper centralizes that decision so the policy is deterministic and
# testable rather than LLM-prose-only (ab-27541df5, Claude's Discretion 3). It
# reads only config (read-only) and has no side effects.
#
# Inputs (flags; all optional, sensible defaults):
#   --node <id>            backlog node id (empty = free-form feature)
#   --provider <p>         claude | codex | gemini      (default claude)
#   --mode <m>             exec | interactive           (default exec)
#   --payload-mode <m>     build | seed | passthrough | handoff (default build)
#   --yolo <0|1>           --yolo in effect             (default 0)
#   --permission-mode <m>  effective harness permission mode (default empty).
#                          bypassPermissions is a gate bypass -> caveat, so the
#                          warning survives even when --yolo was mapped to it for
#                          claude (normalize clears YOLO in that case, x-d235).
#   --allow-merge <0|1>    -m/--allow-merge in effect   (default 0)
#   --yes <0|1>            -y/--yes in effect           (default 0)
#
# spawn is a FREE lane (ab-994222ee): a worker lands a PR for review, nothing is
# billed or destructive, so it does NOT confirm by default. config.agents.confirm
# is repurposed from "confirm spawn (default on)" to an opt-in "confirm even the
# free lanes": `always` confirms; `auto` (default) and `never` skip. chat (billed
# plan credit) and stop (destructive) keep their own always-confirm gates and do
# not route through this helper.
#
# Emits key=value lines on stdout (read line by line; never `eval`):
#   posture=<always|auto|never>   the effective posture (auto when degraded)
#   confirm_required=<0|1>        1 = show the confirm before spawning
#   caveat=<0|1>                  an exec-stall / yolo / merge caveat applies
#   caveat_text=<...>             human caveat string (empty when caveat=0)
#   warn=<...>                    degrade or launched-with-caveat warning (else empty)
#   reason=<...>                  one-line rationale for the decision
#
# Config read (overridable for tests via DISPATCH_CONFIRM_READER, a command
# that prints the posture or exits non-zero): a failed or invalid read degrades
# to the no-confirm default (`auto`) with a staleness hint - the free lane has
# nothing to gate, so a stale config never forces the double-confirm US2 removes.

set -uo pipefail

NODE=""
PROVIDER="claude"
MODE="exec"
PAYLOAD_MODE="build"
YOLO=0
PERMISSION_MODE=""
ALLOW_MERGE=0
YES=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --node)         NODE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --provider)     PROVIDER="${2:-claude}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --mode)         MODE="${2:-exec}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --payload-mode) PAYLOAD_MODE="${2:-build}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --yolo)         YOLO="${2:-0}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --permission-mode) PERMISSION_MODE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --allow-merge)  ALLOW_MERGE="${2:-0}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --yes)          YES="${2:-0}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    *) printf 'posture=always\nconfirm_required=1\ncaveat=0\ncaveat_text=\nwarn=unknown argument: %s\nreason=bad-invocation\n' "$1"; exit 0 ;;
  esac
done

# ---- resolve posture (bounded read; degrade toward safety) -------------------
default_reader() {
  # The read is local-file-only today, but a bounded call costs nothing. macOS
  # ships no `timeout`; fall back to gtimeout, then a bare call (never let a
  # missing timeout binary force a degrade and disable the knob entirely).
  if command -v timeout >/dev/null 2>&1; then
    timeout 5 fno config get config.agents.confirm
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout 5 fno config get config.agents.confirm
  else
    fno config get config.agents.confirm
  fi
}

DEGRADED=0
if [[ -n "${DISPATCH_CONFIRM_READER:-}" ]]; then
  # Intentionally word-split: the seam accepts a multi-word reader command
  # (e.g. "gtimeout 5 fno config get ...") the same way default_reader runs one.
  raw="$($DISPATCH_CONFIRM_READER 2>/dev/null)"; rc=$?
else
  raw="$(default_reader 2>/dev/null)"; rc=$?
fi
raw="$(printf '%s' "$raw" | tr -d '[:space:]')"

case "$raw" in
  always|auto|never)
    if [[ "$rc" -eq 0 ]]; then
      posture="$raw"
    else
      # A non-zero rc with a valid-looking value (e.g. a timeout that still
      # echoed a default) is a failed read. spawn is a FREE lane (ab-994222ee):
      # there is nothing billed or destructive to gate, so degrade toward the
      # no-confirm default (auto), NOT toward a confirm - the whole point of the
      # free lane is that a phone "do it" is never re-asked. Surface the warn.
      posture="auto"; DEGRADED=1
    fi
    ;;
  *) posture="auto"; DEGRADED=1 ;;
esac

WARN=""
[[ "$DEGRADED" -eq 1 ]] && WARN="config.agents.confirm unreadable or invalid (stale fno? run 'fno update'); the free lane does not confirm regardless"

# ---- compute caveat (exec-stall / yolo / merge grant) ------------------------
# Exec-stall: a codex/gemini exec build has nobody to answer a clarifying
# question (codex auto-rejects and continues; gemini aborts the run). Caveats
# always confirm under `auto`, even with an explicit provider (Locked Decision 3).
CAVEAT=0
CAVEAT_TEXT=""
add_caveat() {
  CAVEAT=1
  if [[ -n "$CAVEAT_TEXT" ]]; then
    CAVEAT_TEXT="$CAVEAT_TEXT; $1"
  else
    CAVEAT_TEXT="$1"
  fi
}

if [[ "$PAYLOAD_MODE" == "build" && "$MODE" == "exec" ]]; then
  case "$PROVIDER" in
    codex|gemini) add_caveat "codex/gemini exec build: no one to answer a clarifying question (codex auto-rejects, gemini aborts). Pass -i to stage a drivable session, or --yolo to bypass the approval gate" ;;
  esac
fi
[[ "$YOLO" -eq 1 ]] && add_caveat "running with --yolo (sandbox/approval bypass)"
# bypassPermissions is the same gate-bypass risk class as yolo, and for claude it
# IS the yolo mapping (normalize clears YOLO after mapping, x-d235). Caveat on the
# effective mode so a bypass launch always surfaces the warning - whether it came
# from --yolo on claude or an explicit --permission-mode bypassPermissions.
[[ "$PERMISSION_MODE" == "bypassPermissions" ]] && add_caveat "running with --permission-mode bypassPermissions (permission-gate bypass)"

# ---- decide (free-lane posture, ab-994222ee) ---------------------------------
# spawn is a FREE, reversible lane: an autonomous worker lands a PR for REVIEW
# (no auto-merge), so nothing is billed or destructive to gate. It therefore
# does NOT confirm by default. The ONLY thing that re-introduces a confirm is the
# cautious-operator opt-in `config.agents.confirm: always` ("confirm even the
# free lanes"). `auto` (the default) and `never` both skip; a degraded read also
# skips (nothing to gate) but surfaces the staleness as a warning. Caveats (yolo
# / merge / exec-stall) NO LONGER force a confirm - they surface as warnings
# alongside the genuine receipt (the receipt-echo invariant). chat (billed) and
# stop (destructive) keep their own always-confirm gates; they do NOT route here.
if [[ "$YES" -eq 1 ]]; then
  CONFIRM=0; REASON="-y/--yes: accepted and ignored - the free lane already does not confirm"
elif [[ "$posture" == "always" ]]; then
  CONFIRM=1; REASON="config.agents.confirm=always: cautious opt-in confirms even the free lane"
else
  CONFIRM=0; REASON="free lane (spawn): nothing billed or destructive to gate -> no confirm"
fi

# A skip path that still carries a caveat must surface it (the receipt-echo
# invariant moves the transparency the confirm provided to the warning). Never
# when already warning about a degrade.
if [[ "$CONFIRM" -eq 0 && "$CAVEAT" -eq 1 && -z "$WARN" ]]; then
  WARN="launched without confirm but a caveat applies: $CAVEAT_TEXT"
fi

printf 'posture=%s\n' "$posture"
printf 'confirm_required=%s\n' "$CONFIRM"
printf 'caveat=%s\n' "$CAVEAT"
printf 'caveat_text=%s\n' "$CAVEAT_TEXT"
printf 'warn=%s\n' "$WARN"
printf 'reason=%s\n' "$REASON"
