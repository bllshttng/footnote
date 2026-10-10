#!/usr/bin/env bash
set -euo pipefail

# The generated state-dir stub (STATE_DIR etc.), so the fallback-writer log
# path never hardcodes $HOME/.fno. REPO_ROOT is preset from this file's own
# location so the stub's git rev-parse subshell never runs on the hot path.
REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
# shellcheck source=scripts/lib/paths.sh
source "$REPO_ROOT/scripts/lib/paths.sh"

if [[ $# -eq 0 ]]; then
    echo "cargo-rustc-wrapper: missing rustc command" >&2
    exit 2
fi

# One cargo builds at a time on this machine: two concurrent builds from
# separate worktrees took load to 508 on 12 cores. Probes never wait, and
# admission fails open, so CI, a clone without fno, and an older fno-agents
# that lacks build-admit all build normally. The fail-open is not silent: a
# missing verb is named on stderr with its remedy and journaled as an event.
# See docs/architecture/test-run-lifecycle.md "Build admission" / "Run admission".
admit() {
    local mode="$1"
    # The run door names the program cargo is about to start, so it can
    # refuse an agent's test binary; the compile door names none. Only the
    # admission call sees it: the program itself does not.
    local program="${2:-}"
    # A failed admission is said once per cargo, not once per crate. A
    # marker older than an hour belongs to an earlier cargo with this pid.
    unadmitted="${TMPDIR:-/tmp}/fno-build-unadmitted.$PPID"
    if [[ -e "$unadmitted" && -n "$(find "$unadmitted" -mmin -60 2>/dev/null)" ]]; then
        return 0
    elif command -v fno-agents >/dev/null 2>&1; then
        repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
        rc=0
        # Cargo exports CARGO_MANIFEST_DIR to this wrapper. The live-store
        # fence reads it as "a cargo-launched build" and refuses the claims
        # store, so with it set every build ran claimless. Only this call
        # drops it: the compiler and the test binary keep it.
        env -u CARGO_MANIFEST_DIR FNO_CARGO_RUN_PROGRAM="$program" fno-agents test-run "${mode}-admit" --cargo-pid "$PPID" --worktree "$repo_root" || rc=$?
        if [[ "$rc" -eq 86 ]]; then
            # Slot-busy, and an agent's test binary, are policy, not
            # breakage: the door printed the answer ("commit, push, CI runs
            # it"). Stop the compile or run here; failing open would build
            # unadmitted under the very saturation the gate exists to cap.
            exit 86
        fi
        if [[ "$rc" -ge 128 ]]; then
            # A signal stopped the wait: cargo is stopping, so compile or
            # run nothing.
            exit "$rc"
        elif [[ "$rc" -ne 0 ]]; then
            # A binary that has the verb refuses a bare call with
            # "--cargo-pid is required". An older one that lacks the verb
            # says something else: the line then names the remedy and an
            # event lands in the journal. The usage output is captured, not
            # piped: the bare call exits nonzero, and pipefail would turn a
            # matched grep into a nonzero pipeline.
            usage="$(fno-agents test-run "${mode}-admit" 2>&1 || true)"
            if grep -q -- "--cargo-pid" <<<"$usage"; then
                reason=error
                if [[ "$mode" == "build" ]]; then
                    echo "cargo-rustc-wrapper: build admission unavailable (exit $rc); building unadmitted" >&2
                else
                    echo "cargo-rustc-wrapper: run admission unavailable (exit $rc); running unadmitted" >&2
                fi
            else
                reason=verb_missing
                if [[ "$mode" == "build" ]]; then
                    echo "cargo-rustc-wrapper: the deployed fno-agents ($(command -v fno-agents)) has no build-admit; building unadmitted. Run: fno doctor update" >&2
                else
                    echo "cargo-rustc-wrapper: the deployed fno-agents ($(command -v fno-agents)) has no run-admit; running unadmitted. Run: fno doctor update" >&2
                fi
            fi
            if command -v fno >/dev/null 2>&1; then
                ( fno doctor event emit "${mode}_admission_unavailable" --json "{\"reason\":\"$reason\",\"exit\":$rc,\"mode\":\"$mode\"}" >/dev/null 2>&1 & )
            fi
            : >"$unadmitted" 2>/dev/null || true
        fi
    fi
}

# The execute door: cargo calls this script with --run before every test
# binary and doctest (the macOS triple runner lines in .cargo/config.toml),
# the same way it calls it without --run before every compile. One admission
# contract at both doors: a slot keyed to the cargo pid, no TTL, free on
# cargo exit; a signal that stops the wait stops the compile or run too.
if [[ "${1:-}" == "--run" ]]; then
    shift
    # A worktree nested inside another checkout reads both .cargo/config.toml
    # files, and cargo joins runner arrays, so the program can be this wrapper
    # again under a path relative to the wrong directory. Drop the repeat:
    # one admission, one exec.
    while [[ "${1:-}" == *cargo-rustc-wrapper.sh && "${2:-}" == "--run" ]]; do
        shift 2
    done
    if [[ $# -eq 0 ]]; then
        echo "cargo-rustc-wrapper: --run needs a program" >&2
        exit 2
    fi
    admit run "$1"
    exec "$@"
fi

# sccache is opt-in; the default is bare rustc. The fleet measured 60 percent
# hits on cacheable calls while the costly workspace crates never cached
# (worktree paths bake into their keys), the cache sat full, and one wedged
# server parked every build for up to an hour. A build uses sccache only when
# the project `.fno/config.toml` sets `build.sccache = true` (the global
# `~/.fno/config.toml` is the fallback) or FNO_SCCACHE=1, sccache is
# installed, and SCCACHE_DISABLE is not 1. The 2026-08-20 note stands: Cargo
# decides incremental before this script runs, so `.cargo/config.toml`'s
# [build] incremental = false is what enables caching, not this wrapper. An
# uncached compile still passes build and run admission below.
build_sccache_opted_in() {
    [[ "${FNO_SCCACHE:-}" == "1" ]] && return 0
    local file value
    for file in "$REPO_ROOT/.fno/config.toml" "${CONFIG_FILE:-$STATE_DIR/config.toml}"; do
        [[ -f "$file" ]] || continue
        # Minimal TOML read for one boolean: the `sccache` key under [build],
        # or the dotted `build.sccache` before any section. The project
        # file's answer settles it; the global file is only the fallback
        # for an unset key.
        value="$(awk '
            { line = $0; sub(/#.*/, "", line); gsub(/[[:space:]]/, "", line) }
            line ~ /^\[/ { section = line; gsub(/[\[\]]/, "", section); next }
            section == "build" && line ~ /^sccache=/ { print line; exit }
            section == "" && line ~ /^build\.sccache=/ { print line; exit }
        ' "$file")"
        if [[ -n "$value" ]]; then
            if [[ "$value" == *'=true' ]]; then
                return 0
            fi
            return 1
        fi
    done
    return 1
}

if [[ "${SCCACHE_DISABLE:-0}" != "1" ]] && build_sccache_opted_in && command -v sccache >/dev/null 2>&1; then
    HAS_SCCACHE=1
else
    HAS_SCCACHE=0
fi

case " $* " in
    *" -vV "*)
        if [[ "$HAS_SCCACHE" -eq 1 ]]; then
            echo "cargo-rustc-wrapper: sccache (shared cache)" >&2
        elif [[ "${SCCACHE_DISABLE:-0}" == "1" ]] && command -v sccache >/dev/null 2>&1; then
            echo "cargo-rustc-wrapper: bare rustc (SCCACHE_DISABLE=1)" >&2
        elif ! command -v sccache >/dev/null 2>&1; then
            echo "cargo-rustc-wrapper: bare rustc (sccache not installed)" >&2
        else
            echo "cargo-rustc-wrapper: bare rustc (sccache is opt-in: build.sccache = true or FNO_SCCACHE=1)" >&2
        fi
        ;;
esac

# An env-less cargo build lands in the fallback base ~/.cargo/build (the
# tracked .cargo/config.toml's build-dir template). The wrapper is the one
# door every such compile passes, so it names the offender: once per cargo,
# one tab-separated line in ~/.fno/logs/cargo-fallback-writers.log naming
# the cargo pid and argv, the parent pid and argv, the cwd and the manifest
# dir. The compiler always runs.
name_fallback_writer() {
    if [[ -n "${CARGO_BUILD_BUILD_DIR:-}" || -n "${CI:-}" ]]; then
        return 0
    fi
    # Same once-per-cargo shape as the unadmitted marker above: fresh under
    # 60 minutes, so a marker from an earlier cargo with this pid goes stale.
    # The noclobber create is the once-per-cargo gate: cargo runs rustc calls
    # in parallel, so the exists-then-create test alone would let two of them
    # both log.
    local marker="${TMPDIR:-/tmp}/fno-build-fallback.$PPID"
    if [[ -e "$marker" ]]; then
        if [[ -n "$(find "$marker" -mmin -60 2>/dev/null)" ]]; then
            return 0
        fi
        rm -f "$marker" 2>/dev/null || true
    fi
    if ! ( set -o noclobber; : >"$marker" ) 2>/dev/null; then
        return 0
    fi
    {
        local log="$STATE_DIR/logs/cargo-fallback-writers.log"
        mkdir -p "$(dirname "$log")"
        local cargo_argv parent_pid parent_argv
        cargo_argv="$(ps -o command= -p "$PPID" 2>/dev/null || true)"
        parent_pid="$(ps -o ppid= -p "$PPID" 2>/dev/null | tr -d ' ')"
        parent_argv="$(ps -o command= -p "${parent_pid:-0}" 2>/dev/null || true)"
        printf '%s\tcargo_pid=%s\tcargo=%s\tparent=%s %s\tcwd=%s\tmanifest_dir=%s\n' \
            "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$PPID" "$cargo_argv" \
            "${parent_pid:-0}" "$parent_argv" "$PWD" "${CARGO_MANIFEST_DIR:-}" >>"$log"
        if [[ "$(wc -l <"$log")" -gt 1000 ]]; then
            tail -n 500 "$log" >"$log.tmp" && mv "$log.tmp" "$log"
        fi
    } 2>/dev/null || true
    echo 'cargo-rustc-wrapper: CARGO_BUILD_BUILD_DIR is unset, so this build lands in the fallback base ~/.cargo/build; logged to ~/.fno/logs/cargo-fallback-writers.log. To use the fno base in this shell: export CARGO_BUILD_BUILD_DIR="$(fno config build-dir)"' >&2
}

case " $* " in
    *" -vV "* | *" --print"*) ;;
    *)
        # Cargo sets CARGO_CFG_* only when it runs a build script, so a
        # rustc started by a build script (thiserror's or autocfg's probe)
        # sees the variable and a rustc started by cargo does not. That
        # cargo is already admitted; a probe that asked again waited on it
        # for 1h49m on 2026-09-23.
        [[ -n "${CARGO_CFG_TARGET_ARCH:-}" ]] || admit build
        name_fallback_writer
        ;;
esac

if [[ "$HAS_SCCACHE" -eq 1 ]]; then
    export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-30G}"
    # 0 stops the server exiting on idle mid-build, which fell compiles back
    # to local rustc under fleet load. The daemon sets the same default; this
    # keeps an operator's shorter override working.
    export SCCACHE_IDLE_TIMEOUT="${SCCACHE_IDLE_TIMEOUT:-0}"
    # The fleet cache lives under the build-dir base so one reclaim lane owns
    # the whole tree. fill_sccache_env sets it first for fno-spawned
    # processes; this default covers every other footnote build. fno never
    # exports it machine-wide, so other Rust projects keep their own cache.
    if [[ -z "${SCCACHE_DIR:-}" ]]; then
        base="${FNO_CARGO_TARGETS_BASE:-$STATE_DIR/cargo-build}"
        case "$base" in
            "~"*) base="$HOME${base:1}" ;;
        esac
        export SCCACHE_DIR="$base/sccache"
    fi
    # sccache hashes every CARGO_* var but rustc never reads these three,
    # and fno doctor test sets a new build dir per run, which would split
    # the cache key per worktree.
    unset CARGO_BUILD_BUILD_DIR CARGO_BUILD_TARGET_DIR CARGO_TARGET_DIR
    # A wedged server leaves this client parked at 0 percent CPU for hours
    # while holding the build-dir lock. The bound ends the wait and the
    # compile runs on bare rustc; the daemon's machine-watch tick restarts the
    # server. 0 turns the bound off.
    client_bound="${FNO_SCCACHE_CLIENT_TIMEOUT_SECS:-3600}"
    if [[ "$client_bound" != "0" ]]; then
        # shellcheck source=scripts/lib/with-timeout.sh
        source "$REPO_ROOT/scripts/lib/with-timeout.sh"
        sccache_rc=0
        with_timeout "$client_bound" sccache "$@" || sccache_rc=$?
        if [[ "$sccache_rc" -ne 124 ]]; then
            exit "$sccache_rc"
        fi
        echo "cargo-rustc-wrapper: sccache gave no answer in ${client_bound}s; compiling with bare rustc" >&2
    else
        exec sccache "$@"
    fi
fi

compiler="$1"
shift
exec "$compiler" "$@"
