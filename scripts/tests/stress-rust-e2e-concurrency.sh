#!/usr/bin/env bash
# Run process-backed Rust e2e binaries as independent, non-retrying trials.
# Cargo builds once; each trial executes immutable binaries in parallel so
# Cargo artifact-lock contention cannot be counted as a test red.
set -uo pipefail

trials="${STRESS_TRIALS:-${FNO_STRESS_TRIALS:-20}}"
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

# A dirty tree cannot be pinned to a sha, so the trials would measure something
# nobody can name. That is a reason to stand down, NOT a reason to go red: this
# file is auto-discovered by `fno doctor test smoke` (the scripts/tests/*.sh
# glob), which reads any non-zero exit as a failure, so refusing here turned an
# ordinary edit-in-progress into a red local suite.
#
# Standing down is only safe because the CI job that owns the 20-trial contract
# asserts the stress_summary line rather than the exit code. A checkout there is
# never dirty, and if one ever were, the missing summary fails the job instead
# of passing quietly.
if [[ -n "$(git status --porcelain)" ]]; then
    echo "stress_skipped=working-tree-not-clean sha=unpinnable"
    exit 0
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
    local candidate
    if ! cargo test --manifest-path "$manifest" --test "$test_name" --no-run --message-format=json >>"$build_log" 2>&1; then
        echo "stress_setup=unavailable binary=$key log=$build_log" >&2
        if grep -Eq 'Blocking waiting for file lock|could not acquire.*lock|resource temporarily unavailable' "$build_log"; then
            echo "stress_setup_note=cargo-artifact-lock-contention (not counted as a test failure)" >&2
        fi
        exit 2
    fi
    candidate="$(jq -Rr --arg test_name "$test_name" 'fromjson? | select(.reason == "compiler-artifact" and .target.name == $test_name and (.executable // null) != null) | .executable' "$build_log" | tail -1)"
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

# Count the daemons this checkout's binaries left running. A process-backed e2e
# test that spawns a daemon and never reaps it leaves one alive with a 3600s
# idle exit, so it outlives the whole run; twenty trials turn a two-process miss
# into forty live daemons on one machine, which is the pid-exhaustion shape this
# suite exists to close. The pattern is the built binary path, so an operator's
# own installed daemon is never counted.
#
# Every trial line carries daemons_left, printed whether it is zero or not. That
# number is the positive marker that the check ran: a trial line without the
# field means the instrument is missing, which reads differently from a clean
# zero.
# Derived from the built binary, never assumed. A hardcoded target/debug path
# reads 0 under --release or any CARGO_TARGET_DIR override, and "the instrument
# found nothing" would then be indistinguishable from "nothing leaked" - the
# absence-is-not-evidence failure this field exists to avoid. So the path is
# resolved and its existence is REQUIRED: a missing binary refuses the run
# rather than reporting a clean zero.
# daemon_bin is the compiled TEST binary, which cargo puts in deps/; the
# daemon it spawns sits one level up beside it.
daemon_bin_path="$(dirname "$(dirname "$daemon_bin")")/fno-agents-daemon"
if [[ ! -x "$daemon_bin_path" ]]; then
    echo "stress_setup=unavailable reason=daemon-binary-not-found path=$daemon_bin_path" >&2
    echo "stress_setup_note=the leak counter cannot report a trustworthy zero without it" >&2
    exit 2
fi
daemon_pattern="$daemon_bin_path"
count_daemons() {
    pgrep -f "$daemon_pattern" 2>/dev/null | wc -l | tr -d ' '
}
daemon_baseline="$(count_daemons)"

# The two marker greps below are vacuity guards: a binary can exit 0 because
# the probe that carries the proof never ran. They also mean this harness
# CANNOT be pointed at an older revision to produce a baseline, because both
# markers come from probe tests that do not exist there - every trial reads
# `fail` for a binary whose own output says `test result: ok`. Read the trial
# tails, not the verdicts, when comparing across revisions.
echo "stress_setup=ready sha=$head_sha parallel_binaries=3 per_binary_threads=1 trials=$trials daemon_baseline=$daemon_baseline logs=$log_root"
failures=0
for ((trial = 1; trial <= trials; trial++)); do
    if [[ "$(git rev-parse HEAD)" != "$head_sha" ]]; then
        echo "stress_aborted reason=head-changed expected=$head_sha actual=$(git rev-parse HEAD)" >&2
        exit 2
    fi

    trial_dir="$log_root/trial-$(printf '%02d' "$trial")"
    mkdir -p "$trial_dir"
    # Re-sampled per trial. Nothing here reaps a leaked daemon, so one leak
    # measured against a run-wide baseline would mark every LATER trial failed
    # too and inflate the failure count this job is judged on. Each trial is
    # asked only what it added.
    trial_daemon_baseline="$(count_daemons)"
    run_binary() {
        local key="$1"
        local bin
        case "$key" in
            daemon_e2e) bin="$daemon_bin" ;;
            persistence) bin="$persistence_bin" ;;
            workspace_persistence_e2e) bin="$workspace_bin" ;;
        esac
        "$bin" --nocapture --test-threads=1 >"$trial_dir/$key.log" 2>&1
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
    [[ "$daemon_rc" == 0 ]] && grep -Eq '^daemon_child_env_isolated ' "$trial_dir/daemon_e2e.log" || daemon_verdict=fail
    [[ "$persistence_rc" == 0 ]] || persistence_verdict=fail
    [[ "$workspace_rc" == 0 ]] && grep -Eq 'old_server_reaped_before_rebind old_pid=' "$trial_dir/workspace_persistence_e2e.log" || workspace_verdict=fail

    daemons_left=$(($(count_daemons) - trial_daemon_baseline))
    ((daemons_left < 0)) && daemons_left=0
    leak_verdict=pass
    ((daemons_left > 0)) && leak_verdict=fail

    if [[ "$daemon_verdict" != pass || "$persistence_verdict" != pass || "$workspace_verdict" != pass || "$leak_verdict" != pass ]]; then
        failures=$((failures + 1))
        # Print the failing binary's own tail. The trial logs live in a runner
        # tmpdir that no artifact upload collects, so on CI the summary line was
        # the ONLY evidence a trial produced: "persistence=fail" named the
        # binary and nothing else, and the run that produced it is gone. A
        # 1-in-20 failure is the whole reason this harness exists; it has to
        # arrive diagnosable the first time.
        for key in daemon_e2e persistence workspace_persistence_e2e; do
            case "$key" in
                daemon_e2e) verdict="$daemon_verdict" ;;
                persistence) verdict="$persistence_verdict" ;;
                workspace_persistence_e2e) verdict="$workspace_verdict" ;;
            esac
            [[ "$verdict" == pass ]] && continue
            echo "--- stress_trial_failure trial=$trial binary=$key ---" >&2
            tail -160 "$trial_dir/$key.log" >&2
            echo "--- end stress_trial_failure trial=$trial binary=$key ---" >&2
        done
    fi
    echo "stress_trial trial=$trial sha=$head_sha daemon_e2e=$daemon_verdict persistence=$persistence_verdict workspace_persistence_e2e=$workspace_verdict daemons_left=$daemons_left logs=$trial_dir"
done

echo "stress_summary trials=$trials failures=$failures sha=$head_sha logs=$log_root"
if ((failures > 0)); then
    exit 1
fi
