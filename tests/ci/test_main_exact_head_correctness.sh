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
    check(any(step.get("working-directory") == "cli" for step in steps),
          "Python static-correctness commands run from cli")
    check("set -euo pipefail" in run,
          "Python static-correctness stops before its success marker on failure")
    check("uv run ruff check --no-respect-gitignore src/" in run,
          "Python static-correctness runs the exact Ruff command")
    check("uv run mypy src/" in run,
          "Python static-correctness runs the exact MyPy command")
    ruff_at = run.index("uv run ruff check --no-respect-gitignore src/") if "uv run ruff check --no-respect-gitignore src/" in run else -1
    mypy_at = run.index("uv run mypy src/") if "uv run mypy src/" in run else -1
    marker = "main-python-static: checked"
    marker_at = run.index(marker) if marker in run else -1
    count_guard_at = run.index('test "$python_files" -gt 0') if 'test "$python_files" -gt 0' in run else -1
    check(ruff_at >= 0 and mypy_at > ruff_at and marker_at > mypy_at,
          "Python success marker follows Ruff and MyPy")
    check(bool(re.search(r"find\s+src\b.*-name ['\"]\*\.py['\"]", run)) and
          count_guard_at >= 0 and count_guard_at < ruff_at,
          "Python static-correctness requires a positive Python-file count")
    check(static.get("if") in (None, ""),
          "Python static-correctness is eligible on PR and main events")

rust = load(".github/workflows/rust-ci.yml")
rust_events = event_map(rust)
rust_jobs = rust["jobs"]
rust_push = rust_events.get("push") or {}
rust_pull = rust_events.get("pull_request") or {}
check(rust_push.get("branches") == ["main"] and "paths" not in rust_push,
      "rust-ci push runs on every main head without paths")
check("paths" in rust_pull and rust_pull["paths"],
      "rust-ci pull_request keeps its existing path filter")
for name in ("test", "audit"):
    check(name in rust_jobs and rust_jobs[name].get("if") in (None, ""),
          f"rust-ci {name} is not PR-only")
check(rust_jobs.get("fmt", {}).get("if") == "github.event_name == 'pull_request'",
      "rust-ci pinned formatting is explicitly PR-only")

publish = load(".github/workflows/crates-publish.yml")
publish_jobs = publish["jobs"]
check(publish_jobs.get("dry-run", {}).get("if") == "github.event_name == 'pull_request'",
      "crate dry-runs remain PR-only")
publish_if = str(publish_jobs.get("publish", {}).get("if", ""))
check("github.event_name == 'push'" in publish_if and
      "startsWith(github.ref, 'refs/tags/v')" in publish_if and
      "github.event_name == 'workflow_dispatch'" in publish_if and
      "inputs.confirm == true" in publish_if,
      "crate publishing remains tag/manual-confirm gated")

if fails:
    print(f"{len(fails)} workflow contract assertion(s) failed", file=sys.stderr)
    sys.exit(1)
print("test_main_exact_head_correctness: ALL PASS")
PY
