#!/usr/bin/env bash
# scripts/ci/check-retired-command-strings.sh
#
# A ruling that lives only in a decision record loses to a string the agent
# reads at the moment of acting. The decision record fires when someone goes
# looking; a runtime string fires when someone is about to act. This gate
# carries a ruling to the second moment.
#
# It reads scripts/ci/retired-commands.txt and fails when a caller-facing
# string shows a retired command in RUNNABLE form.
#
# Three narrowings, applied in order. Only the third makes a judgment:
#
#   1. The command must be retired. The scan looks for registry members, not
#      for every command, so most of a naive sweep never enters the report.
#   2. The occurrence must be in RUNNABLE form - the command followed by an
#      argument token (<, {, or $). A reader can copy "claude rm <id>" and
#      run it; "claude rm failed to start" names the same command and cannot
#      be run. This is mechanical and needs no reading of intent. Comment-only
#      lines are out of scope by construction.
#   3. The author declares the verdict. A surviving hit fails unless it
#      carries a `retired-ok:` marker on its own line or the line above,
#      saying why that string does not tell its reader to run the command.
#      Three kinds of string earn one: a failure message naming the shellout
#      that failed, an API docstring naming what the function wraps, and a
#      prohibition that names the command in order to forbid it.
#
# Narrowing 2 has a known ceiling: a bare-form instruction ("run claude rm")
# passes. Widening it to catch that means separating instruction from
# description by reading the sentence, and no regex does that - the attempt
# produces the noise that kills a gate. The ceiling is accepted because a
# runnable string is the one a reader copies. Upgrade path: if a bare-form
# instruction ever ships, add its imperative cue to the pattern, not a
# classifier.
#
# Both controls are load-bearing. An absence-only pass has two explanations
# ("clean" and "the instrument never matched anything"), so the tool control
# proves the pattern matches a canary, and the target control proves the tree
# still contains at least one `retired-ok:` marker to find. A sweep that
# returns zero raw hits has a broken pattern or a drifted surface list and
# fails rather than passing vacuously.
#
# Exit 0 clean; 1 on a surviving hit, a malformed registry, or a control that
# did not fire.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="scripts/ci/retired-commands.txt"
MARKER='retired-ok:'

fail() {
    echo "check-retired-command-strings: $*" >&2
    exit 1
}

[ -f "$REGISTRY" ] || fail "registry not found at $REGISTRY"

# --- 1. Parse the registry -------------------------------------------------
# A malformed line exits non-zero naming the line number rather than being
# skipped: a silently dropped entry is a retired command the gate stops
# looking for, which is the failure this whole file exists to prevent.
declare -a CMDS INSTEAD RULING
lineno=0
while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    case "$line" in ''|'#'*) continue ;; esac
    IFS='|' read -r cmd instead ruling <<<"$line"
    [ -n "$cmd" ] && [ -n "$instead" ] && [ -n "$ruling" ] ||
        fail "$REGISTRY:$lineno: malformed entry, want <command>|<what to say instead>|<ruling id>"
    CMDS+=("$cmd"); INSTEAD+=("$instead"); RULING+=("$ruling")
done <"$REGISTRY"

[ "${#CMDS[@]}" -gt 0 ] || fail "$REGISTRY holds no entries; nothing to police"

# Runnable form: the command followed by an argument token.
ARG='[[:space:]]+[<{$]'
PATTERN=""
for cmd in "${CMDS[@]}"; do
    PATTERN="${PATTERN}${PATTERN:+|}${cmd}${ARG}"
done
PATTERN="(${PATTERN})"

# --- 2. Tool control -------------------------------------------------------
CANARY="run ${CMDS[0]} <short_id> to clean up"
printf '%s\n' "$CANARY" | grep -qE "$PATTERN" ||
    fail "pattern does not match the canary; every future sweep would pass vacuously"

# --- 3. Scan the caller-facing surfaces ------------------------------------
# Each surface is a directory plus an extension, never a `**` pathspec. A
# glob pathspec that resolves to nothing fails SILENTLY in git grep, and the
# whole-scan control cannot see it while any other surface still returns
# hits: an earlier draft of this gate scanned `crates/*/src/**/*.rs`, matched
# zero files, and reported a clean Rust tree that held the sharpest specimen.
# Hence the per-surface control below: each surface must resolve to tracked
# files of its own.
SURFACES=("cli/src:py" "crates:rs" "skills:md")

RAW=""
for surface in "${SURFACES[@]}"; do
    dir="${surface%%:*}"; ext="${surface##*:}"
    git ls-files -- "$dir" | grep -qE "\.${ext}\$" ||
        fail "surface ${dir} holds no tracked .${ext} files; the surface list drifted and its scan is vacuous"
    # git grep over the directory, then filter by extension here: the
    # extension test stays in this script rather than in a pathspec, where a
    # glob that matches nothing is indistinguishable from a clean surface.
    hits="$(git grep -nE "$PATTERN" -- "$dir" | grep -E "^[^:]+\.${ext}:" || true)"
    RAW="${RAW}${hits}${hits:+$'\n'}"
done

[ -n "$RAW" ] ||
    fail "zero raw hits across every surface; the pattern drifted"

# There is deliberately no test-module exclusion. It would have to find where
# a `#[cfg(test)]` module ENDS, and every cheap proxy is wrong: the first
# match in crates/fno/src/client.rs is an indented attribute on a function at
# line 1174, so "first match to end of file" blanks 14k lines of production
# code, and "column-0 match to end of file" still swallows the production
# code that follows a mid-file test module. Measured instead: the runnable-form
# pattern already excludes every test assertion in the tree, because they name
# the command in bare form. Same 7 Rust sites with the exclusion and without.
BAD=""
MARKED=0
INSPECTED=0
while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    file="${hit%%:*}"; rest="${hit#*:}"; num="${rest%%:*}"; text="${rest#*:}"
    stripped="${text#"${text%%[![:space:]]*}"}"

    # Comment-only lines describe the implementation; they never reach a caller.
    case "$file" in
        *.rs) case "$stripped" in '//'*) continue ;; esac ;;
        *.py) case "$stripped" in '#'*) continue ;; esac ;;
    esac

    INSPECTED=$((INSPECTED + 1))
    if [ "$num" -gt 1 ]; then
        prev="$(sed -n "$((num - 1))p" "$file")"
    else
        prev=""
    fi
    if [[ "$text" == *"$MARKER"* || "$prev" == *"$MARKER"* ]]; then
        MARKED=$((MARKED + 1))
        continue
    fi
    BAD="${BAD}${file}:${num}:${stripped}"$'\n'
done <<<"$RAW"

# --- 4. Report -------------------------------------------------------------
# The report comes before the target control on purpose. A drifted marker
# spelling turns every declared site into a violation, which is loud and
# names the sites; only a GREEN result can be vacuous, so that is where the
# control has to stand.
if [ -n "$BAD" ]; then
    echo "check-retired-command-strings: retired command(s) shown in runnable form:" >&2
    printf '%s' "$BAD" >&2
    echo "" >&2
    for idx in "${!CMDS[@]}"; do
        echo "  '${CMDS[$idx]}' was retired by ${RULING[$idx]}. Say instead: ${INSTEAD[$idx]}" >&2
    done
    echo "" >&2
    echo "Two ways out, and the second is a real option, not a formality:" >&2
    echo "  1. Rewrite the string so it names the replacement above." >&2
    echo "  2. Add '${MARKER} <why this does not tell its reader to run it>' on" >&2
    echo "     the line, or the line above it. A failure message naming the" >&2
    echo "     shellout that failed, a docstring naming what a function wraps," >&2
    echo "     and a prohibition that names the command to forbid it all earn one." >&2
    exit 1
fi

# --- 5. Target control -----------------------------------------------------
# Reached only on the green path. At least one marker must exist in the tree:
# zero means the marker spelling drifted and the scan is passing vacuously.
[ "$MARKED" -gt 0 ] ||
    fail "clean, but no ${MARKER} marker was found at any inspected site; the marker spelling drifted and this pass is vacuous"

echo "retired-command strings OK: inspected ${INSPECTED} runnable-form site(s), ${MARKED} declared descriptive, controls fired"
