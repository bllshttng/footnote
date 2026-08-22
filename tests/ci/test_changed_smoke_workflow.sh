#!/usr/bin/env bash
# tests/ci/test_changed_smoke_workflow.sh
#
# Structural guard over the two-job CI shape: the early changed-packet job and
# the canonical full smoke job must start together and stay independent.
#
# These are the invariants a reviewer cannot hold in their head across a yml
# refactor: the changed packet never gates the merge gate (an edge would make
# partial feedback cost merge latency, which is the reason it exists), the
# changed job never writes or reuses FULL evidence, and the merge gate still
# covers the whole suite.
#
# The gate IS sharded now. Sharding was deferred until measurement said final
# merge latency was the bottleneck, and it did: one job ran 2095s, half of it
# a single pytest step and half a tail serial behind it. So `smoke` no longer
# runs the suite itself; it aggregates the shard jobs that do. The protection
# that mattered is unchanged and is asserted below: no shard may stand in for
# the whole gate, and the aggregator must require EVERY shard to pass.
#
# What this guard cannot see is whether the shard selectors still COVER the
# registry. cli/tests/unit/test_smoke_shards.py owns that, reading the same
# selectors out of this workflow.

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
def _needs(job):
    n = job.get("needs")
    return [n] if isinstance(n, str) else list(n or [])


def _reaches(start, target, seen=None):
    """True when `start` depends on `target`, directly or through any chain."""
    seen = seen or set()
    for dep in _needs(jobs.get(start, {})):
        if dep == target or (dep not in seen and _reaches(dep, target, seen | {dep})):
            return True
    return False


# The gate may depend on its own shards. It may never depend on the changed
# packet: that is what would make partial feedback cost merge latency, which
# is the whole reason the changed job exists.
check(not _reaches("smoke", "changed-smoke"),
      "changed packet never gates the merge gate",
      "smoke depends on changed-smoke - partial feedback now delays the merge gate")

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

# --- the merge gate still covers the whole suite ----------------------------
# `smoke` is the required check. It either runs the suite itself, or it
# aggregates the jobs that do. Resolve which, then assert against THOSE jobs -
# the old checks read smoke's own run block, and once the work moved to the
# shards they kept printing ok while asserting nothing about what CI ran.
gate_shards = _needs(smoke)
if gate_shards:
    runner_jobs = {n: jobs[n] for n in gate_shards if n in jobs}
    check(len(runner_jobs) == len(gate_shards),
          "every job the gate needs exists",
          f"smoke needs {sorted(set(gate_shards) - set(runner_jobs))}, which do not exist")
else:
    runner_jobs = {"smoke": smoke}

for name, job in sorted(runner_jobs.items()):
    run = "\n".join(st.get("run", "") for st in job.get("steps", []))
    check("uv run --project cli fno-py doctor test smoke" in run,
          f"{name} runs the canonical full runner invocation",
          f"{name} does not invoke the canonical smoke runner")
    # --only / --skip are how the shards divide the suite, and their COVERAGE
    # is owned by cli/tests/unit/test_smoke_shards.py. These two are different:
    # both select a subset nobody declared, so a shard using either silently
    # shrinks the merge gate to whatever happened to change or fail last.
    narrowed = [f for f in ("--changed", "--retry-failed") if f in run]
    check(not narrowed, f"{name} runs no undeclared subset mode",
          f"the merge gate was narrowed to a subset ({', '.join(narrowed)}) in {name}")

# With shards, each reports its own check, so branch protection pointed at one
# of them would pass on a fraction of the suite. The aggregator is what stops
# that, and only if it requires EVERY shard. Assert the positive marker per
# shard: a gate that names three shards and checks two reads exactly like one
# that checks all three.
if gate_shards:
    gate_run = "\n".join(st.get("run", "") for st in smoke.get("steps", []))
    for name in sorted(gate_shards):
        marker = "needs." + name + ".result"
        # Both on the SAME line. The gate echoes every shard result for the
        # log, so marker-anywhere plus success-anywhere is satisfied by one
        # shard's assertion plus another shard's echo, and deleting a real
        # assertion still read as ok.
        asserted = any(marker in ln and "success" in ln for ln in gate_run.splitlines())
        check(asserted,
              f"the gate requires {name} to succeed",
              f"the gate never asserts {name} succeeded - that shard could fail unnoticed")
    # A skipped required check is not a red one, so the gate has to run even
    # when a shard fails.
    check(str(smoke.get("if", "")).strip() == "always()",
          "the gate runs even when a shard fails (if: always())",
          f"smoke has if: {smoke.get('if')!r} - GitHub SKIPS it when a shard fails, "
          "and a skipped required check does not block")

sys.exit(1 if fails else 0)
PY
rc=$?

echo ""
if [[ $rc -eq 0 ]]; then echo "test_changed_smoke_workflow: ALL PASS"; else
    echo "test_changed_smoke_workflow: FAILED"; fi
exit $rc
