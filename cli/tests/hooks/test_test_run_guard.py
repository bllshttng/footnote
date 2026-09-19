"""The test-run guard: what it refuses and what it must not.

Both halves matter. The guard exists because two workers running raw test
suites crushed the machine on 2026-09-18; a guard that denies too little
does not stop that, and a guard that denies `fno doctor test`, `cargo
build`, or a pytest inside an echo string is a guard someone disables.
"""
from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "test-run-guard.py"


def _load():
    spec = importlib.util.spec_from_file_location("test_run_guard", HOOK)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


guard = _load()


def _git_repo(path: Path) -> Path:
    subprocess.run(["git", "init", "-q", str(path)], check=True, capture_output=True)
    return path


@pytest.fixture(scope="module")
def footnote_repo(tmp_path_factory):
    """A repo shaped like this one: cli/src/fno plus hooks/hooks.json."""
    root = _git_repo(tmp_path_factory.mktemp("footnote"))
    (root / "cli" / "src" / "fno").mkdir(parents=True)
    (root / "hooks").mkdir()
    (root / "hooks" / "hooks.json").write_text("{}", encoding="utf-8")
    return root


@pytest.fixture(scope="module")
def plain_repo(tmp_path_factory):
    """A git repo with no footnote markers: the plugin runs here too."""
    return _git_repo(tmp_path_factory.mktemp("plain"))


@pytest.fixture(scope="module")
def no_repo(tmp_path_factory):
    """A plain directory with no git repository at all."""
    return tmp_path_factory.mktemp("norepo")


# --- refusals ---------------------------------------------------------


def test_uv_run_pytest_refused_names_doctor_test(footnote_repo):
    refusal = guard.decide("uv run pytest cli/tests/unit/x.py", cwd=str(footnote_repo))
    assert refusal and "fno doctor test" in refusal


def test_bare_pytest_refused(footnote_repo):
    assert guard.decide("pytest -q cli/tests", cwd=str(footnote_repo))


def test_full_path_pytest_refused(footnote_repo):
    assert guard.decide("/usr/bin/env pytest -q", cwd=str(footnote_repo))


def test_python_m_pytest_refused(footnote_repo):
    assert guard.decide("python3 -m pytest -q", cwd=str(footnote_repo))


def test_env_prefix_refused(footnote_repo):
    assert guard.decide("FNO_DEBUG=1 pytest -q", cwd=str(footnote_repo))


def test_timeout_wrapper_refused(footnote_repo):
    assert guard.decide("timeout 30 uv run pytest", cwd=str(footnote_repo))


def test_uvx_and_uv_tool_run_refused(footnote_repo):
    assert guard.decide("uvx pytest", cwd=str(footnote_repo))
    assert guard.decide("uv tool run pytest -q", cwd=str(footnote_repo))


def test_uv_run_python_m_pytest_refused(footnote_repo):
    assert guard.decide("uv run python -m pytest -q", cwd=str(footnote_repo))


def test_pipeline_stage_refused(footnote_repo):
    assert guard.decide("rg pattern sources/ | pytest -q", cwd=str(footnote_repo))


def test_bash_c_payload_refused(footnote_repo):
    assert guard.decide("bash -lc 'cd cli && pytest -q'", cwd=str(footnote_repo))


def test_cargo_test_refused_names_doctor_test_rust(footnote_repo):
    refusal = guard.decide("cargo test -p fno", cwd=str(footnote_repo))
    assert refusal and "fno doctor test rust" in refusal


def test_cargo_test_behind_global_flags_refused(footnote_repo):
    assert guard.decide(
        "cargo --manifest-path cli/Cargo.toml test", cwd=str(footnote_repo)
    )


def test_compound_refuses_only_the_test_half(footnote_repo):
    refusal = guard.decide("cargo build && cargo test", cwd=str(footnote_repo))
    assert refusal and "cargo test" in refusal


# --- allows -----------------------------------------------------------


def test_doctor_test_allows(footnote_repo):
    assert (
        guard.decide("fno doctor test cli/tests/unit/x.py", cwd=str(footnote_repo))
        is None
    )
    assert guard.decide("fno doctor test rust", cwd=str(footnote_repo)) is None


def test_cargo_build_allows(footnote_repo):
    assert guard.decide("cargo build --release", cwd=str(footnote_repo)) is None


def test_cargo_nextest_not_in_scope(footnote_repo):
    assert guard.decide("cargo nextest run", cwd=str(footnote_repo)) is None


def test_pytest_in_echo_string_allows(footnote_repo):
    assert guard.decide('echo "pytest passed"', cwd=str(footnote_repo)) is None


def test_pytest_in_heredoc_body_allows(footnote_repo):
    cmd = "cat > script.sh <<'EOF'\npytest -q\nEOF\nbash script.sh"
    assert guard.decide(cmd, cwd=str(footnote_repo)) is None


def test_other_tool_through_uv_allows(footnote_repo):
    assert guard.decide("uv run ruff check hooks/", cwd=str(footnote_repo)) is None


def test_unbalanced_quotes_fail_open(footnote_repo):
    assert guard.decide("pytest -q 'unclosed", cwd=str(footnote_repo)) is None


def test_scope_non_footnote_repo_allows(plain_repo):
    assert guard.decide("uv run pytest x.py", cwd=str(plain_repo)) is None


def test_scope_no_repo_allows(no_repo):
    assert guard.decide("pytest", cwd=str(no_repo)) is None


# --- the hook itself ---------------------------------------------------


def _run_hook(payload: str, cwd: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(HOOK)],
        input=payload,
        capture_output=True,
        text=True,
        cwd=str(cwd),
        timeout=30,
    )


def test_main_denies_via_stdin(footnote_repo):
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "uv run pytest cli/tests/unit/x.py"},
        }
    )
    proc = _run_hook(payload, footnote_repo)
    decision = json.loads(proc.stdout)["hookSpecificOutput"]
    assert decision["permissionDecision"] == "deny"
    assert decision["hookEventName"] == "PreToolUse"
    assert "fno doctor test" in decision["permissionDecisionReason"]


def test_main_allows_non_bash_tool(footnote_repo):
    proc = _run_hook(json.dumps({"tool_name": "Grep", "tool_input": {}}), footnote_repo)
    assert proc.returncode == 0
    assert proc.stdout.strip() == ""


def test_main_allows_malformed_stdin(footnote_repo):
    proc = _run_hook("not json", footnote_repo)
    assert proc.returncode == 0
    assert proc.stdout.strip() == ""
