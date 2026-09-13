#!/usr/bin/env bash
# assert-absent.sh - refuse to report a zero as a verdict without proof the
# instrument works. A zero-hit search has three explanations: the real
# outcome, an instrument that never ran, or a pipeline loss. Two controls
# close the gap, and both run through the SAME argv as the probe, so the
# control always validates the tool that produces the answer - never a
# different grep, never a different scope:
#   1. the control must hit in that scope: a control that matches nothing
#      means the instrument is broken and the zero is unreadable
#   2. the caller must supply a control at all: an absence with no control
#      is not a verdict
# The control must be independently confirmed present in the scanned scope,
# and written in the probe's own syntax (e.g. a `/blueprint`-shaped control
# for a `/spec`-shaped probe): a plain-token control validates the tool,
# not the pattern.
# Specimen: `git grep -nE 'x-aaaa'` finds 307 lines while
# `git grep -nE '\bx-aaaa\b'` finds 0 and exits clean (POSIX ERE has no
# \b). One pattern, two answers; only a control in the same tool tells
# which answer is real. Measured 2026-09-12, Apple git 2.50.1.
#
# Usage:
#   assert-absent.sh --control <pattern> --probe <pattern> -- <tool argv with one {} placeholder>
# The argv is given once; {} is replaced with the control, then the probe.
# Exit 0: control hit, probe zero; prints
#   assert-absent: absent probe=<p> control=<c> control_hits=<n>
# Exit 1: probe hit; prints the probe's hit lines (filter for allowlists)
# Exit 2: refused; reason on stderr, prefix `assert-absent: refused:`

set -uo pipefail

refuse() {
    printf 'assert-absent: refused: %s\n' "$1" >&2
    exit 2
}

CONTROL=""
PROBE=""
ARGV=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --control)
            [[ $# -ge 2 ]] || refuse "--control needs a value"
            CONTROL="$2"
            shift 2
            ;;
        --probe)
            [[ $# -ge 2 ]] || refuse "--probe needs a value"
            PROBE="$2"
            shift 2
            ;;
        --)
            shift
            ARGV=("$@")
            break
            ;;
        *)
            refuse "unknown argument: $1 (usage: --control <p> --probe <p> -- <tool argv with one {}>)"
            ;;
    esac
done

[[ -n "$CONTROL" ]] || refuse "--control is required: an absence with no control is not a verdict"
[[ -n "$PROBE" ]] || refuse "--probe is required"
[[ ${#ARGV[@]} -gt 0 ]] || refuse "tool argv required after --"

placeholders=0
for a in ${ARGV[@]+"${ARGV[@]}"}; do
    [[ "$a" == "{}" ]] && placeholders=$((placeholders + 1))
done
[[ $placeholders -eq 1 ]] || refuse "tool argv must hold exactly one {} placeholder (found $placeholders)"

# Run the argv once with {} swapped for the given token. The caller's cwd,
# env, and tool are inherited untouched, so the control and the probe can
# only ever run in the same tool and the same scope.
run_tool() {
    local token="$1" a
    local -a cmd=()
    for a in ${ARGV[@]+"${ARGV[@]}"}; do
        if [[ "$a" == "{}" ]]; then cmd+=("$token"); else cmd+=("$a"); fi
    done
    "${cmd[@]}"
}

count_lines() {
    printf '%s' "$1" | grep -c '^' || true
}

ctl_out=$(run_tool "$CONTROL")
ctl_rc=$?
[[ $ctl_rc -le 1 ]] || refuse "tool error: control run exited $ctl_rc"
ctl_hits=$(count_lines "$ctl_out")
[[ $ctl_hits -gt 0 ]] || \
    refuse "instrument broken: control $CONTROL matched nothing in this tool and scope, so the zero is unreadable"

probe_out=$(run_tool "$PROBE")
probe_rc=$?
[[ $probe_rc -le 1 ]] || refuse "tool error: probe run exited $probe_rc"
probe_hits=$(count_lines "$probe_out")
if [[ $probe_hits -eq 0 && $probe_rc -eq 0 ]]; then
    refuse "silent probe: tool exited 0 with no output; a success with no rows is indistinguishable from lost output, not a verdict"
fi
if [[ $probe_hits -gt 0 ]]; then
    printf '%s\n' "$probe_out"
    exit 1
fi

printf 'assert-absent: absent probe=%s control=%s control_hits=%s\n' "$PROBE" "$CONTROL" "$ctl_hits"
exit 0
