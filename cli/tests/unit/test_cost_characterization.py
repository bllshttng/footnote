"""Session-cost contract + in-package resolution tests.

The price table moved to Rust (`crates/fno-agents/src/model_price.rs`); the
Rust tests own the price math. What Python still owns and this file pins:

1. CONTRACT: `_session_cost --json` over a fixed-timestamp transcript yields
   deterministic token totals, and an unpriced session (no binary to shell,
   no catalog) reads `cost_usd: null` with `unpriced_model` naming the
   primary model - never a fallback-tier guess.

2. AC2-EDGE: import + run the cost module from a cwd OUTSIDE any repo (a bare
   tmp dir) with only the installed package importable - no stray repo copy.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

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


def test_session_cost_json_contract(tmp_path):
    """`python3 -m fno.cost._session_cost --json <uuid>` over the fixture
    transcript yields deterministic token totals; with no fno-agents binary
    to shell the price leg, the session reads unpriced (AC9-ERR)."""
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
    assert data["cost_usd"] is None
    assert data["unpriced_model"] == "claude-opus-4-8"


def test_calculate_cost_shells_the_price_leg(monkeypatch):
    """A stubbed price call returning 44.97 lands in cost_usd (AC8-HP)."""
    from fno.cost import _session_cost as s

    class FakeCompleted:
        returncode = 0
        stdout = b"44.9700\n"

    calls = []

    def fake_run(argv, **kwargs):
        calls.append(argv)
        return FakeCompleted()

    monkeypatch.setattr(s, "subprocess", subprocess)
    monkeypatch.setattr(s.subprocess, "run", fake_run)
    monkeypatch.setattr(s, "_fno_agents_binary", lambda: Path("/usr/bin/true"))

    m = s.SessionMetrics(session_id="s")
    m.models = {"claude-opus-5-5[1m]": 3}
    m.input_tokens = 1_000_000
    cost = s.calculate_cost(m)
    assert cost == 44.97
    assert not m.unpriced
    argv = calls[0]
    assert "context-run" in argv and "--model-price" in argv


def test_calculate_cost_is_none_when_the_price_call_refuses(monkeypatch):
    """Exit 3 (unpriced) or a missing binary reads cost None, unpriced set."""
    from fno.cost import _session_cost as s

    class Refusing:
        returncode = 3
        stdout = b"unpriced\n"

    monkeypatch.setattr(s.subprocess, "run", lambda argv, **k: Refusing())
    monkeypatch.setattr(s, "_fno_agents_binary", lambda: Path("/usr/bin/true"))
    m = s.SessionMetrics(session_id="s")
    m.models = {"claude-opus-next": 1}
    assert s.calculate_cost(m) is None
    assert m.unpriced

    monkeypatch.setattr(s, "_fno_agents_binary", lambda: None)
    m = s.SessionMetrics(session_id="s")
    m.models = {"claude-opus-5-5": 1}
    assert s.calculate_cost(m) is None
    assert m.unpriced


def test_ac2_edge_cost_module_resolves_in_package_from_tmp_cwd(tmp_path):
    """Run the cost module from a cwd OUTSIDE any repo (a bare tmp dir) with
    only cli/src on PYTHONPATH: the import binds the in-package module and
    never a stray repo copy."""
    bare = tmp_path / "outside-any-repo"
    bare.mkdir()
    # Sanity: this dir is not inside a git repo.
    assert not (bare / ".git").exists()

    probe = (
        "import fno.cost._session_cost as s;"
        "assert s.__name__ == 'fno.cost._session_cost', s.__name__;"
        "assert callable(s.calculate_cost);"
        "print('IN_PACKAGE_OK', s.__file__)"
    )
    r = subprocess.run(
        [sys.executable, "-c", probe],
        capture_output=True, text=True, env=_pkg_env(), cwd=str(bare),
    )
    assert r.returncode == 0, (
        f"in-package cost module resolution failed from {bare}:\n"
        f"stdout: {r.stdout}\nstderr: {r.stderr}"
    )
    assert "IN_PACKAGE_OK" in r.stdout
    assert "ModuleNotFoundError" not in r.stderr
    # The resolved file lives inside the fno package, not a repo scripts dir.
    assert "fno/cost/_session_cost.py" in r.stdout.replace(os.sep, "/")
