#!/usr/bin/env bash
# parse-claims-arg.sh - classify a /blueprint argument as a node-id claim or not.
#
# Usage:
#   eval "$(parse-claims-arg.sh "$ARG")"
#
# When ARG is a well-formed node id (a <prefix>-<4..8 hex> token), prints two:
#   CLAIMS_ID="<prefix>-<hex>"
#   CLAIMS_SEED_ARG=$(jq-resolved title + details)
#
# When ARG is not a node id, prints `CLAIMS_ID=""` and leaves CLAIMS_SEED_ARG
# unset, so the caller falls through to its existing path/description
# classifier without changes.
#
# Exit codes:
#   0  recognised the argument shape (node id or not)
#   1  argument was a node id but `fno backlog get` failed (unknown id, no
#      network, missing CLI). The caller should surface stderr and halt.

set -euo pipefail

# Single fno-vs-external-vs-none classifier, shared with graph-resolve.sh so the
# id-shape test has one home. Sourced as a bundled sibling (BASH_SOURCE resolves
# beside this file at both the repo-root and skills/ locations).
source "$(dirname "${BASH_SOURCE[0]}")/node-id.sh"

ARG="${1:-}"

if [[ -z "$ARG" ]]; then
    printf 'CLAIMS_ID=""\n'
    exit 0
fi

# Only an fno node id is a /blueprint claim target. External tracker ids and
# non-ids are not fno claims; the caller falls through to its path/description
# classifier. A shape match used to trigger `fno backlog get` on any matching
# token, which hard-failed for an external id that collided with the fno shape.
if [[ "$(node_id_kind "$ARG")" != "fno" ]]; then
    printf 'CLAIMS_ID=""\n'
    exit 0
fi

# Argument is an ab-id. Resolve title + details from the graph. Capture
# stdout and stderr separately so a broken `fno` install (rc!=0 with a
# real error message) doesn't masquerade as "id not found".
ERR_FILE="$(mktemp "${TMPDIR:-/tmp}/parse-claims-arg.XXXXXX")"
trap 'rm -f "$ERR_FILE"' EXIT
NODE_JSON="$(fno backlog get "$ARG" 2>"$ERR_FILE")"
RC=$?
if [[ $RC -ne 0 || -z "$NODE_JSON" ]]; then
    ERR_TEXT="$(<"$ERR_FILE")"
    if [[ -n "$ERR_TEXT" ]]; then
        printf 'echo %q >&2\n' "Error resolving ab-id $ARG (rc=$RC): $ERR_TEXT"
    else
        printf 'echo %q >&2\n' "Error: ab-id $ARG not found on graph"
    fi
    printf 'exit 1\n'
    exit 1
fi

# Compose the seed text from title + details. Both fields exist on every
# node (`details` may be null, in which case jq emits an empty string).
SEED="$(printf '%s' "$NODE_JSON" | jq -r '.title + "\n\n" + (.details // "")')"

# Render shell-safe assignments. printf %q quotes embedded newlines and
# special characters so the eval'd output round-trips losslessly.
printf 'CLAIMS_ID=%q\n' "$ARG"
printf 'CLAIMS_SEED_ARG=%q\n' "$SEED"
