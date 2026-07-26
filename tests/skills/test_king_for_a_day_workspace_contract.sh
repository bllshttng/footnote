#!/usr/bin/env bash
# tests/skills/test_king_for_a_day_workspace_contract.sh
#
# Documentation contract for king-for-a-day pane placement.
#
# `fno agents spawn` exposes `--workspace`/`-s` as canonical and keeps
# `--squad` only as a hidden deprecated alias (cli/src/fno/agents/cli.py).
# The alias still works, so a stale example stays green under manual testing
# and the docs drift back silently. These assertions are the guard:
#
#   1. No live command example on a king surface spells `--squad`.
#   2. Exactly one compatibility note names it, in the public spawn guide.
#   3. Placement flags never appear on a bg/headless example - the CLI
#      rejects them there, so such an example is a command that cannot run.
#
# Auto-discovered by `fno test smoke` via the tests/skills/*.sh glob.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT" || exit 1

KING_SURFACES=(
    skills/king-for-a-day/SKILL.md
    skills/king-for-a-day/references/court-operations.md
    skills/king-for-a-day/references/minion-clause.md
)
SPAWN_GUIDE=docs/guides/fno-agents-spawn.md
ALL_SURFACES=("${KING_SURFACES[@]}" "$SPAWN_GUIDE")

fail=0
note() { echo "FAIL: $*"; fail=1; }

for f in "${ALL_SURFACES[@]}"; do
    [[ -r "$f" ]] || note "$f missing or unreadable"
done
[[ $fail -eq 0 ]] || exit 1

# A "live example" is any line that invokes a command. Prose may name the
# flag; a runnable line may not.
# The `:` is allowed before `fno` because these lines arrive from `grep -n`,
# which prefixes each with `<lineno>:`.
is_command_line() { grep -qE '(^|[[:space:]`:])fno[[:space:]]' <<<"$1"; }

# --- 1. No --squad in any king-surface command example -----------------------
for f in "${KING_SURFACES[@]}"; do
    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        note "$f teaches deprecated --squad: $line"
    done < <(grep -n -- '--squad' "$f" || true)
done

# --- 2. Exactly one compatibility note, in the spawn guide -------------------
# No mapfile here: it is bash 4+, and stock macOS ships bash 3.2.
squad_hits=$(grep -c -- '--squad' "$SPAWN_GUIDE")
if [[ "$squad_hits" -ne 1 ]]; then
    note "$SPAWN_GUIDE must name --squad exactly once (found $squad_hits)"
else
    only=$(grep -- '--squad' "$SPAWN_GUIDE")
    if is_command_line "$only"; then
        note "the --squad mention must be prose, not a command: $only"
    fi
    grep -qi 'deprecat' <<<"$only" ||
        note "the --squad mention must mark it deprecated: $only"
fi

# --- 3. Canonical spelling is actually taught --------------------------------
for f in skills/king-for-a-day/SKILL.md \
         skills/king-for-a-day/references/court-operations.md \
         skills/king-for-a-day/references/minion-clause.md \
         "$SPAWN_GUIDE"; do
    grep -q -- '--workspace' "$f" ||
        note "$f teaches no --workspace placement example"
done

# --- 4. Placement flags never ride a bg/headless example ---------------------
# The CLI refuses --workspace/-s/--split/--at outside --substrate pane, so
# such a command documents an invocation that exits nonzero.
#
# This folds backslash continuations into one logical command first: the
# guides split real commands across lines, and a per-line check never sees
# `--substrate bg` and `--workspace` together. It also catches the positional
# substrate form (`fno agents spawn "..." bg ...`), which carries no
# `--substrate` flag at all.
join_continuations() {
    awk '
      { line = $0
        if (cont) { buf = buf " " line } else { start = NR; buf = line }
        if (line ~ /\\[ \t]*$/) { cont = 1; sub(/\\[ \t]*$/, "", buf); next }
        cont = 0; print start ":" buf; buf = "" }
      END { if (buf != "") print start ":" buf }
    ' "$1"
}

NONPANE_RE='--substrate[[:space:]]+(bg|headless)|--headless|(^|[[:space:]])(--once|-o|-p)([[:space:]]|$)'
POSITIONAL_RE='fno[[:space:]]+agents[[:space:]]+spawn[[:space:]].*[[:space:]](bg|headless)([[:space:]]|$)'
PLACEMENT_RE='--workspace|(^|[[:space:]])-s[[:space:]]|--split|(^|[[:space:]])-x[[:space:]]|--at[[:space:]]'

for f in "${ALL_SURFACES[@]}"; do
    while IFS= read -r cmd; do
        [[ -n "$cmd" ]] || continue
        is_command_line "$cmd" || continue
        grep -qE -- "$NONPANE_RE" <<<"$cmd" ||
            grep -qE -- "$POSITIONAL_RE" <<<"$cmd" || continue
        grep -qE -- "$PLACEMENT_RE" <<<"$cmd" || continue
        note "$f puts placement flags on a non-pane substrate: $cmd"
    done < <(join_continuations "$f")
done

if [[ $fail -eq 0 ]]; then
    echo "PASS: king-for-a-day workspace contract"
    exit 0
fi
exit 1
