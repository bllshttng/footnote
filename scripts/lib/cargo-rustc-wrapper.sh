#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
    echo "cargo-rustc-wrapper: missing rustc command" >&2
    exit 2
fi

# Cargo probes the compiler with `rustc -vV` once per invocation, before any
# real compile - the one place a line can announce which path this build
# takes without flooding the log with one line per rustc call. sccache was
# installed 2026-08-19; before that this wrapper fell through to bare rustc
# with no announcement, and every worktree compiled cold behind a working
# switch nobody had flipped.
case " $* " in
    *" -vV "*)
        echo "cargo-rustc-wrapper: $(command -v sccache >/dev/null 2>&1 && echo 'sccache (shared cache)' || echo 'bare rustc (sccache absent)')" >&2
        ;;
esac

if command -v sccache >/dev/null 2>&1; then
    export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
    exec sccache "$@"
fi

compiler="$1"
shift
exec "$compiler" "$@"
