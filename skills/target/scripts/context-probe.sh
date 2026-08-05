#!/usr/bin/env bash
# context-probe.sh - shim over the CLI's single context-window implementation.
#
# Usage: context-probe.sh <transcript-jsonl-path>
#
# Output (exit 0): one JSON line, identical field-for-field to `fno context --json`:
#   {"used_tokens": N, "window_tokens": N, "used_pct": N, "model": "..."}
#
# Exit 3 ("unreadable") on ANY failure, including `fno` being absent from PATH.
# Every caller treats nonzero as "no pressure" (fail-safe), so requiring `fno`
# degrades toward silence rather than a false handoff; both hook callers already
# shell out to `fno` for other reasons.
#
# This stays a shim rather than being deleted because
# skills/target/scripts/handoff.sh resolves it as "$_SCRIPT_DIR/context-probe.sh"
# and that resolution is what skill self-containment requires of a bundled skill.
# The token math and the model->window allowlist live ONCE, in the CLI
# (cli/src/fno/context_probe.py); a second copy is the drift this indirection
# exists to prevent. The untouched tests/test-context-probe.sh suite drives the
# real path and is the port's regression proof.
_EXIT_UNREADABLE=3

if [ $# -lt 1 ] || [ -z "$1" ]; then
  exit "$_EXIT_UNREADABLE"
fi

command -v fno >/dev/null 2>&1 || exit "$_EXIT_UNREADABLE"

# `fno context` exits 3 on unreadable; normalize ANY nonzero (incl. a transient
# fno error) to exit 3 so the shim's failure contract is identical to the old
# shell probe's. stdout passes through verbatim on success.
_out="$(fno context --transcript "$1" --json 2>/dev/null)"
_rc=$?
if [ "$_rc" -ne 0 ]; then
  exit "$_EXIT_UNREADABLE"
fi
printf '%s\n' "$_out"
