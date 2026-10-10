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

# sccache was installed 2026-08-19. Before that this wrapper fell through
# to bare rustc silently, and every worktree compiled cold. Until 2026-08-20
# it was installed-without-effect too: Cargo decides incremental before this
# script ever runs, so `.cargo/config.toml`'s [build] incremental = false
# is what actually enables caching, not anything in this wrapper.
# An uncached retry still passes build and run admission below.
if [[ "${SCCACHE_DISABLE:-0}" != "1" ]] && command -v sccache >/dev/null 2>&1; then
    HAS_SCCACHE=1
else
    HAS_SCCACHE=0
fi

case " $* " in
    *" -vV "*)
        if [[ "$HAS_SCCACHE" -eq 1 ]]; then
            echo "cargo-rustc-wrapper: sccache (shared cache)" >&2
        else
            echo "cargo-rustc-wrapper: bare rustc (sccache absent or disabled)" >&2
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
    # A wedged server leaves this client parked at 0 percent CPU while it
    # holds the build-dir lock: 5h30m on 2026-10-08, 38 minutes inside an fno
    # update on 2026-10-09. The client always sits at 0 percent, so its CPU
    # says nothing. A live compile always shows as a child of the server, so
    # the watcher reads that instead. 0 turns the watcher off.
    client_bound="${FNO_SCCACHE_CLIENT_TIMEOUT_SECS:-300}"
    stall_bound="${FNO_SCCACHE_STALL_SECS:-60}"
    [[ "$client_bound" =~ ^[0-9]+$ ]] || client_bound=300
    [[ "$stall_bound" =~ ^[0-9]+$ ]] || stall_bound=60
    if [[ "$client_bound" == "0" ]]; then
        exec sccache "$@"
    fi
    # The extra-filename hash names this one compile in the server's child
    # argv, so the watcher can tell our rustc from another client's.
    token=""
    for arg in "$@"; do
        case "$arg" in
            *extra-filename=*) token="extra-filename=${arg#*extra-filename=}" ;;
        esac
    done
    cargo_pid="$PPID"
    verdict="$(mktemp "${TMPDIR:-/tmp}/fno-sccache-watch.XXXXXX")"

    # One line in $verdict when the watcher stops the client:
    #   orphan       - cargo is gone; our server-side rustc is stopped too
    #   stall <pid>  - server <pid> ran no compile for stall_bound seconds
    #   bound        - our compile never started within client_bound seconds
    # A long compile that is running is never stopped: the bound only counts
    # while the server runs no rustc for this crate.
    watch_client() {
        local client="$1" poll waited=0 quiet=0 server kids ours
        poll=$(( stall_bound >= 12 ? stall_bound / 12 : 1 ))
        while sleep "$poll"; do
            waited=$((waited + poll))
            read -r server kids ours < <(ps -A -ww -o pid=,ppid=,command= 2>/dev/null | awk -v token="$token" '
                {
                    pid[NR] = $1; ppid[NR] = $2
                    cmd = $0; sub(/^ *[0-9]+ +[0-9]+ */, "", cmd); sub(/ +$/, "", cmd); line[NR] = cmd
                    if (server == "" && (cmd == "(sccache)" || cmd ~ /sccache --start-server$/ || (cmd ~ /(^|\/)sccache$/ && cmd !~ /[ \t]/))) server = $1
                }
                END {
                    kids = 0; ours = 0
                    if (server != "") for (i = 1; i <= NR; i++) if (ppid[i] == server) {
                        kids++
                        if (token != "" && index(line[i], token)) ours = pid[i]
                    }
                    print (server == "" ? 0 : server), kids, ours
                }')
            if ! kill -0 "$cargo_pid" 2>/dev/null; then
                echo orphan >"$verdict"
                if [[ "$ours" != "0" ]]; then
                    kill -TERM "$ours" 2>/dev/null || true
                fi
                break
            fi
            if [[ "$kids" -gt 0 ]]; then
                quiet=0
            else
                quiet=$((quiet + poll))
            fi
            if [[ "$stall_bound" != "0" && "$quiet" -ge "$stall_bound" ]]; then
                echo "stall $server" >"$verdict"
                break
            fi
            if [[ "$ours" == "0" && "$waited" -ge "$client_bound" ]]; then
                echo bound >"$verdict"
                break
            fi
        done
        kill -TERM -"$client" 2>/dev/null || kill -TERM "$client" 2>/dev/null || true
        # A wrapper stopped with its cargo never reads the verdict.
        if ! kill -0 "$$" 2>/dev/null; then
            rm -f "$verdict"
        fi
    }

    # set -m gives the client and the watcher each a process group, so a
    # stop reaches the whole client. The caller's stdin passes through.
    set -m
    sccache "$@" <&0 &
    client=$!
    watch_client "$client" >/dev/null 2>&1 &
    watcher=$!
    set +m
    sccache_rc=0
    wait "$client" 2>/dev/null || sccache_rc=$?
    kill -TERM -"$watcher" 2>/dev/null || kill -TERM "$watcher" 2>/dev/null || true
    wait "$watcher" 2>/dev/null || true
    reason="$(cat "$verdict" 2>/dev/null || true)"
    rm -f "$verdict"
    case "$reason" in
        "")
            exit "$sccache_rc"
            ;;
        orphan)
            echo "cargo-rustc-wrapper: cargo (pid $cargo_pid) is gone; stopped this compile" >&2
            exit 1
            ;;
        stall*)
            server="${reason#stall }"
            # The server ran no compile for the whole window, so stopping it
            # loses no work. One wrapper per server pid stops it; the next
            # client or the daemon's machine-watch tick starts a fresh one.
            if [[ "$server" != "0" ]] && ( set -o noclobber; : >"${TMPDIR:-/tmp}/fno-sccache-stopped.$server" ) 2>/dev/null; then
                kill -TERM "$server" 2>/dev/null || true
                sleep 1
                kill -KILL "$server" 2>/dev/null || true
            fi
            echo "cargo-rustc-wrapper: the sccache server ran no compile for ${stall_bound}s; stopped it, compiling with bare rustc" >&2
            ;;
        *)
            echo "cargo-rustc-wrapper: sccache did not start this compile in ${client_bound}s; compiling with bare rustc" >&2
            ;;
    esac
fi

compiler="$1"
shift
exec "$compiler" "$@"
