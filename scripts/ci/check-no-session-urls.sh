#!/usr/bin/env bash
# check-no-session-urls.sh - CI gate blocking claude.ai/code session URLs from
# landing in a PR's commit messages, title, or body.
#
# A session URL (the `Claude-Session:` trailer the harness appends by default,
# or a bare paste) is an internal, irreversible leak: once a commit carrying it
# is pushed to a public branch a force-push does NOT retract it. The standing
# user rule forbids it; this gate catches the harness default before merge.
#
# Diff-scoped: scans ONLY the PR's own commit range (base..head), never full
# history - main already carries old trailer commits and a full-history scan
# would be permanently red on every PR.
#
# Inputs (all via env; a workflow must NEVER interpolate PR-controlled text
# inline into a `run:` block - that is a shell-injection vector):
#   PR_BASE_SHA  base commit of the PR (github.event.pull_request.base.sha)
#   PR_HEAD_SHA  head commit of the PR (github.event.pull_request.head.sha)
#   PR_TITLE     PR title
#   PR_BODY      PR body
#
# Exit 0 when clean; exit 1 on a hit OR on an infrastructure failure (an
# unfetched base SHA makes the range unresolvable). A missing base must fail
# LOUD, never pass vacuously - the same failure class the check-no-internal-refs
# git ls-files capture guards against (review #503).
#
# Run locally:
#   PR_BASE_SHA=origin/main PR_HEAD_SHA=HEAD PR_TITLE="t" PR_BODY="b" \
#     bash scripts/ci/check-no-session-urls.sh

set -uo pipefail

# Single pattern: a real session URL is always `claude.ai/code/<session-path>`,
# and the `Claude-Session:` trailer embeds that same form - so requiring the
# trailing slash covers every real leak while letting prose that merely NAMES
# the concept ("scans for claude.ai/code") pass, including this gate's own PR
# body and docs.
PATTERN='claude\.ai/code/'

BASE="${PR_BASE_SHA:-}"
HEAD="${PR_HEAD_SHA:-HEAD}"
TITLE="${PR_TITLE:-}"
BODY="${PR_BODY:-}"

VIOLATIONS=0
REPORT=""

# --- Commit-message scan (diff-scoped) ---------------------------------------
if [[ -z "$BASE" ]]; then
    echo "check-no-session-urls: PR_BASE_SHA is empty - cannot scope the commit scan." >&2
    echo "  Infrastructure error (the workflow must pass the PR base SHA), not a" >&2
    echo "  clean pass. Failing loud." >&2
    exit 1
fi

# The base commit must actually exist in this checkout, or the range is
# unresolvable and the scan would silently cover nothing. Fail loud.
if ! git rev-parse --verify --quiet "${BASE}^{commit}" >/dev/null 2>&1; then
    echo "check-no-session-urls: base SHA '${BASE}' is not present in this checkout." >&2
    echo "  The range ${BASE}..${HEAD} is unresolvable - the workflow needs" >&2
    echo "  fetch-depth: 0 (or an explicit base fetch). Failing loud, not green." >&2
    exit 1
fi

if ! commit_shas=$(git rev-list "${BASE}..${HEAD}" 2>/dev/null); then
    echo "check-no-session-urls: 'git rev-list ${BASE}..${HEAD}' failed - range unresolvable." >&2
    echo "  Infrastructure error, not a clean pass. Failing loud." >&2
    exit 1
fi

while IFS= read -r sha; do
    [[ -z "$sha" ]] && continue
    msg=$(git log -1 --format='%B' "$sha" 2>/dev/null || true)
    hits=$(printf '%s\n' "$msg" | grep -nE "$PATTERN" || true)
    [[ -z "$hits" ]] && continue
    while IFS= read -r line; do
        REPORT+="[commit ${sha:0:12}] $line"$'\n'
        VIOLATIONS=$((VIOLATIONS + 1))
    done <<< "$hits"
done <<< "$commit_shas"

# --- PR title + body scan ----------------------------------------------------
for field in TITLE BODY; do
    val="${!field}"
    [[ -z "$val" ]] && continue
    hits=$(printf '%s\n' "$val" | grep -nE "$PATTERN" || true)
    [[ -z "$hits" ]] && continue
    label="PR body"
    [[ "$field" == "TITLE" ]] && label="PR title"
    while IFS= read -r line; do
        REPORT+="[${label}] $line"$'\n'
        VIOLATIONS=$((VIOLATIONS + 1))
    done <<< "$hits"
done

if [[ $VIOLATIONS -eq 0 ]]; then
    echo "check-no-session-urls: no violations found"
    exit 0
fi

{
    echo "check-no-session-urls: $VIOLATIONS violation(s) found"
    echo
    printf '%s' "$REPORT"
    echo
    echo "A claude.ai/code session URL (often the 'Claude-Session:' commit trailer"
    echo "the harness appends by default) must not reach a public branch: the URL"
    echo "is internal and a force-push does NOT retract an already-pushed commit."
    echo "To fix:"
    echo "  - PR body/title: edit the field to remove the URL. A re-push alone does"
    echo "    NOT help - the body/title is stored on the PR, not in a commit."
    echo "  - commit message: rewrite the offending commit(s) to strip the trailer,"
    echo "      git rebase -i ${BASE}      # reword each flagged commit"
    echo "    then force-push the branch; the leak is gone only once the commit"
    echo "    carrying it is rewritten."
} >&2
exit 1
