#!/usr/bin/env bash
# scripts/ci/check-registry-schema-bump.sh
#
# Registry schema field-set / version bump guard. The collision
# class: PR 2090 minted v33 for lineage_reason; PR 2091 added spawn_id and
# spawn_provenance AT v33 without a bump. An fno installed between the two
# merges called itself v33, read the new rows as its own version, and
# Python's strict reader refused the whole registry.
#
# Rule: when `fields` at the PR head differs from `fields` at the base tip,
# the head version must be strictly greater. State comparison, not a patch
# scan: the toml is the version's single owner. A stale branch that never
# bumped fails too, told to merge main.
#
# Exit codes: 0 ok; 1 a fields change without a strictly greater version;
# 2 fail closed: base unreachable, head unresolvable, or toml unparseable.
#
# Env: PR_HEAD_SHA (default HEAD), PR_BASE_REF (default main), PR_REMOTE
# (default origin). Env-only config; any argument refuses.
set -uo pipefail

if [[ ${#} -gt 0 ]]; then
    echo "ERROR: arg refusal: env-only config (PR_HEAD_SHA, PR_BASE_REF, PR_REMOTE)" >&2
    exit 2
fi

REMOTE="${PR_REMOTE:-origin}"
BASE_REF="${PR_BASE_REF:-main}"
# Empty PR_HEAD_SHA under CI must not become HEAD: HEAD is the merge ref
# there and cannot show the PR's own field set.
if [[ -n "${CI:-}" && -z "${PR_HEAD_SHA:-}" ]]; then
    echo "ERROR: PR_HEAD_SHA empty under CI" >&2
    exit 2
fi
HEAD_SHA="${PR_HEAD_SHA:-HEAD}"

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "ERROR: not inside a git repository" >&2
    exit 2
}
cd "$REPO_ROOT" || exit 2

SCHEMA_FILE="crates/fno-agents/src/registry_schema.toml"

# version and fields at a rev, failing loud on anything unparseable. An
# unreadable schema must never read as "no bump needed".
read_version() {
    local rev="$1" label="$2" blob line
    blob="$(git show "$rev:$SCHEMA_FILE")" || {
        echo "ERROR: cannot read schema toml at $label ($rev)" >&2
        return 1
    }
    line="$(grep -E '^version[[:space:]]*=' <<<"$blob" || true)"
    if [[ -z "$line" ]]; then
        echo "ERROR: no version line in $SCHEMA_FILE at $label" >&2
        return 1
    fi
    if [[ ! "$line" =~ =[[:space:]]*([0-9]+) ]]; then
        echo "ERROR: cannot parse version at $label: $line" >&2
        return 1
    fi
    printf '%s\n' "${BASH_REMATCH[1]}"
}

# The single-line `fields = [...]` array, split to sorted names, one per
# line. Empty output when the key is absent (the introducing PR). Tolerates
# trailing commas and quoting.
read_fields() {
    local rev="$1" label="$2" blob
    blob="$(git show "$rev:$SCHEMA_FILE")" || {
        echo "ERROR: cannot read schema toml at $label ($rev)" >&2
        return 1
    }
    grep -E '^fields[[:space:]]*=' <<<"$blob" \
        | sed -e 's/^[^=]*=[[:space:]]*//' -e 's/^\[//; s/\][[:space:]]*$//' \
        | tr ',' '\n' | tr -d '"'"'"' \t' \
        | sed '/^$/d' | sort
}

# A `fields` line that does not end the array means a multi-line TOML array,
# which this reader cannot see past line one: both sides would then compare
# truncated sets and a real bump could read as "unchanged". Fail closed and
# name the fix instead. A multi-line array is refused BEFORE the compare by
# re-reading the raw line here.
fields_array_is_single_line() {
    local rev="$1" label="$2" blob
    blob="$(git show "$rev:$SCHEMA_FILE")" || return 1
    local line
    line="$(grep -E '^fields[[:space:]]*=' <<<"$blob" || true)"
    if [[ -z "$line" ]]; then
        return 0
    fi
    if [[ "$line" != *"]"* ]]; then
        echo "ERROR: the fields array at $label spans multiple lines; this" >&2
        echo "       guard reads one line only. Keep the array on a single" >&2
        echo "       line, or extend this reader." >&2
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Resolve the base tip. Fail CLOSED when unreachable: a failed fetch that
# silently skipped the check would re-open the collision class. Fetch is
# unconditional and a failed fetch is fatal; no --depth (truncates a
# complete clone); explicit refspec beats a narrowed fetch config. Same
# discipline as check-proto-version-bump.sh, for the same reasons.
# ---------------------------------------------------------------------------
if ! git fetch --quiet "$REMOTE" \
        "+refs/heads/$BASE_REF:refs/remotes/$REMOTE/$BASE_REF"; then
    echo "ERROR: cannot fetch $REMOTE/$BASE_REF - unable to verify the bump" >&2
    exit 2
fi
BASE_TIP="$(git rev-parse --verify --quiet "$REMOTE/$BASE_REF")" || {
    echo "ERROR: cannot resolve $REMOTE/$BASE_REF - unable to verify the bump" >&2
    exit 2
}
if ! git cat-file -e "$BASE_TIP:$SCHEMA_FILE" 2>/dev/null; then
    echo "ERROR: $SCHEMA_FILE does not exist at $REMOTE/$BASE_REF - this guard is stale" >&2
    exit 2
fi
if ! git rev-parse --verify --quiet "$HEAD_SHA^{commit}" >/dev/null; then
    git fetch --quiet "$REMOTE" "$HEAD_SHA" 2>/dev/null || true
fi
if ! git rev-parse --verify --quiet "$HEAD_SHA^{commit}" >/dev/null; then
    echo "ERROR: cannot resolve PR head $HEAD_SHA - unable to verify the bump" >&2
    exit 2
fi

BASE_V="$(read_version "$BASE_TIP" "$REMOTE/$BASE_REF tip")" || exit 2
HEAD_V="$(read_version "$HEAD_SHA" "PR head")" || exit 2
BASE_F="$(read_fields "$BASE_TIP" "$REMOTE/$BASE_REF tip")" || exit 2
HEAD_F="$(read_fields "$HEAD_SHA" "PR head")" || exit 2
fields_array_is_single_line "$BASE_TIP" "$REMOTE/$BASE_REF tip" || exit 2
fields_array_is_single_line "$HEAD_SHA" "PR head" || exit 2

if [[ -z "$BASE_F" ]]; then
    echo "check-registry-schema-bump: no baseline fields at base tip"
    echo "  head=$HEAD_SHA base=$REMOTE/$BASE_REF@$BASE_TIP version=v$HEAD_V"
    exit 0
fi

if [[ "$HEAD_F" == "$BASE_F" ]]; then
    echo "check-registry-schema-bump: fields unchanged; nothing to check"
    echo "  head=$HEAD_SHA base=$REMOTE/$BASE_REF@$BASE_TIP version=v$BASE_V"
    exit 0
fi

# 10# forces base 10: bash reads a leading-zero literal as octal, and this
# guard reads the toml as text, where a malformed `version = 010` would
# compare as 8 before the TOML parser ever sees it.
if (( 10#$HEAD_V > 10#$BASE_V )); then
    echo "check-registry-schema-bump: OK (v$BASE_V -> v$HEAD_V, field set changed)"
    echo "  head=$HEAD_SHA base=$REMOTE/$BASE_REF@$BASE_TIP"
    exit 0
fi

echo "ERROR: registry schema field set changed without a monotonic version bump." >&2
echo "  $REMOTE/$BASE_REF tip: v$BASE_V, fields:" >&2
printf '    %s\n' $BASE_F >&2
echo "  this PR (head):        v$HEAD_V, fields:" >&2
printf '    %s\n' $HEAD_F >&2

ADDED="$(comm -13 <(printf '%s\n' $BASE_F) <(printf '%s\n' $HEAD_F))"
if [[ -n "$ADDED" ]]; then
    echo "  Added at head:" >&2
    printf '    %s\n' $ADDED >&2
fi
REMOVED="$(comm -13 <(printf '%s\n' $HEAD_F) <(printf '%s\n' $BASE_F))"
if [[ -n "$REMOVED" ]]; then
    echo "  Removed at head:" >&2
    printf '    %s\n' $REMOVED >&2
fi

echo "  Fix: add the new fields to the fields array, then bump version in" >&2
echo "  crates/fno-agents/src/registry_schema.toml, in the same PR. Never add" >&2
echo "  a RegistryEntry field at the current version: an equal-version fno" >&2
echo "  reads the new rows as its own version and the strict Python reader" >&2
echo "  refuses the whole registry." >&2
exit 1
