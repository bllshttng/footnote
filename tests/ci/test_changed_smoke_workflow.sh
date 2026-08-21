#!/usr/bin/env bash
# tests/ci/test_changed_smoke_workflow.sh
#
# Structural guard over the two-job CI shape: the early changed-packet job and
# the canonical full smoke job must start together and stay independent.
#
# These are the invariants a reviewer cannot hold in their head across a yml
# refactor: no dependency edge in either direction (an edge would make partial
# feedback cost merge latency, which is the reason it exists), the changed job
# never writes or reuses FULL evidence, and the full smoke job keeps its exact
# unsharded command - x-b0e8 defers sharding until the changed-packet
# measurements say final merge latency, not first feedback, is the bottleneck.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

WF=".github/workflows/cli-ci.yml"
[[ -f "$WF" ]] || { echo "FAIL: $WF missing"; exit 1; }

python3 - "$WF" <<'PY'
import sys

try:
    import yaml
except ImportError:
    sys.exit("SKIP-AS-FAIL: PyYAML not installed (pip install pyyaml); "
             "this guard must not silently pass")

wf = yaml.safe_load(open(sys.argv[1]))
jobs = wf["jobs"]
fails = []


def ok(msg):
    print(f"  ok: {msg}")


def check(cond, good, bad):
    ok(good) if cond else (print(f"  FAIL: {bad}"), fails.append(bad))


check("changed-smoke" in jobs, "changed-smoke job exists",
      "no changed-smoke job")
check("smoke" in jobs, "canonical smoke job still exists",
      "the full smoke job disappeared")
if fails:
    sys.exit(1)

changed, smoke = jobs["changed-smoke"], jobs["smoke"]

# --- AC8: started together, no dependency edge either way -------------------
check("needs" not in changed, "changed-smoke has no needs (starts immediately)",
      f"changed-smoke waits on {changed.get('needs')!r} - that delays first feedback")
check("needs" not in smoke, "smoke has no needs (changed packet never gates it)",
      f"smoke waits on {smoke.get('needs')!r} - a partial job now delays the merge gate")

changed_run = "\n".join(s.get("run", "") for s in changed["steps"])
smoke_run = "\n".join(s.get("run", "") for s in smoke["steps"])

# --- the changed job runs the packet, with explicit revisions ---------------
check("--changed" in changed_run, "changed-smoke invokes the --changed packet",
      "changed-smoke does not invoke --changed")
check("--base" in changed_run and "--head" in changed_run,
      "changed-smoke pins explicit base/head revisions",
      "changed-smoke relies on a mutable ref instead of explicit base/head")
check(changed.get("steps", [{}])[0].get("with", {}).get("fetch-depth") == 0,
      "changed-smoke fetches full history (the base must resolve)",
      "changed-smoke uses a shallow checkout - the base cannot resolve")

# --- the changed job may not produce FULL evidence --------------------------
for forbidden, why in (
    ("preflight-last-failures", "write the full runner's failure record"),
    ("preflight-attestation", "touch the FULL attestation"),
    ("mode=FULL", "claim FULL evidence"),
):
    check(forbidden not in changed_run, f"changed-smoke does not {why}",
          f"changed-smoke may {why}")
# A bare `test smoke` with no subset flag in the changed job would be a second
# full run wearing the partial job's label.
bare_full = any(
    "fno-py doctor test smoke" in line and "--changed" not in line
    for line in changed_run.splitlines()
)
check(not bare_full, "changed-smoke never runs an unlabelled full smoke",
      "changed-smoke runs a full smoke under the partial job's name")

# --- the merge gate is unchanged and unsharded ------------------------------
check("uv run --project cli fno-py doctor test smoke" in smoke_run,
      "smoke keeps the canonical full runner invocation",
      "the full smoke command changed")
# Every subset flag, not just --changed: swapping the gate to --only or
# --retry-failed narrows it exactly as much and must red here too.
narrowed = [f for f in ("--changed", "--only", "--retry-failed") if f in smoke_run]
check(not narrowed, "smoke runs no subset mode",
      f"the merge gate was narrowed to a subset ({', '.join(narrowed)})")
# Sharding the full suite is allowed. What must never happen is a single shard
# standing in for the whole gate: with a matrix, each shard reports its own
# check, so branch protection pointed at one of them would pass on a fraction of
# the suite. If smoke is sharded, some job must depend on it and aggregate.
if "strategy" in smoke:
    aggregators = [
        name for name, job in jobs.items()
        if name != "smoke" and "smoke" in (
            [job["needs"]] if isinstance(job.get("needs"), str) else job.get("needs") or []
        )
    ]
    check(bool(aggregators),
          f"sharded smoke has an aggregating gate job ({', '.join(aggregators)})",
          "smoke is sharded with no job depending on it - branch protection would "
          "then gate on a single shard, i.e. a fraction of the suite")
else:
    ok("smoke is unsharded (a single job is its own gate)")

sys.exit(1 if fails else 0)
PY
rc=$?

echo ""
if [[ $rc -eq 0 ]]; then echo "test_changed_smoke_workflow: ALL PASS"; else
    echo "test_changed_smoke_workflow: FAILED"; fi
exit $rc
