#!/usr/bin/env bash
#
# PostToolUse on Edit|Write: format the file that was just written.
#
# The census of 64,792 Bash calls over 21 days found formatting run as its own
# gated command after almost every Rust edit. A PostToolUse hook does it for
# free, so the agent never spends a permission decision on `cargo fmt`.
#
# Rust ONLY, and that is measured, not a scope cut. The plan called for a
# `ruff format` leg beside it. The tree refuses one: CI runs `ruff check`
# (guards.yml, main-python-static) and no `ruff format` anywhere, so the repo
# has never been ruff-formatted - 458 of 522 files under cli/src, 3 of 5 under
# hooks/, 34 of 38 under scripts/ would be rewritten. A hook formatting Python
# on edit would put that churn in every PR touching a Python file, to satisfy
# no gate. Rust is the opposite case: `cargo +1.94.1 fmt --all --check` is a
# real gate in scripts/ci/preflight.sh, so the hook here does work CI demands.
# Add the Python leg the day the repo adopts `ruff format` repo-wide.
#
# The one invariant: a formatter must NEVER fail the edit. Every failure path
# here exits 0 silently - a missing toolchain, a syntax error the formatter
# rejects, a path outside the repo. Type checks (mypy) and lints (clippy) stay
# in scripts/ci/preflight.sh; they are slow and they have opinions an edit
# mid-refactor should not have to satisfy.

set -uo pipefail

PINNED_FMT="1.94.1"   # keep in lockstep with scripts/ci/preflight.sh PINNED_FMT

PAYLOAD="$(cat 2>/dev/null || true)"
[ -n "$PAYLOAD" ] || exit 0

FILE_PATH=""
if command -v jq >/dev/null 2>&1; then
    FILE_PATH="$(printf '%s' "$PAYLOAD" | jq -er '.tool_input.file_path | select(type == "string" and length > 0)' 2>/dev/null || true)"
elif command -v python3 >/dev/null 2>&1; then
    FILE_PATH="$(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    value = json.load(sys.stdin).get("tool_input", {}).get("file_path")
    if isinstance(value, str) and value:
        print(value)
except Exception:
    pass
' 2>/dev/null || true)"
fi

[ -n "$FILE_PATH" ] || exit 0
[ -f "$FILE_PATH" ] || exit 0

_repo_root() {
    local dir
    dir="$(cd "$(dirname "$FILE_PATH")" 2>/dev/null && pwd)" || return 1
    while [ "$dir" != "/" ]; do
        if [ -e "$dir/.git" ]; then
            printf '%s\n' "$dir"
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    return 1
}

REPO_ROOT="$(_repo_root || true)"
[ -n "$REPO_ROOT" ] || exit 0

case "$FILE_PATH" in
    /*) ABS="$FILE_PATH" ;;
    *)  ABS="$(cd "$(dirname "$FILE_PATH")" 2>/dev/null && pwd)/$(basename "$FILE_PATH")" || exit 0 ;;
esac
REL="${ABS#"$REPO_ROOT"/}"
[ "$REL" != "$ABS" ] || exit 0

_sum() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'
    else
        cksum "$1" 2>/dev/null | awk '{print $1}'
    fi
}

BEFORE="$(_sum "$ABS")"

case "$ABS" in
    *.rs)
        case "$REL" in
            crates/*) ;;
            *) exit 0 ;;
        esac
        command -v rustfmt >/dev/null 2>&1 || exit 0
        rustfmt "+$PINNED_FMT" --edition 2021 "$ABS" >/dev/null 2>&1 || exit 0
        ;;
    *)
        exit 0
        ;;
esac

AFTER="$(_sum "$ABS")"
if [ -n "$BEFORE" ] && [ -n "$AFTER" ] && [ "$BEFORE" != "$AFTER" ]; then
    echo "format-on-edit: rewrote $REL"
fi
exit 0
