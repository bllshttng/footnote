#!/usr/bin/env bash
# check-file-budget.sh - a tracked source file over 5,000 lines is shrink-only.
#
# The biggest modules are the ones every worker edits, so a sub-100-line PR
# conflicts there by default, and nothing in CI notices growth. This is a
# ratchet, not a ceiling: there is no per-file number to raise. The merge base
# is the baseline, so a file over budget may only get smaller relative to it,
# and every shrink is banked.
#
# The refusal is the doc. It names the file, the delta, the budget, and the
# remedy: new code lands in a new module named by the QUESTION it answers
# (never server2.rs), and the code the change touched moves with it. A gate
# that only says "too big" produces server2.rs.
#
# Operator law (subject file-budget): a refusal is answered by REFACTORING in
# the same PR, never by raising a budget and never by splitting the PR. Both
# of those move the number and leave the bloat. Attack what the growth is made
# of: bloaters, object-oriented abusers, change preventers, dispensables
# (comments, duplicate code, dead code) and excessive couplers.
#
# New test files born over budget WARN instead of failing, for one reason: a
# pure test-motion change moves test blocks out of oversized production files
# into files of their own, and refusing those would refuse the very move that
# shrinks the production files. An EXISTING over-budget test file is under the
# same rule as production code - it may not grow either.
#
# The Python tree under cli/src/fno is shrink-only as a TREE, net: Python is
# the compatibility shell, Rust is the product. Net growth above the allowance
# is refused, so a bug fix never has to port a verb to land, while a feature -
# larger than the allowance - does. The remedy is to port the verb to crates/,
# land the feature in Rust, or refactor the growth away: per-harness DATA
# belongs in the capability contract, long prose belongs in docs/, and
# duplicate blocks belong behind one loop.
#
# Run: bash scripts/ci/check-file-budget.sh [--quiet]
# Exit: 0 pass, 1 a refused grow (a grown over-budget file, a new over-budget
#       production file, or the Python tree over allowance), 2 misuse or an
#       unresolvable base ref - a gate that cannot find its base never
#       reports a pass.
#
# Env (all optional):
#   FILE_BUDGET_LINES  per-file line budget. Default 5000.
#   PY_TREE_ALLOWANCE  net lines cli/src/fno Python may grow per change.
#                      Default 100.
#   PR_BASE_REF        base branch name, no remote prefix. Default: main.
#   PR_REMOTE          remote holding the base. Default: origin.
#   FILE_BUDGET_BASE_SHA  explicit base sha to diff instead of the merge base;
#                      the push-to-main alarm (guards.yml passes
#                      github.event.before). The all-zeros sha counts as unset
#                      (a branch's first push has no previous tip); anything
#                      else that does not resolve exits 2 - never a silent
#                      fall back to the merge base, which on main IS HEAD and
#                      would diff nothing.

set -euo pipefail

QUIET=0
if [[ $# -gt 0 ]]; then
    case "$1" in
        -h|--help) sed -n '2,/^set -/{/^set -/q;s/^# \{0,1\}//p;}' "$0"; exit 0 ;;
        --quiet) QUIET=1 ;;
        *)
            echo "check-file-budget: unknown arg: $1" >&2
            echo "       this check is configured by env only; see --help" >&2
            exit 2 ;;
    esac
fi

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

BUDGET="${FILE_BUDGET_LINES:-5000}"
PY_ALLOWANCE="${PY_TREE_ALLOWANCE:-100}"
# An env override is caller configuration, so garbage there is refused loudly -
# under set -e a non-numeric value would otherwise kill the arithmetic test with
# no output at all.
case "$BUDGET" in '' | *[!0-9]*)
    echo "check-file-budget: FILE_BUDGET_LINES must be a number, got '$BUDGET'" >&2
    exit 2 ;;
esac
case "$PY_ALLOWANCE" in '' | *[!0-9]*)
    echo "check-file-budget: PY_TREE_ALLOWANCE must be a number, got '$PY_ALLOWANCE'" >&2
    exit 2 ;;
esac
REMOTE="${PR_REMOTE:-origin}"
BASE_REF="${PR_BASE_REF:-main}"

# Base resolution follows check-proto-version-bump.sh: an EXPLICIT refspec, so
# a narrowed fetch config cannot leave a stale ref reading as current, and a
# refusal (never a silent pass against a missing ref) when the base cannot be
# established. The count printed by every refusal is computed live so the
# message reports progress without a second script.
PUSH_ALARM=0
BASE_SHA="${FILE_BUDGET_BASE_SHA:-}"
if [[ "$BASE_SHA" == "0000000000000000000000000000000000000000" ]]; then
    BASE_SHA=""   # a branch's first push: no previous tip exists to diff
fi

# A shallow checkout (actions/checkout's default depth is 1) holds neither the
# previous tip nor full history, so base lookups fail with no output. Heal by
# fetching full history once; local clones and deep CI checkouts never pay for
# this. The caller retries its own lookup afterwards and refuses if it still
# will not resolve - never a silent pass. Always succeeds: set -e runs this
# mid-list, and the retry, not the return code, is the verdict.
heal_shallow() {
    [[ "$(git rev-parse --is-shallow-repository 2>/dev/null)" == "true" ]] || return 0
    git fetch --quiet --unshallow "$REMOTE" 2>/dev/null || true
    return 0
}

if [[ -n "$BASE_SHA" ]]; then
    # The push-to-main alarm (guards.yml passes github.event.before). An
    # explicit sha never falls back to the merge base: on main the merge base
    # IS HEAD, and that silent empty diff is the green-on-nothing trap this
    # override exists to close.
    BASE="$(git rev-parse --verify --quiet "$BASE_SHA^{commit}")" || {
        heal_shallow
        BASE="$(git rev-parse --verify --quiet "$BASE_SHA^{commit}")" || true
    }
    if [[ -z "${BASE:-}" ]]; then
        echo "check-file-budget: FILE_BUDGET_BASE_SHA $BASE_SHA does not resolve" >&2
        echo "       (the checkout must hold that commit; unset FILE_BUDGET_BASE_SHA to use the merge base)" >&2
        exit 2
    fi
    PUSH_ALARM=1
else
    if ! git fetch --quiet "$REMOTE" \
            "+refs/heads/$BASE_REF:refs/remotes/$REMOTE/$BASE_REF"; then
        echo "check-file-budget: cannot fetch $REMOTE/$BASE_REF - unable to establish the merge base" >&2
        echo "       (fetch it, or set PR_BASE_REF/PR_REMOTE)" >&2
        exit 2
    fi
    BASE_TIP="$(git rev-parse --verify --quiet "$REMOTE/$BASE_REF")" || {
        echo "check-file-budget: cannot resolve $REMOTE/$BASE_REF - unable to establish the merge base" >&2
        echo "       (fetch it, or set PR_BASE_REF/PR_REMOTE)" >&2
        exit 2
    }
    BASE="$(git merge-base "$BASE_TIP" HEAD 2>/dev/null)" || {
        heal_shallow
        BASE="$(git merge-base "$BASE_TIP" HEAD 2>/dev/null)" || true
        if [[ -z "${BASE:-}" ]]; then
            echo "check-file-budget: cannot establish a merge base between $REMOTE/$BASE_REF and HEAD" >&2
            echo "       (shallow or unrelated histories? fetch --unshallow, or fix PR_BASE_REF/PR_REMOTE)" >&2
            exit 2
        fi
    }
fi

_CACHED_COUNT=""
live_count() {
    if [[ -z "$_CACHED_COUNT" ]]; then
        # -z / xargs -0: a quoted or spaced path must count, never split. The
        # diff below reads with core.quotepath=off for the same reason - a
        # changed file that arrives quoted would fail cat-file and silently
        # escape the gate.
        _CACHED_COUNT="$(git -c core.quotepath=off ls-files -z '*.rs' '*.py' '*.sh' '*.ts' '*.tsx' \
            | xargs -0 wc -l | awk -v b="$BUDGET" '$1 > b && $2 != "total"' \
            | wc -l | tr -d ' ')"
    fi
    echo "$_CACHED_COUNT"
}

is_test_path() {
    case "$1" in
        *_tests.rs | */tests/* | test_*.py | *_test.py) return 0 ;;
        *) return 1 ;;
    esac
}

fails=0
py_added=0
py_deleted=0
findings="$(mktemp)"
trap 'rm -f "$findings"' EXIT

while IFS=$'\t' read -r added deleted path; do
    [[ "$added" == "-" ]] && continue            # binary row
    case "$path" in *"=>"*) path="${path#*=> }" ;; esac
    [[ -z "$path" ]] && continue
    git cat-file -e "HEAD:$path" 2>/dev/null || continue   # deleted at HEAD

    if [[ "$path" == cli/src/fno/*.py ]] && ! is_test_path "$path"; then
        py_added=$((py_added + added))
        py_deleted=$((py_deleted + deleted))
    fi

    head_lines="$(git cat-file -p "HEAD:$path" | wc -l | tr -d ' ')"
    [[ "$head_lines" -gt "$BUDGET" ]] || continue   # under budget grows freely

    if git cat-file -e "$BASE:$path" 2>/dev/null; then
        if [[ "$added" -gt "$deleted" ]]; then
            if [[ "$PUSH_ALARM" -eq 1 ]]; then
                # A red push run is an alarm, not a refusal: the merge already
                # happened. The shrink number is the measured net growth, so
                # the next author gets the exact size of the owed payback.
                net=$((added - deleted))
                echo "check-file-budget: main advanced $(git rev-parse --short "$BASE")..$(git rev-parse --short HEAD) and grew $path by +$added/-$deleted" >> "$findings"
                echo "  ($head_lines lines, budget $BUDGET). This landed without the gate running on its PR head." >> "$findings"
                echo "  The next change touching this file must shrink it by at least $net lines." >> "$findings"
            else
                echo "check-file-budget: $path is $head_lines lines (budget $BUDGET) and this change grows it by +$added/-$deleted. A file over budget may only shrink. Put the new code in a module named by the question it answers (never server2.rs), and move the code you touched with it. Then refactor the rest away here: duplicate code, dead code, comment bloat, and anything a data file or a doc should hold. Splitting the PR is not a remedy. Files over budget today: $(live_count); each shrink is banked." >> "$findings"
            fi
            fails=1
        elif [[ "$QUIET" -eq 0 ]]; then
            net=$((deleted - added))
            if [[ "$net" -eq 0 ]]; then
                echo "check-file-budget: ok $path $head_lines lines, change +$added/-$deleted (net 0); no grow"
            else
                echo "check-file-budget: ok $path $head_lines lines, change +$added/-$deleted (net -$net); shrink banked"
            fi
        fi
    elif is_test_path "$path"; then
        echo "check-file-budget: WARN $path is a new test file at $head_lines lines (budget $BUDGET). Tests are warned, not refused, for one reason: a pure test-motion change lands test blocks moved out of oversized production files, and refusing them would refuse the move that shrinks those files. New tests still belong in a module named by the question they answer." >&2
    else
        echo "check-file-budget: $path is a new file at $head_lines lines (budget $BUDGET). A production file is not born over budget. Put the new code in a module named by the question it answers (never server2.rs), and move the code you touched with it. Then refactor the rest away here: duplicate code, dead code, comment bloat, and anything a data file or a doc should hold. Splitting the PR is not a remedy. Files over budget today: $(live_count); each shrink is banked." >> "$findings"
        fails=1
    fi
# Two-dot, deliberately: the diff must measure from BASE exactly. On the
# merge-base path BASE is an ancestor of HEAD, so BASE..HEAD and BASE...HEAD
# are identical there; on the explicit-sha path a sha that is not an ancestor
# (a force-push overwrite) must still be honored as pinned, which three-dot
# would silently widen to the merge base.
done < <(git -c core.quotepath=off diff --numstat -M "$BASE"..HEAD -- '*.rs' '*.py' '*.sh' '*.ts' '*.tsx')

py_net=$((py_added - py_deleted))
if [[ "$py_net" -gt "$PY_ALLOWANCE" ]]; then
    echo "check-file-budget: cli/src/fno grew by +$py_added/-$py_deleted net +$py_net (allowance $PY_ALLOWANCE). Python is the compatibility shell; port the verb you touched to crates/ or land the feature in Rust. Or refactor the growth away in THIS PR: move data to the capability contract or another data file, move the long prose to docs/, cut duplicate and dead code, and extract or compose what is left. Raising $PY_ALLOWANCE and splitting the PR are both refused: they move the number and leave the bloat." >> "$findings"
    fails=1
fi

if [[ -s "$findings" ]]; then
    cat "$findings" >&2
fi
if [[ "$fails" -eq 1 ]]; then
    exit 1
fi
if [[ "$QUIET" -eq 0 ]]; then
    echo "check-file-budget: ok (no over-budget file grew; cli/src/fno net +$py_net, allowance $PY_ALLOWANCE)"
fi
exit 0
