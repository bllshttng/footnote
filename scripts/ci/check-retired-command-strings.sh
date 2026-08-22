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
#   2. The occurrence must be in RUNNABLE form - an argument placeholder, a
#      concrete short id, or the command left dangling at end of line by a
#      wrapped string. A reader can copy "claude rm <id>" and run it;
#      "claude rm failed to start" names the same command and cannot be run.
#      This is mechanical and needs no reading of intent. Comment-only lines
#      are out of scope by construction.
#   3. The author declares the verdict. A surviving hit fails unless it
#      carries a `retired-ok:` marker on its own line or the line above,
#      saying why that string does not tell its reader to run the command.
#      Three kinds of string earn one: a failure message naming the shellout
#      that failed, an API docstring naming what the function wraps, and a
#      prohibition that names the command in order to forbid it.
#
# Narrowing 2 has a known ceiling: a bare-form instruction with no argument
# at all ("run claude rm") passes. Widening it to catch that means separating
# instruction from description by reading the sentence, and no regex does
# that - the attempt produces the noise that kills a gate. Upgrade path: if a
# bare-form instruction ever ships, add its imperative cue to the pattern,
# not a classifier.
#
# THREE controls, all load-bearing, because an absence-only pass has two
# explanations ("clean" and "the instrument never matched anything"):
#
#   tool     the pattern must match an in-script canary invocation.
#   surface  each surface must resolve to tracked files of its own. A glob
#            pathspec that matches nothing fails SILENTLY, and a whole-scan
#            control cannot see it while another surface still returns hits.
#            An earlier draft scanned `crates/*/src/**/*.rs`, matched zero
#            files, and reported a clean Rust tree that held the sharpest
#            specimen in the repo.
#   target   the canary FIXTURE's marked line must be seen and cleared. It
#            asserts on the fixture and not on production text, so a real
#            cleanup can take the tree to zero marked sites without trapping
#            the gate red.
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

# Runnable form, three shapes:
#   1. an argument placeholder - `claude rm <id>`, `{row}`, `$job`
#   2. a concrete short id - `claude rm 7c5dcf5d`, which is strictly MORE
#      copyable than a placeholder and was missed by an earlier draft
#   3. the command at end of line, which is how a wrapped format string
#      hides one: `f"Orphan supervisor: claude rm "` on one line and
#      `f"{short_id} to clean later."` on the next shipped a live
#      instruction straight past a line-scoped scan
ARG='([[:space:]]+[<{$]|[[:space:]]+[0-9a-f]{8}|[[:space:]]*"[[:space:]]*$)'
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
# A surface is a directory plus an extension, never a `**` pathspec (see the
# surface control above). Everywhere a caller reads at the moment of acting.
# `docs` is not optional:
# the operator guide is the most caller-facing surface there is, and leaving
# it out would exempt the text most likely to be followed by hand.
SURFACES=(
    "cli/src:py"
    "crates:rs"
    "skills:md"
    "docs:md"
    "scripts:sh"
    "hooks:sh"
    "agents:md"
    "commands:md"
)

RAW=""
for surface in "${SURFACES[@]}"; do
    dir="${surface%%:*}"; ext="${surface##*:}"
    # `grep -c`, never `grep -q`: -q exits on the first match and SIGPIPEs
    # `git ls-files`, which under `set -o pipefail` makes the pipeline status
    # 141 as soon as the file list outgrows the pipe buffer. That read as
    # "this surface holds no files" and failed the gate on the real tree
    # (cli/src emits ~17 KB, over the 16 KiB macOS buffer) while every
    # synthetic-tree test stayed green. -c consumes all input.
    tracked="$(git ls-files -- "$dir" | grep -cE "\.${ext}\$" || true)"
    [ "${tracked:-0}" -gt 0 ] ||
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
# pattern already excludes almost every test assertion in the tree, because
# they name the command in bare form; the two that survive are the absence
# assertions in daemon.rs, which carry markers. Measured: the same Rust sites
# with the exclusion and without.
CANARY_FILE="scripts/ci/fixtures/retired-command-canary.sh"
BAD=""
MARKED=0
INSPECTED=0
CANARY_SEEN=0
while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    file="${hit%%:*}"; rest="${hit#*:}"; num="${rest%%:*}"; text="${rest#*:}"
    stripped="${text#"${text%%[![:space:]]*}"}"

    # Comment-only lines describe the implementation; they never reach a caller.
    case "$file" in
        *.rs) case "$stripped" in '//'*) continue ;; esac ;;
        *.py|*.sh) case "$stripped" in '#'*) continue ;; esac ;;
    esac

    INSPECTED=$((INSPECTED + 1))
    if [ "$num" -gt 1 ]; then
        prev="$(sed -n "$((num - 1))p" "$file")"
    else
        prev=""
    fi
    if [[ "$text" == *"$MARKER"* || "$prev" == *"$MARKER"* ]]; then
        MARKED=$((MARKED + 1))
        [ "$file" = "$CANARY_FILE" ] && CANARY_SEEN=1
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
# Reached only on the green path. The canary fixture carries one marked site;
# not seeing it cleared means the marker spelling drifted, so every declared
# site in the tree would read as a violation. Asserting on the FIXTURE and
# not on production text is deliberate: a real cleanup may legitimately take
# the tree to zero marked sites, and that must stay distinguishable from a
# broken marker.
[ "$CANARY_SEEN" -eq 1 ] ||
    fail "clean, but the ${MARKER} canary at ${CANARY_FILE} was never seen and cleared; the marker spelling or the fixture drifted, so this pass is vacuous"

echo "retired-command strings OK: inspected ${INSPECTED} runnable-form site(s), ${MARKED} declared descriptive, controls fired"
