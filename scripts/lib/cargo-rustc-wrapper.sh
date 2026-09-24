#!/usr/bin/env bash
set -euo pipefail

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
    # A failed admission is said once per cargo, not once per crate. A
    # marker older than an hour belongs to an earlier cargo with this pid.
    unadmitted="${TMPDIR:-/tmp}/fno-build-unadmitted.$PPID"
    if [[ -e "$unadmitted" && -n "$(find "$unadmitted" -mmin -60 2>/dev/null)" ]]; then
        return 0
    elif command -v fno-agents >/dev/null 2>&1; then
        repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
        rc=0
        fno-agents test-run ${mode}-admit --cargo-pid "$PPID" --worktree "$repo_root" || rc=$?
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
            usage="$(fno-agents test-run ${mode}-admit 2>&1 || true)"
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
    admit run
    exec "$@"
fi

# sccache was installed 2026-08-19. Before that this wrapper fell through
# to bare rustc silently, and every worktree compiled cold. Until 2026-08-20
# it was installed-without-effect too: Cargo decides incremental before this
# script ever runs, so `.cargo/config.toml`'s [build] incremental = false
# is what actually enables caching, not anything in this wrapper.
if command -v sccache >/dev/null 2>&1; then
    HAS_SCCACHE=1
else
    HAS_SCCACHE=0
fi

case " $* " in
    *" -vV "*)
        if [[ "$HAS_SCCACHE" -eq 1 ]]; then
            echo "cargo-rustc-wrapper: sccache (shared cache)" >&2
        else
            echo "cargo-rustc-wrapper: bare rustc (sccache absent)" >&2
        fi
        ;;
esac

case " $* " in
    *" -vV "* | *" --print"*) ;;
    *)
        # Cargo sets CARGO_CFG_* only when it runs a build script, so a
        # rustc started by a build script (thiserror's or autocfg's probe)
        # sees the variable and a rustc started by cargo does not. That
        # cargo is already admitted; a probe that asked again waited on it
        # for 1h49m on 2026-09-23.
        [[ -n "${CARGO_CFG_TARGET_ARCH:-}" ]] || admit build
        ;;
esac

if [[ "$HAS_SCCACHE" -eq 1 ]]; then
    export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-30G}"
    # sccache hashes every CARGO_* var but rustc never reads these three,
    # and fno doctor test sets a new build dir per run, which would split
    # the cache key per worktree.
    unset CARGO_BUILD_BUILD_DIR CARGO_BUILD_TARGET_DIR CARGO_TARGET_DIR
    exec sccache "$@"
fi

compiler="$1"
shift
exec "$compiler" "$@"
