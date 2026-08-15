#!/usr/bin/env bash
# Emit the code-review attestation when a native /code-review pass reports
# CLEAN (x-e97b). Wired as a PostToolUse hook on the ReportFindings tool,
# which is the channel the harness review verb reports through: an empty
# findings array is "no finding survived verification", the same clean-pass
# signal sigma's Step 6c acts on inside its own skill. Without this producer
# a clean self-review still read uncovered and every code PR needed a human
# to name emit-attestation.sh by hand.
#
# Fail direction: any parse problem, a non-ReportFindings tool, or a
# NON-empty findings array emits nothing, so the reviewers gate holds rather
# than clearing on evidence that never arrived. A review that found bugs
# emits nothing here on purpose: the fixes move HEAD, and the freshness
# protocol would kill a premature attestation anyway. Re-run the review on
# the new HEAD; this hook fires on that pass's clean report.
set -euo pipefail

input="$(cat)"
tool_name="$(printf '%s' "$input" | jq -r '.tool_name // empty' 2>/dev/null || true)"
[[ "$tool_name" == "ReportFindings" ]] || exit 0

# A clean pass is the findings key PRESENT and an EMPTY array. Absent is not
# clean (assert a positive marker, never an absence): a payload without the
# key says nothing about the review outcome and must not attest.
findings="$(printf '%s' "$input" | jq -c '.tool_input.findings? // "absent"' 2>/dev/null || true)"
[[ "$findings" == "[]" ]] || exit 0

# Emit from the SESSION cwd, not the plugin root: a worktree session pins its
# own HEAD, and the event log is per-checkout.
cwd="$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null || true)"
[[ -n "$cwd" ]] || exit 0
cd "$cwd"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bash "$script_dir/../skills/review/scripts/emit-attestation.sh" code-review
