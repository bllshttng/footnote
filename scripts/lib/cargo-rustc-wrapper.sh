#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
    echo "cargo-rustc-wrapper: missing rustc command" >&2
    exit 2
fi

if command -v sccache >/dev/null 2>&1; then
    export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
    exec sccache "$@"
fi

compiler="$1"
shift
exec "$compiler" "$@"
