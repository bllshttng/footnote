#!/usr/bin/env bash
# Run process-backed Rust e2e binaries as independent, non-retrying trials.
# Cargo builds once; each trial executes immutable binaries in parallel so
# Cargo artifact-lock contention cannot be counted as a test red.
set -uo pipefail

trials=20
while (($# > 0)); do
    case "$1" in
        --trials)
            shift
            [[ "${1:-}" =~ ^[0-9]+$ ]] || {
                echo "usage: $0 [--trials 1..20]" >&2
                exit 2
            }
            trials="$1"
            ;;
        --help|-h)
            echo "usage: $0 [--trials 1..20]"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "usage: $0 [--trials 1..20]" >&2
            exit 2
            ;;
    esac
    shift
done

if ((trials < 1 || trials > 20)); then
    echo "--trials must be between 1 and 20" >&2
    exit 2
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "stress_setup=unavailable reason=not-a-git-checkout" >&2
    exit 2
}
cd "$repo_root" || exit 2

if [[ -n "$(git status --porcelain)" ]]; then
    echo "stress_setup=refused reason=working-tree-not-clean" >&2
    exit 2
fi

head_sha="$(git rev-parse HEAD)" || exit 2
head_short="${head_sha:0:12}"
log_root="$(mktemp -d "${TMPDIR:-/tmp}/fno-rust-e2e-concurrency-${head_short}.XXXXXX")" || {
    echo "stress_setup=unavailable reason=temporary-directory" >&2
    exit 2
}

daemon_bin=""
persistence_bin=""
workspace_bin=""
build_test_binary() {
    local key="$1" manifest="$2" test_name="$3"
    local build_log="$log_root/build-${key}.log"
    local target_dir candidate

    target_dir="$(cargo metadata --manifest-path "$manifest" --no-deps --format-version 1 2>"$build_log" | jq -r '.target_directory' 2>>"$build_log")" || {
        echo "stress_setup=unavailable binary=$key log=$build_log" >&2
        exit 2
    }
    if ! cargo test --manifest-path "$manifest" --test "$test_name" --no-run >>"$build_log" 2>&1; then
        echo "stress_setup=unavailable binary=$key log=$build_log" >&2
        if rg -q 'Blocking waiting for file lock|could not acquire.*lock|resource temporarily unavailable' "$build_log"; then
            echo "stress_setup_note=cargo-artifact-lock-contention (not counted as a test failure)" >&2
        fi
        exit 2
    fi
    candidate="$(find "$target_dir/debug/deps" -maxdepth 1 -type f -name "${test_name}-*" -perm -111 -print | sort | tail -1)"
    [[ -n "$candidate" && -x "$candidate" ]] || {
        echo "stress_setup=unavailable binary=$key reason=compiled-test-binary-not-found log=$build_log" >&2
        exit 2
    }
    case "$key" in
        daemon_e2e) daemon_bin="$candidate" ;;
        persistence) persistence_bin="$candidate" ;;
        workspace_persistence_e2e) workspace_bin="$candidate" ;;
    esac
}

build_test_binary daemon_e2e crates/fno-agents/Cargo.toml daemon_e2e
build_test_binary persistence crates/fno/Cargo.toml persistence
build_test_binary workspace_persistence_e2e crates/fno/Cargo.toml workspace_persistence_e2e

echo "stress_setup=ready sha=$head_sha parallel_binaries=3 trials=$trials logs=$log_root"
failures=0
for ((trial = 1; trial <= trials; trial++)); do
    if [[ "$(git rev-parse HEAD)" != "$head_sha" ]]; then
        echo "stress_aborted reason=head-changed expected=$head_sha actual=$(git rev-parse HEAD)" >&2
        exit 2
    fi

    trial_dir="$log_root/trial-$(printf '%02d' "$trial")"
    mkdir -p "$trial_dir"
    run_binary() {
        local key="$1"
        local bin
        case "$key" in
            daemon_e2e) bin="$daemon_bin" ;;
            persistence) bin="$persistence_bin" ;;
            workspace_persistence_e2e) bin="$workspace_bin" ;;
        esac
        "$bin" --nocapture --test-threads=16 >"$trial_dir/$key.log" 2>&1
        printf '%s\n' "$?" >"$trial_dir/$key.rc"
    }

    run_binary daemon_e2e & daemon_pid=$!
    run_binary persistence & persistence_pid=$!
    run_binary workspace_persistence_e2e & workspace_pid=$!
    wait "$daemon_pid"
    wait "$persistence_pid"
    wait "$workspace_pid"

    daemon_rc="$(<"$trial_dir/daemon_e2e.rc")"
    persistence_rc="$(<"$trial_dir/persistence.rc")"
    workspace_rc="$(<"$trial_dir/workspace_persistence_e2e.rc")"
    daemon_verdict=pass
    persistence_verdict=pass
    workspace_verdict=pass
    [[ "$daemon_rc" == 0 ]] && rg -q '^daemon_child_env_isolated ' "$trial_dir/daemon_e2e.log" || daemon_verdict=fail
    [[ "$persistence_rc" == 0 ]] || persistence_verdict=fail
    [[ "$workspace_rc" == 0 ]] && rg -q '^old_server_reaped_before_rebind ' "$trial_dir/workspace_persistence_e2e.log" || workspace_verdict=fail

    if [[ "$daemon_verdict" != pass || "$persistence_verdict" != pass || "$workspace_verdict" != pass ]]; then
        failures=$((failures + 1))
    fi
    echo "stress_trial trial=$trial sha=$head_sha daemon_e2e=$daemon_verdict persistence=$persistence_verdict workspace_persistence_e2e=$workspace_verdict logs=$trial_dir"
done

echo "stress_summary trials=$trials failures=$failures sha=$head_sha logs=$log_root"
if ((failures > 0)); then
    exit 1
fi
