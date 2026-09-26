#!/usr/bin/env bash
# Compile the merge result of one rev pair and run the repo-wide static and
# preamble-budget steps on it. Specimen: 3334b826a133 merged two green parents into a red main -
# F821 Undefined name _TAG_SHUTDOWN at cli/src/fno/graph/store.py:525 - and
# the combination compiled on no machine that ran CI. The caller resolves the
# PR's base/head and the already-contains shortcut; this script owns the
# merge-tree, the extraction, and the static step.
#
# Usage: check-merge-result.sh <repo-toplevel> <base-rev> <head-oid>
# Exit: 0 the merge result is statically green, 3 red (the reason is on
#       stdout), 4 unknown (a step could not run).
# Env:  RUFF / MYPY override the tool commands (the caller points them at the
#       canonical cli's uv environment; the bare tools are the default).
set -euo pipefail
export NO_COLOR=1
repo="$1"; base="$2"; head="$3"
cd "$repo"

out="$(git merge-tree --write-tree "$base" "$head" 2>&1)" && mt_rc=0 || mt_rc=$?
if [ "$mt_rc" -ne 0 ]; then
    if printf '%s\n' "$out" | grep -q '^CONFLICT'; then
        paths="$(printf '%s\n' "$out" | grep '^CONFLICT' | sed 's/.*Merge conflict in //' | head -3 | paste -sd ', ' -)"
        echo "merge-result: red - git cannot merge cleanly: conflict in $paths"
        exit 3
    fi
    echo "merge-result: unknown - git merge-tree exit $mt_rc: $(printf '%s\n' "$out" | tail -1)" >&2
    exit 4
fi

tree="$(printf '%s\n' "$out" | head -1)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/fno-merge-result-XXXXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# The preamble budget must hold on main PLUS this PR, not on each PR's own
# base: two PRs that each pass their own guards can merge into a main over the
# byte ceiling. Run the MERGED tree's own copy, so the ceiling main will
# enforce after the merge is the ceiling measured now. Exit 3 maps to red in
# the caller, like the static step, and no GitHub call is made.
pre_line=""
if git cat-file -e "$tree:scripts/ci/check-preamble-budget.sh" 2>/dev/null; then
    pre_paths=""
    for p in AGENTS.md CLAUDE.md skills agents .claude/rules scripts/ci/check-preamble-budget.sh; do
        git cat-file -e "$tree:$p" 2>/dev/null && pre_paths="$pre_paths $p"
    done
    # shellcheck disable=SC2086  # the split is the point: only existing paths
    git archive --format=tar "$tree" $pre_paths | tar -x -C "$tmp"
    pre_out="$(bash "$tmp/scripts/ci/check-preamble-budget.sh" "$tmp" 2>&1)" && pre=0 || pre=$?
    if [ "$pre" -ne 0 ]; then
        hit="$(printf '%s\n' "$pre_out" | grep -E '^check-preamble-budget: .*-byte (ceiling|band)' | head -1 || true)"
        if [ -n "$hit" ]; then
            echo "merge-result: red - preamble: ${hit#'check-preamble-budget: '}"
            exit 3
        fi
        tail="$(printf '%s\n' "$pre_out" | grep -v '^$' | tail -1 || true)"
        echo "merge-result: unknown - preamble step exited $pre: ${tail:-no output}" >&2
        exit 4
    fi
    raw="$(printf '%s\n' "$pre_out" | grep -E '^check-preamble-budget: [0-9]+ / [0-9]+ bytes' | head -1 || true)"
    if [ -n "$raw" ]; then
        pre_line="preamble: $(printf '%s\n' "$raw" | sed -E 's/^check-preamble-budget: ([0-9]+ \/ [0-9]+) bytes.*/\1 B/')"
    fi
fi
prefix=""
[ -n "$pre_line" ] && prefix="$pre_line - "

if git cat-file -e "$tree:cli" 2>/dev/null; then
    git archive --format=tar "$tree" cli | tar -x -C "$tmp"
fi
if [ ! -d "$tmp/cli" ]; then
    echo "merge-result: ok - ${prefix}merge tree carries no cli/ - nothing for the static step to check"
    exit 0
fi
static_out="$(bash "$(cd "$(dirname "$0")" && pwd)/check-python-static.sh" "$tmp/cli" 2>&1)" && st=0 || st=$?
if [ "$st" -eq 0 ]; then
    ok_line="$(printf '%s\n' "$static_out" | grep '^python-static:' | tail -1 || true)"
    echo "merge-result: ok - ${prefix}${ok_line:-static step passed}"
    exit 0
fi
red="$(printf '%s\n' "$static_out" | grep -E '^src/' | sed 's|^src/|cli/src/|' | head -3 | paste -sd '; ' - || true)"
if [ -n "$red" ]; then
    echo "merge-result: red - $red"
    exit 3
fi
# A crashed tool (mypy internal error, uv build failure) verified nothing: a
# tail of crash prose is never a red.
tail="$(printf '%s\n' "$static_out" | grep -v '^$' | tail -3 | paste -sd '; ' - || true)"
echo "merge-result: static step exited $st without src/-prefixed errors: ${tail:-no output}" >&2
exit 4
