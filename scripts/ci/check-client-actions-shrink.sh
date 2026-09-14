#!/usr/bin/env bash
# check-client-actions-shrink.sh - ALL_CLIENT_ACTIONS is shrink-only.
#
# Law d-fe66560a: top-level verbs are not allowed at all, hidden ones
# included. The fno-agents binary's action list may only shrink: a token
# removed is banked, a token added refuses - a swap refuses too, since it
# contains an addition. The gap that let a hidden verb in: the verb ratchet
# registers all of fno-agents as ONE baseline leaf, so nothing noticed the
# list growing. This gate closes that gap for the binary the way
# check-file-budget.sh closes it for file size.
#
# There is no exception label, because the law allows none. The remedy for a
# refused grow is named in the refusal: an argument of an existing action, or
# a field on an existing verb's output - never a new action.
#
# Run: bash scripts/ci/check-client-actions-shrink.sh [--quiet]
# Exit: 0 pass, 1 an added action, 2 misuse, a missing/moved const, or an
#       unresolvable base ref - a gate that cannot see its baseline never
#       reports a pass.
#
# Env (all optional):
#   PR_BASE_REF       base branch name, no remote prefix. Default: main.
#   PR_REMOTE         remote holding the base. Default: origin.
#   ACTIONS_BASE_SHA  explicit base sha to diff instead of the merge base;
#                      the push-to-main alarm (guards.yml passes
#                      github.event.before). The all-zeros sha counts as unset.
#                      Anything else that does not resolve exits 2 - never a
#                      silent fall back to the merge base, which on main IS
#                      HEAD and would diff nothing.

set -euo pipefail

QUIET=0
case "${1:-}" in
    "") ;;
    -h | --help) sed -n '2,/^set -/{/^set -/q;s/^# \{0,1\}//p;}' "$0"; exit 0 ;;
    --quiet) QUIET=1 ;;
    *)
        echo "check-client-actions-shrink: unknown arg: $1" >&2
        echo "       this check is configured by env only; see --help" >&2
        exit 2 ;;
esac

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

CONST_PATH="crates/fno-agents/src/bin/client.rs"
REMOTE="${PR_REMOTE:-origin}"
BASE_REF="${PR_BASE_REF:-main}"

# A shallow checkout (actions/checkout's default depth is 1) holds neither the
# previous tip nor full history; heal once and let the caller's lookup retry.
heal_shallow() {
    [[ "$(git rev-parse --is-shallow-repository 2>/dev/null)" == "true" ]] || return 0
    git fetch --quiet --unshallow "$REMOTE" 2>/dev/null || true
    return 0
}

BASE_SHA="${ACTIONS_BASE_SHA:-}"
if [[ "$BASE_SHA" == "0000000000000000000000000000000000000000" ]]; then
    BASE_SHA="" # a branch's first push: no previous tip exists to diff
fi
if [[ -n "$BASE_SHA" ]]; then
    BASE="$(git rev-parse --verify --quiet "$BASE_SHA^{commit}")" || {
        heal_shallow
        BASE="$(git rev-parse --verify --quiet "$BASE_SHA^{commit}")" || true
    }
    if [[ -z "${BASE:-}" ]]; then
        echo "check-client-actions-shrink: ACTIONS_BASE_SHA $BASE_SHA does not resolve" >&2
        echo "       (unset ACTIONS_BASE_SHA to use the merge base)" >&2
        exit 2
    fi
else
    BASE="$(git merge-base HEAD "$REMOTE/$BASE_REF" 2>/dev/null)" || {
        heal_shallow
        BASE="$(git merge-base HEAD "$REMOTE/$BASE_REF" 2>/dev/null)" || true
    }
    if [[ -z "${BASE:-}" ]]; then
        echo "check-client-actions-shrink: cannot resolve a base (HEAD against $REMOTE/$BASE_REF)" >&2
        echo "       fetch $REMOTE and retry; refusing to diff nothing" >&2
        exit 2
    fi
fi

# The tokens between `const ALL_CLIENT_ACTIONS` and its closing `];`, one per
# line. An empty output means the const is gone or renamed - refused below, so
# a moved list cannot silently bypass the gate.
tokens_from() {
    awk '
        /^const ALL_CLIENT_ACTIONS:/ { in_block = 1; next }
        in_block && /^\];/ { in_block = 0 }
        in_block {
            line = $0
            while (match(line, /"[^"]+"/)) {
                print substr(line, RSTART + 1, RLENGTH - 2)
                line = substr(line, RSTART + RLENGTH)
            }
        }
    '
}

BASE_TEXT="$(git show "$BASE:$CONST_PATH" 2>/dev/null)" || {
    echo "check-client-actions-shrink: base $BASE has no $CONST_PATH" >&2
    exit 2
}
HEAD_TEXT="$(cat "$CONST_PATH")"
if ! grep -q '^const ALL_CLIENT_ACTIONS:' <<<"$BASE_TEXT"; then
    echo "check-client-actions-shrink: base side lost const ALL_CLIENT_ACTIONS ($CONST_PATH at $BASE)" >&2
    exit 2
fi
if ! grep -q '^const ALL_CLIENT_ACTIONS:' <<<"$HEAD_TEXT"; then
    echo "check-client-actions-shrink: head side lost const ALL_CLIENT_ACTIONS ($CONST_PATH)" >&2
    exit 2
fi

base_tokens="$(mktemp)"
head_tokens="$(mktemp)"
trap 'rm -f "$base_tokens" "$head_tokens"' EXIT
tokens_from <<<"$BASE_TEXT" | sort >"$base_tokens"
tokens_from <<<"$HEAD_TEXT" | sort >"$head_tokens"

added="$(comm -13 "$base_tokens" "$head_tokens" | sort -u)"
banked="$(comm -23 "$base_tokens" "$head_tokens" | sort -u)"

for token in $banked; do
    [[ "$QUIET" == 1 ]] || echo "banked: $token"
done

if [[ -n "$added" ]]; then
    for token in $added; do
        echo "added: $token"
    done
    echo "check-client-actions-shrink: ALL_CLIENT_ACTIONS grew; law d-fe66560a allows no new action, hidden or advertised." >&2
    echo "       The remedy is an argument of an existing action, or a field on an existing verb's output - never a new action." >&2
    exit 1
fi

[[ "$QUIET" == 1 ]] || echo "check-client-actions-shrink: ok - the action list only shrank"
exit 0
