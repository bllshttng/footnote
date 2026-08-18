#!/usr/bin/env bash
# pre-push protected-branch gate, destination-only.
#
# Git feeds this hook one stdin line per ref being written:
#   <local_ref> <local_sha> <remote_ref> <remote_sha>
# The gate reads the DESTINATION (remote_ref) and nothing else. It never
# consults the pushing checkout's current branch: a checkout on main pushing
# a backup or feature ref is not the protected-branch case this hook exists
# to stop, and refusing it trains the operator on the hook-skipping flag,
# which disarms this gate on every future push.
# fno-pre-push-destination-gate (marker used by the installer's --check)

set -u

PROTECTED=("main" "master" "develop" "dev")

refuse() {
    # $1 = destination ref as git named it, $2 = protected branch it writes
    echo "PUSH BLOCKED: '$1' writes protected branch '$2'." >&2
    echo "Protected branches: ${PROTECTED[*]}" >&2
    echo "This gate reads the DESTINATION, not your current branch. Pushing a" >&2
    echo "non-protected ref from a checkout on main is allowed." >&2
    echo "Open a PR instead:" >&2
    echo "  git push origin HEAD:refs/heads/feature/<name>" >&2
}

while read -r local_ref local_sha remote_ref remote_sha; do
    [ -n "$remote_ref" ] || continue
    case "$remote_ref" in
        refs/heads/*) dest="${remote_ref#refs/heads/}" ;;
        *) continue ;;   # tags, notes, refs/for/*: not a branch write
    esac
    for b in "${PROTECTED[@]}"; do
        [ "$dest" = "$b" ] && { refuse "$remote_ref" "$dest"; exit 1; }
    done
done

exit 0
