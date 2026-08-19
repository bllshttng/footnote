"""Policy tests for opt-in local preflight.

`local_verification_required` is the ONE function that decides whether a full
local `scripts/ci/preflight.sh` receipt is required; every Python lane
(`fno pr evidence-required`, the worker ship lane, the batch lane) routes
through it, and the bash ship path asks it via the CLI. These tests pin the
returned reason string, not just the boolean, because the reason is the
receipt three blocked workers needed on 2026-08-19.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.pr._preflight import _preflight_required_by_config, local_verification_required

REPO_ROOT = Path(__file__).resolve().parents[3]


def _opt_in_preflight(tmp_path: Path) -> None:
    """Pin the project config to `preflight.required = true` for tmp_path."""
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "config.toml").write_text("[preflight]\nrequired = true\n")


def _fake_git(monkeypatch: pytest.MonkeyPatch, changed: str) -> None:
    """ls-tree sees the runner in the base; diff names `changed`."""

    def fake(args, *_args, **_kwargs):
        output = (
            "100755 blob abc\tscripts/ci/preflight.sh\n"
            if args[0] == "ls-tree"
            else f"{changed}\n"
        )
        return type("Result", (), {"returncode": 0, "stdout": output})()

    monkeypatch.setattr("fno.pr._preflight._git", fake)


def _make_runner(tmp_path: Path) -> None:
    runner = tmp_path / "scripts" / "ci" / "preflight.sh"
    runner.parent.mkdir(parents=True, exist_ok=True)
    runner.write_text("#!/bin/sh\n")
    runner.chmod(0o755)


def test_stock_config_returns_policy_opt_in_before_any_git_work(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """AC1: a stock config never requires the rehearsal, reason pinned."""
    _make_runner(tmp_path)

    def unreachable(*_args, **_kwargs):  # pragma: no cover - assertion fires first
        raise AssertionError("policy line must fire before any git subprocess work")

    monkeypatch.setattr("fno.pr._preflight._git", unreachable)
    assert local_verification_required(cwd=str(tmp_path)) == (False, "policy-opt-in")


def test_opted_in_project_requires_on_a_code_diff(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """AC2: preflight.required = true restores (True, "required")."""
    _make_runner(tmp_path)
    _opt_in_preflight(tmp_path)
    _fake_git(monkeypatch, "cli/code.py")
    assert local_verification_required(cwd=str(tmp_path)) == (True, "required")


def test_opted_in_project_keeps_runner_removed_and_docs_only_reasons(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The pre-existing reasons survive unchanged under the opt-in."""
    _opt_in_preflight(tmp_path)

    # runner-not-configured: no local runner, none in the base either
    def base_git(args, *_args, **_kwargs):
        output = "" if args[0] == "ls-tree" else "cli/code.py\n"
        return type("Result", (), {"returncode": 0, "stdout": output})()

    monkeypatch.setattr("fno.pr._preflight._git", base_git)
    assert local_verification_required(cwd=str(tmp_path)) == (False, "runner-not-configured")

    # docs-only: runner present, non-docs list empty under the opt-in
    _make_runner(tmp_path)
    _fake_git(monkeypatch, "docs/readme.md")
    assert local_verification_required(cwd=str(tmp_path)) == (False, "docs-only")


def test_explicit_skip_env_still_wins_over_opt_in(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """AC5: FNO_SKIP_PREFLIGHT=1 beats preflight.required = true."""
    _opt_in_preflight(tmp_path)
    assert local_verification_required(
        cwd=str(tmp_path), env={"FNO_SKIP_PREFLIGHT": "1"}
    ) == (False, "explicit-skip")


@pytest.mark.parametrize("bad_config", ["[preflight\nrequired = true\n", "preflight: ["])
def test_malformed_config_fails_open(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, bad_config: str
) -> None:
    """AC4: a gate no one can evaluate must not stop a green commit."""
    _make_runner(tmp_path)
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "config.toml").write_text(bad_config)
    assert local_verification_required(cwd=str(tmp_path)) == (False, "policy-opt-in")


def test_wrong_typed_required_fails_open(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A non-bool block raises inside the loader; the policy returns False.

    `required = "yes"` is NOT a failure (pydantic lax-coerces the string to
    True), so the fail-open path needs a type the model genuinely rejects.
    """
    _make_runner(tmp_path)
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "config.toml").write_text("preflight = \"banana\"\n")
    assert local_verification_required(cwd=str(tmp_path)) == (False, "policy-opt-in")


def test_fno_config_pin_is_honored(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """FNO_CONFIG names the only candidate; the key is read, not hardcoded."""
    pinned = tmp_path / "pf-check.toml"
    pinned.write_text("preflight.required = true\n")
    assert _preflight_required_by_config(tmp_path, {"FNO_CONFIG": str(pinned)}) is True

    pinned.write_text("preflight.required = false\n")
    assert _preflight_required_by_config(tmp_path, {"FNO_CONFIG": str(pinned)}) is False


def test_ship_phase_bash_path_asks_the_policy() -> None:
    """Path-uniqueness pin: the /target ship path consults evidence-required.

    A future edit that reintroduces an unconditional scripts/ci/preflight.sh
    run on that path fails this named test instead of quietly restoring the
    six-concurrent-runs load this node exists to remove.
    """
    ship_phase = REPO_ROOT / "skills" / "target" / "references" / "ship-phase.md"
    content = ship_phase.read_text(encoding="utf-8")
    assert "fno pr evidence-required" in content, (
        "skills/target/references/ship-phase.md must ask fno pr evidence-required "
        "before running scripts/ci/preflight.sh; a guard on one of two reachable "
        "paths is decorative"
    )
    # No second decider beside the policy call: the old self-deciding guard
    # never asked the policy, and a non_docs grep layer disagreed with the
    # Python docs-only rule for runtime markdown.
    assert 'FNO_SKIP_PREFLIGHT:-0} != "1" && -x scripts/ci/preflight.sh' not in content
    assert 'non_docs="' not in content


def test_policy_read_uses_the_env_seam_not_os_environ(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The env param controls FNO_SKIP_PREFLIGHT and FNO_CONFIG alike.

    An ambient FNO_CONFIG in the parent shell must not override what a caller
    injecting env= says the config pin is.
    """
    ambient = tmp_path / "ambient.toml"
    ambient.write_text("preflight.required = true\n")
    monkeypatch.setenv("FNO_CONFIG", str(ambient))
    injected = tmp_path / "injected.toml"
    injected.write_text("preflight.required = false\n")

    assert local_verification_required(cwd=str(tmp_path), env={"FNO_CONFIG": str(injected)}) == (
        False,
        "policy-opt-in",
    )
