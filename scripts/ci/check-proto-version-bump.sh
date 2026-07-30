#!/usr/bin/env bash
# scripts/ci/check-proto-version-bump.sh
#
# PROTO_VERSION monotonic-bump guard.
#
# The collision class: two branches both bump PROTO_VERSION N -> N+1. Both make
# the IDENTICAL one-line change, so git merges them without a conflict. The
# second to merge lands its own wire change with no effective bump, and the
# handshake in proto.rs then accepts a client that cannot decode the new shape.
# This happened repeatedly across v16-v20.
#
# The rule: if a PR's own diff touches the PROTO_VERSION line, the resulting
# value must be strictly greater than the value at the CURRENT base tip. The
# base TIP, not the merge-base, is the whole point -- the merge-base still holds
# the pre-collision value and comparing against it passes the exact case this
# guard exists for.
#
# READ THE PR HEAD, NOT THE CHECKOUT.
# `actions/checkout` on a pull_request event checks out the MERGE ref, whose
# content already contains the base branch. When both sides bumped to the same
# value the merged file simply reads that value, so the PR's own change to the
# line has already been absorbed and a diff against the base shows nothing.
# The intent is only visible in the PR's own head commit, which is why
# PR_HEAD_SHA is wired in from the event payload rather than read from HEAD.
#
# Env (all optional; CI wires the first two from the pull_request payload):
#   PR_HEAD_SHA  the PR's own head commit. Default: HEAD (correct locally).
#   PR_BASE_REF  base branch name, no remote prefix. Default: main.
#   PR_REMOTE    remote holding the base. Default: origin.
#
# Exit codes:
#   0  no bump required, or the bump is monotonic
#   1  bump required but not monotonic, or the version could not be determined
#   2  the base ref is unreachable (fail closed: cannot verify)
set -uo pipefail

PROTO_FILE="crates/fno/src/proto.rs"
REMOTE="${PR_REMOTE:-origin}"
BASE_REF="${PR_BASE_REF:-main}"
HEAD_SHA="${PR_HEAD_SHA:-HEAD}"

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "ERROR: not inside a git repository" >&2
    exit 2
}
cd "$REPO_ROOT" || exit 2

# ---------------------------------------------------------------------------
# Extract the const value from a blob, failing loud on anything unparseable.
#
# An unreadable version must never be treated as "no bump needed": that is a
# silent pass on the exact defect class, so every failure here exits non-zero
# with the raw line it could not parse.
# ---------------------------------------------------------------------------
read_version() {
    local rev="$1" label="$2" blob line
    if ! blob="$(git show "$rev:$PROTO_FILE" 2>/dev/null)"; then
        echo "ERROR: cannot read $PROTO_FILE at $label ($rev)" >&2
        return 1
    fi
    # Here-string, not a pipe: under `pipefail` a producer that takes a SIGPIPE
    # makes the pipeline non-zero even on a successful match. Harmless with a
    # bare `grep` today, since it reads to EOF, but the same shape one flag away
    # (`-q`, `-m1`) is a silent pass, so neither instance uses a pipe.
    line="$(grep -E '^pub const PROTO_VERSION' <<<"$blob" || true)"
    if [[ -z "$line" ]]; then
        echo "ERROR: no 'pub const PROTO_VERSION' line in $PROTO_FILE at $label" >&2
        return 1
    fi
    if [[ ! "$line" =~ =[[:space:]]*([0-9]+)[[:space:]]*\; ]]; then
        echo "ERROR: cannot parse a version out of $label line: $line" >&2
        return 1
    fi
    printf '%s\n' "${BASH_REMATCH[1]}"
}

# ---------------------------------------------------------------------------
# Resolve the base tip. Fail CLOSED when it is unreachable: a shallow clone that
# silently skipped the check would re-open the collision class.
# ---------------------------------------------------------------------------
if ! git rev-parse --verify --quiet "$REMOTE/$BASE_REF" >/dev/null; then
    git fetch --quiet --depth=1 "$REMOTE" "$BASE_REF" 2>/dev/null || true
fi
BASE_TIP="$(git rev-parse --verify --quiet "$REMOTE/$BASE_REF" \
    || git rev-parse --verify --quiet FETCH_HEAD)" || {
    echo "ERROR: cannot resolve $REMOTE/$BASE_REF - unable to verify the bump" >&2
    echo "       (fetch it, or set PR_BASE_REF/PR_REMOTE)" >&2
    exit 2
}

# The head sha travels from the event payload and may not be in a depth-1 clone.
if ! git rev-parse --verify --quiet "$HEAD_SHA^{commit}" >/dev/null; then
    git fetch --quiet "$REMOTE" "$HEAD_SHA" 2>/dev/null || true
fi
if ! git rev-parse --verify --quiet "$HEAD_SHA^{commit}" >/dev/null; then
    echo "ERROR: cannot resolve PR head $HEAD_SHA - unable to verify the bump" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# Did the PR's own commits touch the const line?
#
# Scanned PER COMMIT, not as one aggregate merge-base diff. An aggregate diff
# loses the edit as soon as the branch merges the updated base into itself (the
# ordinary "update my branch" move): merge-base advances to that base, both sides
# then read the same value, and the branch's own bump vanishes from the diff
# while its wire change is still in the PR. That is the collision this guard
# exists for, reported as "untouched".
#
# Merge commits contribute no patch here by default, which is exactly right:
# inheriting someone else's bump by pulling the base in is not this PR editing
# the line. Only a commit whose own patch touches it counts.
#
# Matching the const line rather than the file keeps an unrelated proto.rs edit
# (a doc comment, a new message type) from forcing a version bump.
# ---------------------------------------------------------------------------
MERGE_BASE="$(git merge-base "$BASE_TIP" "$HEAD_SHA" 2>/dev/null)" || {
    echo "ERROR: no merge-base between $REMOTE/$BASE_REF and $HEAD_SHA" >&2
    exit 2
}

# Captured, then matched WITHOUT a pipe. Piping into `grep -q` lets grep exit at
# the first match while the producer is still writing, so the producer takes a
# SIGPIPE and `pipefail` turns the whole pipeline non-zero. A negated test then
# reads a successful match as "no match" and passes the guard on a real
# collision. It needs enough pending output to fill the pipe buffer, which a
# bump sitting on top of a large earlier proto.rs commit supplies.
PROTO_PATCH="$(git log -p --format='' "$MERGE_BASE..$HEAD_SHA" -- "$PROTO_FILE" 2>/dev/null)" || {
    echo "ERROR: cannot read this PR's $PROTO_FILE history - unable to verify the bump" >&2
    exit 2
}

if ! grep -qE '^[-+]pub const PROTO_VERSION' <<<"$PROTO_PATCH"; then
    echo "proto-version-bump: PROTO_VERSION line untouched by this PR; nothing to check"
    exit 0
fi

BASE_V="$(read_version "$BASE_TIP" "$REMOTE/$BASE_REF tip")" || exit 1
HEAD_V="$(read_version "$HEAD_SHA" "PR head")" || exit 1

if (( HEAD_V > BASE_V )); then
    echo "proto-version-bump: OK (v$BASE_V -> v$HEAD_V)"
    exit 0
fi

echo "ERROR: PROTO_VERSION bump is not monotonic against $REMOTE/$BASE_REF." >&2
echo "       $REMOTE/$BASE_REF tip: v$BASE_V" >&2
echo "       this PR:               v$HEAD_V" >&2
echo "" >&2
if (( HEAD_V == BASE_V )); then
    echo "       $REMOTE/$BASE_REF is already at v$BASE_V - a parallel branch bumped to the" >&2
    echo "       same value and merged first. Your identical one-line change merged" >&2
    echo "       cleanly, so this PR's wire change is landing with no effective bump." >&2
    echo "       Fix: re-bump to v$((BASE_V + 1)) in $PROTO_FILE (and any" >&2
    echo "       assert_eq!(PROTO_VERSION, ...) pinned to the old value)." >&2
else
    echo "       This PR lowers PROTO_VERSION. It must only ever increase." >&2
    echo "       Fix: set it above v$BASE_V in $PROTO_FILE." >&2
fi
exit 1
