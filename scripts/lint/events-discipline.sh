#!/usr/bin/env bash
# scripts/lint/events-discipline.sh
#
# Three rules to keep the events.jsonl substrate honest:
#
#   bypass-append:      direct shell `>> .../.fno/events.jsonl` outside the
#                       common event helper
#   soft-outside-hooks: `--soft` flag in cli/ or skills/ (allowed under hooks/)
#   unwrapped-set-gate: `bash .../set-gate.sh ...` not wrapped in
#                       `if !`/`||`/`&&` and not under `set -e` at file scope
#   gate-lane-journal:  a hand-built <root>/.fno/events.jsonl under
#                       cli/src/fno/pr/ or cli/src/fno/review/
#
# Exit codes:
#   0  clean
#   1  one or more violations (each printed to stderr with file:line and
#      a one-line remediation)
#   2  substrate failure (missing dependency, e.g. git not on PATH)
#
# Bash 3.2 compatible: no associative arrays, no process substitution
# for sourcing, no `mapfile` (use plain `while IFS= read`).

set -uo pipefail

if ! REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "lint script unavailable: missing dependency: git" >&2
    exit 2
fi

cd "$REPO_ROOT" || {
    echo "lint script unavailable: cannot cd to repo root" >&2
    exit 2
}

violations=0
remediation() { printf '  remediation: %s\n' "$1" >&2; }

# Rule 1: bypass-append
# Match any executable shell line that appends directly to a literal
# events.jsonl path. The common helper appends through an events_path variable,
# so every literal append is a reachable bypass regardless of echo/printf/cat.
#
# Exclusions:
#   - scripts/lint/events-discipline.sh (this lint itself; the diagnostic
#     string contains the same pattern so a substring match would self-flag)
#   - tests/ (lint fixtures + test-bash-validator harnesses)
while IFS= read -r hit; do
    [[ -z "$hit" ]] && continue
    echo "events bypass at $hit: direct shell append to .fno/events.jsonl" >&2
    remediation "use scripts/lib/events.sh::emit_event_raw or _append_bounded_event instead"
    violations=$((violations + 1))
done < <(
    grep -rEn '^[[:space:]]*[^#].*>>[[:space:]]*.*([/"]events\.jsonl|\$\{?EVENTS(_LOG|_FILE)?\}?)' \
        --include='*.sh' --include='*.bash' \
        cli/ skills/ scripts/ hooks/ 2>/dev/null \
        | grep -v 'scripts/lint/events-discipline.sh' \
        | grep -v '/tests/' \
        || true
)

# Rule 2: soft-outside-hooks
# Match `--soft` as a whitespace-bounded flag.
while IFS= read -r hit; do
    [[ -z "$hit" ]] && continue
    echo "--soft flag forbidden outside hooks/ at $hit" >&2
    remediation "move the call to a hook, or remove --soft to use strict-default validation"
    violations=$((violations + 1))
done < <(
    grep -rEn '(^|[[:space:]])--soft($|[[:space:]])' \
        --include='*.sh' --include='*.bash' --include='*.py' \
        cli/ skills/ 2>/dev/null \
        | grep -v '/tests/' \
        || true
)

# Rule 3: unwrapped-set-gate
# Heuristic: any `bash .../set-gate.sh ...` line that is NOT
#   (a) inside a file with `set -e` near the top, OR
#   (b) part of an `if`, `&&`, or `||` construct on the same line.
# We deliberately scan only .sh / .bash files; .md skill bodies render
# guidance but never execute the embedded snippets directly.
while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    file="${line%%:*}"
    rest="${line#*:}"
    lineno="${rest%%:*}"
    code="${rest#*:}"
    # Skip files that are themselves the set-gate implementation OR the
    # lint script (which embeds the path string in its rule definitions).
    case "$file" in
        scripts/lib/set-gate.sh|skills/target/scripts/lib/set-gate.sh|scripts/lint/events-discipline.sh)
            continue
            ;;
    esac
    # Strip leading whitespace from the code excerpt for predicate checks.
    trimmed="${code#"${code%%[![:space:]]*}"}"
    # Skip comment lines: usage/example callouts inside scripts.
    if [[ "$trimmed" == \#* ]]; then
        continue
    fi
    # Strict-mode check: file enables `set -e` (or `set -euo`...) in the
    # first 10 lines.
    if head -n 10 "$file" 2>/dev/null | grep -qE '^[[:space:]]*set[[:space:]]+-[a-z]*e'; then
        continue
    fi
    # Wrapped in a control construct on the same line?
    if [[ "$trimmed" == "if "* || "$code" == *"||"* || "$code" == *"&&"* ]]; then
        continue
    fi
    echo "unwrapped set-gate.sh call at $file:$lineno" >&2
    remediation "wrap with 'if ! fno gate set ...; then exit 1; fi' or add 'set -e' near the top"
    violations=$((violations + 1))
done < <(
    grep -rEn 'bash[[:space:]]+[^[:space:]]*scripts/lib/set-gate\.sh' \
        --include='*.sh' --include='*.bash' \
        cli/ skills/ scripts/ hooks/ 2>/dev/null \
        | grep -v '/tests/' \
        || true
)

# Rule 4: gate-lane-journal
# The merge-gate lane must resolve its event journal through fno.paths
# (project_log / project_events_json): a hand-built <root>/.fno/events.jsonl
# here is what read a migrated-away file and reported covered PRs unreviewed.
# Scoped to these two directories on purpose: the remaining production sites
# are a per-lane paydown, and their live count is named below so the debt is
# read rather than inferred.
HAND_BUILT_JOURNAL='\.fno["'"'"' /,()]{0,10}events\.jsonl'
while IFS= read -r hit; do
    [[ -z "$hit" ]] && continue
    echo "hand-built project journal at $hit" >&2
    remediation "resolve through fno.paths project_log(\"events.jsonl\", project_root=...) or project_events_json() instead"
    violations=$((violations + 1))
done < <(
    grep -rEn "$HAND_BUILT_JOURNAL" \
        --include='*.py' \
        cli/src/fno/pr cli/src/fno/review 2>/dev/null \
        || true
)

# The debt outside the gate lane, same pattern: real hand-built sites (hooks
# today) that stay legal until their own lane sweeps them.
remaining_hand_built=$(
    grep -rEn "$HAND_BUILT_JOURNAL" \
        --include='*.py' --include='*.rs' --include='*.sh' \
        cli/src crates hooks scripts 2>/dev/null \
        | grep -v '/tests/' \
        | grep -vE '^cli/src/fno/(pr|review)/' \
        | grep -v 'scripts/lint/events-discipline.sh' \
        | grep -c . || true
)
printf '  note: %s hand-built journal site(s) or references remain outside the gate lane; sweep them lane by lane\n' "$remaining_hand_built" >&2

if [[ $violations -gt 0 ]]; then
    echo "events-discipline: $violations violation(s) found" >&2
    exit 1
fi

exit 0
