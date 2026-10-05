#!/usr/bin/env bash
# scripts/lib/events-validate.sh
#
# Thin adapter over the native validator (fno doctor event emit-envelope
# --validate-only). The judge lives in the store binary now: the schema is
# compiled in and every writer is judged at the store commit, so this
# adapter hands the payload to that one validator and relays its verdict.
# The shell contract it keeps stable:
#
#   validate_event TYPE JSON_PAYLOAD
#       rc=0  valid
#       rc=1  invalid (diagnostic names the failed field on stderr)
#       rc=2  substrate failure (no usable fno binary, payload not one
#             JSON object)
#
# Compatibility:
#   - bash 3.2 (macOS default). No associative arrays, no process
#     substitution.
#   - Portability: a consumer project may carry ONLY scripts/lib from the
#     plugin. The adapter locates the fno binary through FNO_BIN, the
#     checkout build above this lib, or PATH, in that order; there is no
#     Python leg to resolve anymore.

set -uo pipefail

validate_event() {
    local type="${1:?type required}"
    local payload="${2:?payload required}"

    # The caller's schema-path override stays a contract: an explicit path
    # that cannot be read is a substrate failure, never a validation pass
    # against some other schema. The native judge compiles the schema in,
    # so a readable override changes nothing about the verdict itself.
    if [ -n "${EVENTS_SCHEMA_PATH:-}" ] && [ ! -r "$EVENTS_SCHEMA_PATH" ]; then
        printf '%s\n' "validate-event: schema unavailable: $EVENTS_SCHEMA_PATH" >&2
        return 2
    fi

    local lib_dir root bin
    lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" 2>/dev/null && pwd)"
    root="$(cd "$lib_dir/../.." 2>/dev/null && pwd)"
    bin="${FNO_BIN:-}"
    if [ -z "$bin" ] && [ -n "$root" ] && [ -x "$root/crates/fno/target/debug/fno" ]; then
        bin="$root/crates/fno/target/debug/fno"
    fi
    if [ -z "$bin" ] && [ -n "$root" ] && [ -x "$root/crates/fno/target/release/fno" ]; then
        bin="$root/crates/fno/target/release/fno"
    fi
    if [ -z "$bin" ]; then
        bin="$(command -v fno 2>/dev/null || true)"
    fi
    if [ -z "$bin" ]; then
        printf '%s\n' "validate-event: no usable fno binary for the validator" >&2
        return 2
    fi
    printf '%s' "$payload" | "$bin" doctor event emit-envelope --validate-only --expect-type "$type"
}
