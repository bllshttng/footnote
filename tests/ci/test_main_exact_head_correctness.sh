#!/usr/bin/env bash
# tests/ci/test_main_exact_head_correctness.sh
#
# Contract for the two external-correctness capabilities that must run on
# every pull request and every main head. This intentionally does not enforce
# parity with the historical PR check count: PR-only metadata, formatting,
# packaging, and harness checks remain deliberately scoped to PRs or releases.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

python3 - <<'PY'
import re
import sys

try:
    import yaml
except ImportError:
    sys.exit("SKIP-AS-FAIL: PyYAML not installed (pip install pyyaml); "
             "this workflow contract must not silently pass")


def load(path):
    with open(path) as handle:
        return yaml.safe_load(handle)


def event_map(workflow):
    # PyYAML 1.1 parses the YAML 1.2 key `on` as True.
    return workflow.get("on", workflow.get(True, {})) or {}


fails = []


def check(condition, message):
    if condition:
        print(f"  ok: {message}")
    else:
        print(f"  FAIL: {message}")
        fails.append(message)


guards = load(".github/workflows/guards.yml")
guards_events = event_map(guards)
guards_jobs = guards["jobs"]
guards_pull = guards_events.get("pull_request") or {}
guards_push = guards_events.get("push") or {}

check("pull_request" in guards_events and "paths" not in guards_pull,
      "guards pull_request is unfiltered")
check(guards_push.get("branches") == ["main"] and "paths" not in guards_push,
      "guards push runs on every main head without paths")
check("guards-pr" in guards_jobs and
      guards_jobs["guards-pr"].get("if") == "github.event_name == 'pull_request'",
      "guards-pr remains PR-only")

static = guards_jobs.get("main-python-static")
check(static is not None, "named Python static-correctness job exists")
if static is not None:
    check(static.get("name") == "Python static correctness (495 sources)",
          "Python static-correctness check has its visible name")
    check(static.get("timeout-minutes") == 5,
          "Python static-correctness job has a five-minute timeout")
    steps = static.get("steps") or []
    setup = [step for step in steps if step.get("uses") == "./.github/actions/guards-setup"]
    check(bool(setup), "Python static-correctness job uses guards-setup")
    run = "\n".join(str(step.get("run", "")) for step in steps)
    check("bash scripts/ci/check-python-static.sh" in run,
          "Python static-correctness runs the one shared static script")
    check(static.get("if") in (None, ""),
          "Python static-correctness is eligible on PR and main events")

# The pinned commands live in the shared script CI, fno doctor test, and the
# merge-result probe all run; pin them there.
script = open("scripts/ci/check-python-static.sh").read()
check("set -euo pipefail" in script,
      "static script stops before its success marker on failure")
check("${RUFF:-uv run ruff} check --no-respect-gitignore --color=never --output-format=concise src/" in script,
      "static script runs the exact Ruff command")
check("${MYPY:-uv run mypy} --no-color-output src/" in script,
      "static script runs the exact MyPy command")
ruff_at = script.index("${RUFF:-uv run ruff} check") if "${RUFF:-uv run ruff} check" in script else -1
mypy_at = script.index("${MYPY:-uv run mypy}") if "${MYPY:-uv run mypy}" in script else -1
marker = "python-static: checked"
marker_at = script.index(marker) if marker in script else -1
count_guard_at = script.index('test "$python_files" -gt 0') if 'test "$python_files" -gt 0' in script else -1
check(ruff_at >= 0 and mypy_at > ruff_at and marker_at > mypy_at,
      "Python success marker follows Ruff and MyPy")
check(bool(re.search(r"find\s+src\b.*-name ['\"]\*\.py['\"]", script)) and
      count_guard_at >= 0 and count_guard_at < ruff_at,
      "static script requires a positive Python-file count")

rust = load(".github/workflows/rust-ci.yml")
rust_events = event_map(rust)
rust_jobs = rust["jobs"]
rust_push = rust_events.get("push") or {}
rust_pull = rust_events.get("pull_request") or {}
check(rust_push.get("branches") == ["main"] and "paths" not in rust_push,
      "rust-ci push runs on every main head without paths")
check("paths" in rust_pull and rust_pull["paths"],
      "rust-ci pull_request keeps its existing path filter")
for name in ("audit",):
    check(name in rust_jobs and rust_jobs[name].get("if") in (None, ""),
          f"rust-ci {name} is not PR-only")
check(rust_jobs.get("fmt", {}).get("if") == "github.event_name == 'pull_request'",
      "rust-ci pinned formatting is explicitly PR-only")

cli = load(".github/workflows/cli-ci.yml")
cli_events = event_map(cli)
cli_push = cli_events.get("push") or {}
check(cli_push.get("branches") == ["main"] and cli_push.get("paths"),
      "cli-ci remains scoped to relevant main pushes")
check(bool(cli_events.get("schedule")),
      "cli-ci schedules a full run even when main is quiet")
cli_jobs = cli["jobs"]
for name in ("smoke-pytest", "smoke-rest"):
    check(cli_jobs.get(name, {}).get("if") ==
          "needs.pr-affected.outputs.python_full == 'true'",
          f"cli-ci {name} is eligible when the non-PR selector says full")
for name in ("test-agents", "test-agents-integration", "test-mux"):
    check(cli_jobs.get(name, {}).get("if") ==
          "needs.pr-affected.outputs.cargo == 'true'",
          f"cli-ci {name} is eligible when the non-PR selector says full")

publish = load(".github/workflows/crates-publish.yml")
publish_jobs = publish["jobs"]
check(publish_jobs.get("dry-run", {}).get("if") == "github.event_name == 'pull_request'",
      "crate dry-runs remain PR-only")
check("publish" not in publish_jobs,
      "crates-publish.yml carries no publish job; publishing lives in release.yml")

# release.yml is the one release workflow: its publish job is the single
# approval click (the release environment) and only ever runs for rc/stable;
# the nightly publishes with no approval and no v* tag.
release = load(".github/workflows/release.yml")
release_events = event_map(release)
release_jobs = release["jobs"]
release_dispatch = (release_events.get("workflow_dispatch") or {}).get("inputs") or {}
check(sorted((release_dispatch.get("channel") or {}).get("options") or []) == ["nightly", "rc", "stable"],
      "release.yml dispatches a channel among nightly, rc and stable")
check("schedule" in release_events,
      "release.yml runs the nightly on a schedule")
check((release_dispatch.get("dry_run") or {}).get("type") == "boolean",
      "release.yml carries the dry_run rehearsal input")
publish_job = release_jobs.get("publish") or {}
check(publish_job.get("environment") == "release",
      "release.yml's publish job sits behind the release approval environment")
check((publish_job.get("needs") or []) == ["resolve", "binaries", "wheels"],
      "release.yml's publish job runs after resolve and both build workflows")
publish_if = str(publish_job.get("if", ""))
check("(inputs.channel || 'nightly') != 'nightly'" in publish_if,
      "release.yml's publish job runs only for rc or stable")
nightly_if = str((release_jobs.get("publish-nightly") or {}).get("if", ""))
check("(inputs.channel || 'nightly') == 'nightly'" in nightly_if,
      "release.yml's nightly job runs only for the nightly channel")
check(str((release.get("concurrency") or {}).get("group", "")) == "release-${{ inputs.channel || 'nightly' }}",
      "release.yml serializes per channel so an approval wait never parks the nightly")

# Build workflows are callable and build-only: release.yml calls them and owns
# every publish leg behind the release-environment approval.
for wf_name in ("release-binaries.yml", "release-wheels.yml"):
    wf = load(f".github/workflows/{wf_name}")
    wf_events = event_map(wf)
    call_inputs = (wf_events.get("workflow_call") or {}).get("inputs") or {}
    check("version" in call_inputs and "ref" in call_inputs,
          f"{wf_name} is callable with version and ref inputs")
    check("push" not in wf_events,
          f"{wf_name} never fires on a tag or branch push")
    wf_jobs = wf["jobs"]
    check(not any(j in wf_jobs for j in ("release", "publish-pypi", "update-homebrew-tap", "publish")),
          f"{wf_name} carries no publish leg; publishing lives in release.yml")

if fails:
    print(f"{len(fails)} workflow contract assertion(s) failed", file=sys.stderr)
    sys.exit(1)
print("test_main_exact_head_correctness: ALL PASS")
PY
