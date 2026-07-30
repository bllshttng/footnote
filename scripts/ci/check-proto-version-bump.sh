#!/usr/bin/env bash
# scripts/ci/check-proto-version-bump.sh
#
# PROTO_VERSION monotonic-bump guard.
#
# The collision class: two branches both bump PROTO_VERSION N -> N+1. Both make
# the IDENTICAL one-line change, so git merges them without a conflict. The
# second to merge lands its own wire change with no effective bump, and the
# handshake in proto.rs then accepts a client that cannot decode the new shape.
#
# It has happened twice: two unrelated features each landed v17, and two each
# landed v31. Both were caught by hand, during a conflict resolution, after the
# fact. The recipe below also reports v9, which is NOT a collision - it is one
# change committed twice, and only one of the pair is an ancestor of main, so it
# only appears while that unmerged branch is still around. Verify with:
#   git log --all --format=%H -- crates/fno/src/proto.rs | while read c; do
#     git show $c -- crates/fno/src/proto.rs |
#       sed -n 's/^+pub const PROTO_VERSION[^=]*=[[:space:]]*\([0-9]\{1,\}\).*/\1/p'
#   done | sort -n | uniq -d
# Note the `[^=]*` there: the type in `u32` supplies a digit before the value, so
# a naive first-number match reports every commit as v32.
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
# THIS CHECK ALONE DOES NOT CLOSE THE CLASS.
# It reads the base at run time, and a `pull_request` run is not re-triggered
# when the base branch advances. So: PR B goes green against v10, PR A merges
# v11, PR B merges - collision, guard never re-ran. Closing that needs branch
# protection requiring branches to be up to date before merging, which forces a
# re-run against the advanced base. Without it this is a strong early warning
# rather than a gate.
#
# Exit codes:
#   0  no bump required, or the bump is monotonic
#   1  bump required but not monotonic, or the version could not be determined
#   2  the base ref is unreachable (fail closed: cannot verify)
set -uo pipefail

# Arg parsing exists only to REFUSE. Configuration is env-only, so an unknown
# flag means a caller believes it is steering something it is not - and silently
# ignoring `--base other-branch` while checking against main is precisely the
# kind of quiet misconfiguration this guard is written to prevent.
# `if`, not `while`: there is no shift here, so a loop would spin forever the
# moment someone adds a branch that does not exit.
if [[ $# -gt 0 ]]; then
    case "$1" in
        # Header block only: stop at `set -`, or --help runs on through every
        # column-0 implementation comment in the file and ends mid-rationale.
        -h|--help) sed -n '2,/^set -/{/^set -/q;s/^# \{0,1\}//p;}' "$0"; exit 0 ;;
        *)
            echo "ERROR: unknown arg: $1" >&2
            echo "       this check is configured by env only; see --help" >&2
            exit 2 ;;
    esac
fi

PROTO_FILE="crates/fno/src/proto.rs"
REMOTE="${PR_REMOTE:-origin}"
BASE_REF="${PR_BASE_REF:-main}"
# A set-but-EMPTY PR_HEAD_SHA must not quietly become HEAD under CI: HEAD is the
# merge ref there, which by design shows no const diff against the base, so that
# fallback is the blind mode this guard exists to avoid and it looks identical to
# a legitimate local run. `:-` cannot tell empty from unset, so the refusal below
# is what draws the line; outside CI, empty still means "use HEAD", which is the
# right local default.
if [[ -n "${CI:-}" && -z "${PR_HEAD_SHA:-}" ]]; then
    echo "ERROR: PR_HEAD_SHA is empty under CI - refusing to fall back to HEAD," >&2
    echo "       which is the merge ref here and cannot show the PR's own bump." >&2
    exit 2
fi
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
    if ! blob="$(git show "$rev:$PROTO_FILE")"; then
        echo "ERROR: cannot read $PROTO_FILE at $label ($rev)" >&2
        return 1
    fi
    # Here-string, not a pipe, for the reason spelled out at the match site below.
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
# Fetched UNCONDITIONALLY, and a FAILED fetch is fatal.
#
# Both halves matter. Fetching only when the tracking ref is missing compares
# against a ref that may be days behind; and tolerating a failed fetch because
# "the rev-parse below re-derives it" is worse, because rev-parse happily
# succeeds against the stale ref. Measured on a broken remote with a tracking ref
# pinned at v10 while the real base held v11: the guard printed
# "OK (v10 -> v11)" and exited 0 on a real collision. Not fetching and not
# knowing are the same state, and this check answers "cannot verify" with exit 2.
#
# NO --depth here. `git fetch --depth=1` against a COMPLETE clone does not fetch
# less, it writes .git/shallow and truncates the whole repository: measured, a
# 5-commit clone came out at 1 commit needing `--unshallow` to recover. This
# script is locally runnable, so it must not mutate the caller's repo to answer a
# read-only question.
#
# git's own error is left on stderr: "cannot verify the bump" with no reason is
# the hardest version of this failure to act on.
if ! git fetch --quiet "$REMOTE" "$BASE_REF"; then
    if git rev-parse --verify --quiet "$REMOTE/$BASE_REF" >/dev/null; then
        echo "ERROR: cannot fetch $REMOTE/$BASE_REF - unable to verify the bump" >&2
        echo "       A local $REMOTE/$BASE_REF exists but cannot be confirmed current," >&2
        echo "       and a stale base passes a real collision. Refusing." >&2
    else
        echo "ERROR: cannot resolve $REMOTE/$BASE_REF - unable to verify the bump" >&2
        echo "       (fetch it, or set PR_BASE_REF/PR_REMOTE)" >&2
    fi
    exit 2
fi

# Resolved ONLY from the remote-tracking ref. An earlier draft fell back to
# FETCH_HEAD, which is whatever the last fetch in this repo happened to leave
# behind - an unrelated branch, or a stale commit from a previous run - and
# nothing tied it to BASE_REF. Comparing against that and printing OK is a pass
# with no base behind it.
BASE_TIP="$(git rev-parse --verify --quiet "$REMOTE/$BASE_REF")" || {
    echo "ERROR: cannot resolve $REMOTE/$BASE_REF - unable to verify the bump" >&2
    echo "       (fetch it, or set PR_BASE_REF/PR_REMOTE)" >&2
    exit 2
}

# The guarded path must still exist at the base, or this whole script is a
# permanent green: `git log -- <moved path>` returns nothing for every PR, and
# the selftest cannot catch it because its fixtures hardcode the same path.
if ! git cat-file -e "$BASE_TIP:$PROTO_FILE" 2>/dev/null; then
    echo "ERROR: $PROTO_FILE does not exist at $REMOTE/$BASE_REF - this guard is stale." >&2
    echo "       The file moved or was renamed; update PROTO_FILE in this script" >&2
    echo "       and PROTO_REL in scripts/tests/check-proto-version-bump-selftest.sh." >&2
    exit 2
fi

# The head sha travels from the event payload, so it need not be present yet.
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
PROTO_PATCH="$(git log -p --format='' "$MERGE_BASE..$HEAD_SHA" -- "$PROTO_FILE")" || {
    echo "ERROR: cannot read this PR's $PROTO_FILE history - unable to verify the bump" >&2
    exit 2
}

# Read the base version BEFORE deciding the line was untouched.
#
# Both greps anchor `pub const` at column 0, so if the const is ever indented
# into a `mod`, gains an attribute, or is renamed, nothing matches and the
# untouched branch below would print a pass for every PR forever, with
# read_version's loud errors sitting on a branch that is never reached. Reading
# the base first means an unlocatable const fails loud on BOTH paths - the same
# reason the file-existence check above exists, one level finer.
BASE_V="$(read_version "$BASE_TIP" "$REMOTE/$BASE_REF tip")" || exit 1

if ! grep -qE '^[-+]pub const PROTO_VERSION' <<<"$PROTO_PATCH"; then
    echo "check-proto-version-bump: PROTO_VERSION line untouched by this PR; nothing to check"
    echo "  head=${HEAD_SHA} base=${REMOTE}/${BASE_REF}@${BASE_TIP} base_version=v${BASE_V}"
    exit 0
fi

HEAD_V="$(read_version "$HEAD_SHA" "PR head")" || exit 1

if (( HEAD_V > BASE_V )); then
    echo "check-proto-version-bump: OK (v$BASE_V -> v$HEAD_V)"
    echo "  head=${HEAD_SHA} base=${REMOTE}/${BASE_REF}@${BASE_TIP}"
    exit 0
fi

echo "ERROR: PROTO_VERSION bump is not monotonic against $REMOTE/$BASE_REF." >&2
echo "       $REMOTE/$BASE_REF tip: v$BASE_V" >&2
echo "       this PR:               v$HEAD_V" >&2
echo "" >&2
if (( HEAD_V == BASE_V )); then
    # State the observation, then the likely cause as a likely cause. An earlier
    # version asserted "a parallel branch bumped to the same value and merged
    # first" as fact, which is a fabricated diagnosis for the other topology that
    # lands here: a PR that bumps and then reverts within its own commits reads
    # as touched, ends level with the base, and has no parallel branch anywhere.
    # Sending that author to look for one wastes their time.
    #
    # The two are not indistinguishable, only not worth distinguishing. A value
    # comparison at the merge-base cannot separate them (both read head ==
    # merge-base), but PROTO_PATCH already holds the discriminator: the revert
    # carries a `+` line above BASE_V and the collision does not. That is a
    # heuristic, not a total function - overshoot to v12, come back to v11, and
    # collide at v11 and it mislabels - so this branch names both instead.
    echo "       This PR's commits touch the PROTO_VERSION line, but its value is" >&2
    echo "       level with $REMOTE/$BASE_REF rather than above it, so the change" >&2
    echo "       lands with no effective bump." >&2
    echo "" >&2
    echo "       Usually: a parallel branch took v$BASE_V and merged first. Your" >&2
    echo "       identical one-line change then merged cleanly, leaving no bump." >&2
    echo "       Fix: re-bump to v$((BASE_V + 1)) in $PROTO_FILE (and any" >&2
    echo "       assert_eq!(PROTO_VERSION, ...) pinned to the old value)." >&2
    echo "" >&2
    echo "       If instead the value is unchanged for any reason - a revert, a" >&2
    echo "       reflow, an attribute - and the wire shape did not change, no bump" >&2
    echo "       is needed: undo the const edits so the line reads as untouched." >&2
else
    echo "       This PR lowers PROTO_VERSION. It must only ever increase." >&2
    echo "       Fix: set it above v$BASE_V in $PROTO_FILE." >&2
fi
exit 1
