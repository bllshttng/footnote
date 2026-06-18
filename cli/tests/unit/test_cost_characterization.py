"""Characterization (parity) + AC2-EDGE tests for the moved cost trio (ab-58645f63).

Three scripts were moved verbatim into the fno package (git mv, no logic change):

    scripts/metrics/session-cost.py  -> cli/src/fno/cost/_session_cost.py
    scripts/lib/cost_tracker.py      -> cli/src/fno/cost/cost_tracker.py
    scripts/metrics/register-task.py -> cli/src/fno/cost/_register.py

This is a move-not-rewrite: the risk is silent behavioral drift. This test pins
the move three ways:

1. CONTRACT: run the in-package modules via `python3 -m fno.cost.<mod>` on
   deterministic fixtures and assert the documented output (cost_tracker
   estimate math; session-cost --json over a fixed-timestamp transcript).

2. PARITY vs the pre-move scripts: pull the OLD scripts out of git history
   (`git show <rev>:scripts/...`), reconstruct the old `scripts/{metrics,lib}/`
   layout in a tmp dir (so the old session-cost.py's `sys.path.insert(.../lib);
   from cost_tracker import` sibling-import resolves), run them on the SAME
   fixtures, and assert byte-identical output. If the move drifted, the diff
   catches it. Skipped (not failed) when git history lacks the pre-move blob.

3. AC2-EDGE: import + run the cost module from a cwd OUTSIDE any repo (a bare
   tmp dir) with only the installed package importable, asserting `cost_tracker`
   resolves IN-PACKAGE - never a `ModuleNotFoundError: cost_tracker` and never a
   stray repo copy. This is the whole point of replacing the sys.path hack.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_SRC = REPO_ROOT / "cli" / "src"

# A UUID-shaped transcript id (find_transcript() requires UUID_RE to match).
FIXTURE_UUID = "0123abcd-4567-89ab-cdef-0123456789ab"

# A two-line transcript with FIXED timestamps so duration_minutes is
# deterministic (5.0 min) - no time-varying field in the --json output.
FIXTURE_TRANSCRIPT = (
    json.dumps(
        {
            "type": "user",
            "timestamp": "2026-06-13T10:00:00.000Z",
        }
    )
    + "\n"
    + json.dumps(
        {
            "type": "assistant",
            "timestamp": "2026-06-13T10:05:00.000Z",
            "requestId": "req-1",
            "message": {
                "id": "msg-1",
                "model": "claude-opus-4-8",
                "usage": {
                    "input_tokens": 1000,
                    "output_tokens": 500,
                    "cache_read_input_tokens": 2000,
                    "cache_creation_input_tokens": 300,
                },
            },
        }
    )
    + "\n"
)


def _pkg_env() -> dict:
    """Child env with cli/src on PYTHONPATH so `-m fno.cost.<mod>` resolves
    even when this test runs from a bare checkout (no editable install)."""
    env = os.environ.copy()
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = str(CLI_SRC) + (os.pathsep + existing if existing else "")
    return env


def _run_module(module: str, *args: str, env_overrides: dict | None = None,
                cwd: Path | None = None) -> subprocess.CompletedProcess:
    env = _pkg_env()
    if env_overrides:
        env.update(env_overrides)
    return subprocess.run(
        [sys.executable, "-m", module, *args],
        capture_output=True,
        text=True,
        env=env,
        cwd=str(cwd) if cwd else None,
    )


def _fake_home_with_transcript(tmp_path: Path) -> Path:
    """A tmp HOME carrying ~/.claude/projects/<proj>/<uuid>.jsonl."""
    home = tmp_path / "home"
    proj = home / ".claude" / "projects" / "-fixture-project"
    proj.mkdir(parents=True)
    (proj / f"{FIXTURE_UUID}.jsonl").write_text(FIXTURE_TRANSCRIPT)
    return home


# ---------------------------------------------------------------------------
# 1. CONTRACT
# ---------------------------------------------------------------------------

def test_cost_tracker_estimate_contract():
    """`python3 -m fno.cost.cost_tracker estimate` prices opus-4.8 by the table."""
    r = _run_module("fno.cost.cost_tracker", "estimate",
                    "claude-opus-4-8", "1000000", "1000000")
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "30.0000"  # $5/M input + $25/M output


def test_session_cost_json_contract(tmp_path):
    """`python3 -m fno.cost._session_cost --json <uuid>` over the fixture
    transcript yields deterministic cost + token totals."""
    home = _fake_home_with_transcript(tmp_path)
    r = _run_module("fno.cost._session_cost", "--json", FIXTURE_UUID,
                    env_overrides={"HOME": str(home)})
    assert r.returncode == 0, r.stderr
    data = json.loads(r.stdout)
    assert data["session_id"] == FIXTURE_UUID
    assert data["tokens"]["input"] == 1000
    assert data["tokens"]["output"] == 500
    assert data["tokens"]["cache_read"] == 2000
    assert data["tokens"]["cache_create"] == 300
    assert data["tokens"]["total"] == 3800
    assert data["duration_minutes"] == 5.0
    assert data["primary_model"] == "claude-opus-4-8"


# ---------------------------------------------------------------------------
# 2. PARITY vs the pre-move scripts (reconstruct the old scripts/ layout)
# ---------------------------------------------------------------------------

OLD_SESSION_COST = "scripts/metrics/session-cost.py"
OLD_COST_TRACKER = "scripts/lib/cost_tracker.py"
OLD_REGISTER = "scripts/metrics/register-task.py"


def _git_show(path: str) -> str | None:
    """Return the pre-move source from git history, or None if absent.

    The blob was deleted at the tip of this branch (the move commit), so it
    lives on a parent commit. Probe HEAD and a few ancestors for it.
    """
    for rev in ("HEAD", "HEAD~1", "HEAD~2", "HEAD~3", "origin/main"):
        try:
            out = subprocess.run(
                ["git", "show", f"{rev}:{path}"],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
            )
        except OSError:
            return None
        if out.returncode == 0 and out.stdout.strip():
            return out.stdout
    return None


def _reconstruct_old_scripts(tmp_path: Path) -> Path | None:
    """Rebuild the pre-move scripts/{metrics,lib}/ layout from git history so
    the old session-cost.py's sibling `from cost_tracker import` resolves.
    Returns the scripts dir, or None when history is unavailable."""
    sc = _git_show(OLD_SESSION_COST)
    ct = _git_show(OLD_COST_TRACKER)
    if sc is None or ct is None:
        return None
    scripts = tmp_path / "old_scripts"
    (scripts / "metrics").mkdir(parents=True)
    (scripts / "lib").mkdir(parents=True)
    (scripts / "metrics" / "session-cost.py").write_text(sc)
    (scripts / "lib" / "cost_tracker.py").write_text(ct)
    return scripts


def test_parity_cost_tracker_estimate(tmp_path):
    """The in-package cost_tracker and the pre-move script produce
    byte-identical `estimate` output across a model/token matrix."""
    ct = _git_show(OLD_COST_TRACKER)
    if ct is None:
        pytest.skip("pre-move cost_tracker.py not available in git history")
    old = tmp_path / "old_cost_tracker.py"
    old.write_text(ct)

    cases = [
        ("claude-opus-4-8", "1000000", "1000000"),
        ("claude-opus-4-8", "0", "0", "1000000", "1000000"),
        ("sonnet", "1000000", "1000000"),
        ("haiku", "1000000", "0"),
        ("claude-haiku-4-5", "123456", "7890"),
    ]
    for args in cases:
        old_out = subprocess.run(
            [sys.executable, str(old), "estimate", *args],
            capture_output=True, text=True,
        )
        new_out = _run_module("fno.cost.cost_tracker", "estimate", *args)
        assert old_out.returncode == new_out.returncode, (
            f"exit drift for {args}: old={old_out.returncode} new={new_out.returncode}"
        )
        assert old_out.stdout == new_out.stdout, (
            f"estimate drift for {args}: old={old_out.stdout!r} new={new_out.stdout!r}"
        )


def test_parity_session_cost_json(tmp_path):
    """The in-package _session_cost and the pre-move session-cost.py produce
    byte-identical `--json` output over the same fixture transcript."""
    scripts = _reconstruct_old_scripts(tmp_path)
    if scripts is None:
        pytest.skip("pre-move session-cost.py / cost_tracker.py not in git history")

    home = _fake_home_with_transcript(tmp_path)
    env = os.environ.copy()
    env["HOME"] = str(home)

    old_out = subprocess.run(
        [sys.executable, str(scripts / "metrics" / "session-cost.py"),
         "--json", FIXTURE_UUID],
        capture_output=True, text=True, env=env,
    )
    new_out = _run_module("fno.cost._session_cost", "--json", FIXTURE_UUID,
                          env_overrides={"HOME": str(home)})

    assert old_out.returncode == new_out.returncode, (
        f"exit drift: old={old_out.returncode} new={new_out.returncode}\n"
        f"old stderr: {old_out.stderr}\nnew stderr: {new_out.stderr}"
    )
    assert old_out.stdout == new_out.stdout, (
        "session-cost --json drift between pre-move script and in-package module"
    )


def test_parity_register_task_ledger(tmp_path):
    """The in-package _register and the pre-move register-task.py append a
    byte-identical ledger entry (modulo the inherently time-varying
    completed_at/registered_at timestamps) on the same target-state."""
    reg = _git_show(OLD_REGISTER)
    if reg is None:
        pytest.skip("pre-move register-task.py not available in git history")
    old_script = tmp_path / "old_register.py"
    old_script.write_text(reg)

    def _run_register(script_args, home: Path) -> tuple[int, list[dict]]:
        env = os.environ.copy()
        env["HOME"] = str(home)
        # cli/src on PYTHONPATH covers the in-package run; harmless for the old.
        env["PYTHONPATH"] = str(CLI_SRC) + (
            os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else ""
        )
        state = home / "proj" / ".fno" / "target-state.md"
        state.parent.mkdir(parents=True, exist_ok=True)
        state.write_text(
            "---\n"
            "status: IN_PROGRESS\n"
            "session_id: 20260613T100000Z-1-parity\n"
            "input: parity feature\n"
            "type: execution\n"
            "---\n"
            "# Target Session State\n"
            "graph_node_id: ab-parity01\n"
        )
        out = subprocess.run(
            script_args + [str(state), "tid-parity",
                           "--termination-reason", "DonePRGreen"],
            capture_output=True, text=True, env=env, cwd=str(home / "proj"),
        )
        ledger = home / ".fno" / "ledger.json"
        entries = []
        if ledger.exists():
            try:
                entries = json.loads(ledger.read_text()).get("entries", [])
            except (json.JSONDecodeError, OSError):
                entries = []
        return out.returncode, entries

    def _normalize(entry: dict) -> dict:
        """Drop the inherently time-varying fields so two runs compare."""
        e = dict(entry)
        for k in ("completed", "completed_at", "registered_at", "timestamp", "date"):
            e.pop(k, None)
        return e

    old_home = tmp_path / "old_home"
    new_home = tmp_path / "new_home"
    old_rc, old_entries = _run_register([sys.executable, str(old_script)], old_home)
    new_rc, new_entries = _run_register(
        [sys.executable, "-m", "fno.cost._register"], new_home
    )

    assert old_rc == new_rc, f"register exit drift: old={old_rc} new={new_rc}"
    assert len(old_entries) == len(new_entries) == 1, (
        f"expected one ledger entry each: old={old_entries} new={new_entries}"
    )
    assert _normalize(old_entries[0]) == _normalize(new_entries[0]), (
        "ledger entry drift between pre-move register-task.py and in-package _register"
    )


# ---------------------------------------------------------------------------
# 3. AC2-EDGE: in-package cost_tracker resolution from a non-repo cwd
# ---------------------------------------------------------------------------

def test_ac2_edge_cost_tracker_resolves_in_package_from_tmp_cwd(tmp_path):
    """Run the cost module from a cwd OUTSIDE any repo (a bare tmp dir) with
    only cli/src on PYTHONPATH. The former `sys.path.insert(.../lib)` hack
    would have looked for cost_tracker beside a nonexistent repo `lib/`; the
    in-package import must bind fno.cost.cost_tracker with NO ModuleNotFoundError.
    """
    bare = tmp_path / "outside-any-repo"
    bare.mkdir()
    # Sanity: this dir is not inside a git repo.
    assert not (bare / ".git").exists()

    # Importing _session_cost executes `from fno.cost.cost_tracker import ...`
    # at module top; if it resolved a stray sibling it would crash here.
    probe = (
        "import fno.cost._session_cost as s;"
        "import fno.cost.cost_tracker as ct;"
        "import sys;"
        # The bound cost_tracker must be the in-package module, and there must
        # be NO bare top-level `cost_tracker` (the old sys.path-hack name).
        "assert ct.__name__ == 'fno.cost.cost_tracker', ct.__name__;"
        "assert 'cost_tracker' not in sys.modules or "
        "sys.modules.get('cost_tracker') is None, "
        "'a bare top-level cost_tracker module leaked in';"
        "assert s.model_tier is not None;"
        "print('IN_PACKAGE_OK', ct.__file__)"
    )
    r = subprocess.run(
        [sys.executable, "-c", probe],
        capture_output=True, text=True, env=_pkg_env(), cwd=str(bare),
    )
    assert r.returncode == 0, (
        f"in-package cost_tracker resolution failed from {bare}:\n"
        f"stdout: {r.stdout}\nstderr: {r.stderr}"
    )
    assert "IN_PACKAGE_OK" in r.stdout
    assert "ModuleNotFoundError" not in r.stderr
    # The resolved file lives inside the fno package, not a repo scripts/lib.
    assert "fno/cost/cost_tracker.py" in r.stdout.replace(os.sep, "/")
