"""Integration tests for fno config doctor.

Task 4.6 (Phase 4): fno config doctor diagnostic command.

AC4-HP: clean defaults exit 0
AC4-ERR: state_dir under /tmp exits non-zero with reason
AC4-FR: missing settings.yaml exits non-zero without traceback
AC4-EDGE: plans_dir under ~/Dropbox flags sync-conflict reason

Autouse fixture pins FNO_REPO_ROOT per feedback_fno_repo_root_leaks_between_tests.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()
_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb", "FNO_SKIP_MIGRATION": "1"}


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Pin FNO_REPO_ROOT and clear caches before/after each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    if hasattr(paths_mod, "resolve_repo_root"):
        paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()
    if hasattr(paths_mod, "resolve_repo_root"):
        paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]


def _write_settings(tmp_path: Path, content: str) -> Path:
    state = tmp_path / ".fno"
    state.mkdir(exist_ok=True)
    f = state / "settings.yaml"
    f.write_text(content, encoding="utf-8")
    return f


def test_config_doctor_help_renders() -> None:
    """AC4-HP: fno config doctor --help exits 0 and shows help text."""
    result = runner.invoke(app, ["config", "doctor", "--help"], env=_ENV)
    assert result.exit_code == 0, f"exit {result.exit_code}:\n{result.output}"
    assert "doctor" in result.output.lower()


def test_config_app_registered() -> None:
    """AC4-HP: fno config --help shows the config subapp."""
    result = runner.invoke(app, ["config", "--help"], env=_ENV)
    assert result.exit_code == 0, f"exit {result.exit_code}:\n{result.output}"
    assert "doctor" in result.output


def test_doctor_clean_exits_zero(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-HP: clean settings.yaml with default paths exits 0.

    FNO_TEST_MODE=1 bypasses /tmp/ suspicious-path checks so this test
    passes on Linux CI runners where pytest's tmp_path is under /tmp/.
    """
    settings = _write_settings(
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: {str(tmp_path / '.fno')}/\n",
    )
    env = {**_ENV, "FNO_CONFIG": str(settings), "FNO_TEST_MODE": "1"}
    result = runner.invoke(app, ["config", "doctor"], env=env)
    assert result.exit_code == 0, (
        f"Expected exit 0 for clean settings, got {result.exit_code}:\n{result.output}"
    )
    assert "OK" in result.output or "no suspicious" in result.output


def test_doctor_flags_tmp_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-ERR: state_dir under /tmp exits non-zero with temp-directory reason."""
    state_dir = "/tmp/fno-test-state"
    settings = _write_settings(
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: {state_dir}/\n",
    )
    env = {**_ENV, "FNO_CONFIG": str(settings)}
    result = runner.invoke(app, ["config", "doctor"], env=env)
    assert result.exit_code != 0, (
        f"Expected non-zero exit for /tmp state_dir, got {result.exit_code}:\n{result.output}"
    )
    assert "temp" in result.output.lower() or "reboot" in result.output.lower(), (
        f"Expected temp-directory reason in output:\n{result.output}"
    )


def test_doctor_flags_dropbox_plans_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-EDGE: plans_dir under ~/Dropbox exits non-zero with sync-conflict reason."""
    # We don't need Dropbox to actually exist; doctor checks the configured path string.
    dropbox_path = str(Path.home() / "Dropbox" / "fno-plans")
    settings = _write_settings(
        tmp_path,
        f"schema_version: 1\nconfig:\n"
        f"  state_dir: {str(tmp_path / '.fno')}/\n"
        f"  paths:\n    briefs_dir: '{dropbox_path}'\n",
    )
    env = {**_ENV, "FNO_CONFIG": str(settings)}
    result = runner.invoke(app, ["config", "doctor"], env=env)
    assert result.exit_code != 0, (
        f"Expected non-zero exit for Dropbox path, got {result.exit_code}:\n{result.output}"
    )
    assert "dropbox" in result.output.lower() or "sync" in result.output.lower(), (
        f"Expected sync-conflict reason in output:\n{result.output}"
    )


def test_doctor_exits_nonzero_on_accessor_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 3: accessor ERROR must cause doctor to exit non-zero.

    {vault} in paths.graph_json with obsidian disabled causes the accessor
    to raise at resolve time (settings loads fine). Doctor must count that
    as an error and return non-zero.

    Note: {project} no longer raises for non-git dirs (it uses root.name),
    so this test uses {vault} with obsidian disabled instead.
    """
    settings = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n"
        f"  state_dir: {str(tmp_path / '.fno')}/\n"
        "  paths:\n"
        "    graph_json: '{vault}/graph.json'\n"
        "  obsidian:\n"
        "    enabled: false\n",
    )
    env = {
        **_ENV,
        "FNO_CONFIG": str(settings),
    }
    result = runner.invoke(app, ["config", "doctor"], env=env)
    assert result.exit_code != 0, (
        f"Expected non-zero exit when accessor raises, got {result.exit_code}:\n{result.output}"
    )


def test_doctor_missing_settings_exits_nonzero_no_traceback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-FR: missing settings.yaml exits non-zero without a Python traceback."""
    nonexistent = tmp_path / "nonexistent" / "settings.yaml"
    env = {**_ENV, "FNO_CONFIG": str(nonexistent)}
    result = runner.invoke(app, ["config", "doctor"], env=env)
    assert result.exit_code != 0, (
        f"Expected non-zero exit for missing settings, got {result.exit_code}:\n{result.output}"
    )
    # No raw Python traceback
    assert "Traceback" not in result.output, (
        f"Unexpected traceback in output:\n{result.output}"
    )
    # Should have a friendly message
    output_lower = result.output.lower()
    assert any(
        hint in output_lower
        for hint in ("settings.yaml", "migrate-paths", "not found", "no settings", "missing")
    ), f"Expected friendly message in output:\n{result.output}"


# ---------------------------------------------------------------------------
# ab-554d37ef: config doctor surfaces malformed kanban.wip_caps
# ---------------------------------------------------------------------------


def test_check_wip_caps_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A valid wip_caps block reports no problems."""
    from fno.setup.doctor import check_wip_caps
    f = tmp_path / "global.yaml"
    f.write_text("config:\n  kanban:\n    wip_caps:\n      now: 20\n      next: 50\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    assert check_wip_caps() == []


def test_check_wip_caps_absent_is_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """No kanban/wip_caps block at all is clean (the common case)."""
    from fno.setup.doctor import check_wip_caps
    f = tmp_path / "global.yaml"
    f.write_text("config:\n  obsidian:\n    enabled: false\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    assert check_wip_caps() == []


def test_check_wip_caps_flags_malformed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Quoted-string, negative, and boolean caps are each reported (ab-554d37ef).

    These are exactly the values render_html._load_wip_caps silently drops, so
    doctor is the place a user finds out a cap stopped working."""
    from fno.setup.doctor import check_wip_caps
    f = tmp_path / "global.yaml"
    f.write_text(
        "config:\n"
        "  kanban:\n"
        "    wip_caps:\n"
        '      now: "20"\n'   # quoted string -> not an int
        "      next: -5\n"     # negative
        "      later: true\n"  # boolean
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    problems = check_wip_caps()
    assert len(problems) == 3, problems
    joined = " ".join(problems)
    assert "'now'" in joined and "'next'" in joined and "'later'" in joined


def test_check_worktree_policy_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A valid policy + correctly-spelled per-project key reports nothing."""
    from fno.setup.doctor import check_worktree_policy
    f = tmp_path / "global.yaml"
    f.write_text(
        "config:\n  worktree:\n    policy: never\n"
        "work:\n  workspaces:\n    default:\n      projects:\n"
        "        - name: vault\n          worktree: never\n"
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    assert check_worktree_policy() == []


def test_check_worktree_policy_flags_out_of_enum(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An out-of-enum global policy value is surfaced (it refuses creation)."""
    from fno.setup.doctor import check_worktree_policy
    f = tmp_path / "global.yaml"
    f.write_text("config:\n  worktree:\n    policy: conductor\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    problems = check_worktree_policy()
    assert len(problems) == 1 and "conductor" in problems[0]


def test_check_worktree_policy_flags_typo_key(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A per-project key one edit from 'worktree' is flagged as the silent typo
    trap (extra='ignore' drops it, so the project gets the default policy)."""
    from fno.setup.doctor import check_worktree_policy
    f = tmp_path / "global.yaml"
    f.write_text(
        "work:\n  workspaces:\n    default:\n      projects:\n"
        "        - name: vault\n          worktre: never\n"  # typo
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    problems = check_worktree_policy()
    assert len(problems) == 1 and "worktre" in problems[0] and "vault" in problems[0]


def test_check_worktree_policy_scans_repo_local(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A typo'd per-project key in the REPO-LOCAL .fno/config.toml is surfaced
    (the override, and its typo, can live there, not only in global config)."""
    import fno.paths as _paths
    from fno.setup.doctor import check_worktree_policy

    g = tmp_path / "global.yaml"
    g.write_text("config:\n  obsidian:\n    enabled: false\n")  # clean global
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(g))

    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    (repo / ".fno" / "config.toml").write_text(
        "[[work.workspaces.default.projects]]\nname = \"repo\"\nworktre = \"never\"\n"
    )

    # Stub resolve_repo_root to point at our fake repo. It carries a .cache_clear
    # so the local teardown fixture (which clears the real lru_cache) doesn't trip
    # over a bare function replacement.
    class _StubRepoRoot:
        @staticmethod
        def cache_clear() -> None:
            return None

        def __call__(self) -> Path:
            return repo

    monkeypatch.setattr(_paths, "resolve_repo_root", _StubRepoRoot())
    problems = check_worktree_policy()
    assert len(problems) == 1 and "worktre" in problems[0]


def test_check_wip_caps_non_mapping_block(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """wip_caps as a scalar (not a mapping) is reported once."""
    from fno.setup.doctor import check_wip_caps
    f = tmp_path / "global.yaml"
    f.write_text("config:\n  kanban:\n    wip_caps: 20\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    problems = check_wip_caps()
    assert len(problems) == 1 and "not a mapping" in problems[0]


def test_check_wip_caps_non_dict_top_level(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A global YAML that parses to a list/scalar must not crash (Gemini PR #430):
    data.get('config') on a non-dict would otherwise raise AttributeError."""
    from fno.setup.doctor import check_wip_caps
    f = tmp_path / "global.yaml"
    f.write_text("- just\n- a\n- list\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(f))
    assert check_wip_caps() == []
