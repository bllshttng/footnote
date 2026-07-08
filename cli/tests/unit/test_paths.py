"""Tests for fno.paths typed resolver.

Task 1.2: Create paths.py typed resolver with template substitution.
Task 1.4: Round out coverage for all AC items.

All tests use tmp_path + monkeypatch isolation. An autouse fixture pins
FNO_REPO_ROOT to tmp_path so resolve_repo_root() is isolated
(feedback_abi_repo_root_leaks_between_tests memory entry).
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Generator

import pytest


# ---------------------------------------------------------------------------
# Autouse fixture: pin FNO_REPO_ROOT and clear caches before each test
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Isolate each test: reset caches and pin repo root + settings."""
    # Pin FNO_REPO_ROOT so resolve_repo_root() doesn't wander
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    # Clear settings cache so monkeypatched env takes effect
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    # Clear paths caches (resolve_repo_root now @cached)
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    if hasattr(paths_mod, "resolve_repo_root"):
        try:
            paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    yield
    # Clear again after test to avoid pollution
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    if hasattr(paths_mod, "resolve_repo_root"):
        try:
            paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass


def _write_settings(tmp_path: Path, content: str) -> Path:
    """Write a settings.yaml to tmp_path and return its path."""
    f = tmp_path / "settings.yaml"
    f.write_text(content, encoding="utf-8")
    return f


def _set_settings(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, content: str) -> None:
    """Write a settings.yaml and wire it via FNO_CONFIG."""
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))


# ---------------------------------------------------------------------------
# AC1-HP: resolve_repo_root() preserved
# ---------------------------------------------------------------------------


def test_resolve_repo_root_still_importable() -> None:
    """Existing callers import resolve_repo_root from fno.paths - must not break."""
    from fno.paths import resolve_repo_root

    result = resolve_repo_root()
    assert isinstance(result, Path)


def test_resolve_repo_root_respects_abi_repo_root_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: resolve_repo_root() returns FNO_REPO_ROOT when set.

    This also exercises the @cache invalidation path: the autouse fixture
    clears the cache; with FNO_REPO_ROOT pinned, this call must return
    the pinned value, not a stale cached value from a prior test.
    """
    import fno.paths as paths_mod

    expected = tmp_path / "my_repo"
    expected.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(expected))

    result = paths_mod.resolve_repo_root()
    assert result == expected.resolve()


# ---------------------------------------------------------------------------
# ab-fe825805 change 4: FNO_REPO_ROOT foreign-project overload warning
# ---------------------------------------------------------------------------


def _fake_git_toplevel(path: Path):
    """A subprocess.run stand-in that reports `path` as the cwd's git toplevel."""
    return lambda *a, **k: type("R", (), {"returncode": 0, "stdout": str(path) + "\n"})()


def _make_plugin_root(path: Path) -> Path:
    """Stamp `path` with the fno plugin marker file so _is_plugin_root()
    recognizes it (the warning gate keys on the marker, not the basename)."""
    marker = path / "hooks" / "helpers" / "init-target-state.sh"
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("#!/usr/bin/env bash\n")
    return path


def test_abi_repo_root_warns_when_pinned_to_abilities_from_foreign_repo(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """FNO_REPO_ROOT pinned at the fno plugin root + a different cwd repo
    emits a one-line stderr heads-up (the silent-wrong-project footgun)."""
    import fno.paths as paths_mod

    abilities_dir = _make_plugin_root(tmp_path / "fno")
    other_repo = tmp_path / "acme-web"
    other_repo.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(abilities_dir))
    # cwd (the pytest cwd) is not inside abilities_dir, so the same-repo
    # short-circuit does not fire; the (stubbed) git probe reports other_repo.
    monkeypatch.setattr(paths_mod.subprocess, "run", _fake_git_toplevel(other_repo))

    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    result = paths_mod.resolve_repo_root()

    assert result == abilities_dir.resolve()  # warning is non-fatal
    err = capsys.readouterr().err
    assert "FNO_REPO_ROOT pins" in err
    assert str(abilities_dir.resolve()) in err


def test_abi_repo_root_no_warning_when_root_is_not_plugin_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A non-plugin-root FNO_REPO_ROOT (e.g. the tmp test hook, even if named
    'fno') never warns - it is not the footgun."""
    import fno.paths as paths_mod

    other = tmp_path / "fno"  # named fno but NO marker file
    other.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(other))

    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    paths_mod.resolve_repo_root()
    assert "FNO_REPO_ROOT pins" not in capsys.readouterr().err


def test_abi_repo_root_no_warning_when_cwd_is_inside_the_pinned_repo(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Running inside the fno plugin root with FNO_REPO_ROOT pointing at
    it is not a footgun - same repo, no warning, and no git subprocess.

    This is the regression that broke the CLI-wrapper tests: those globally
    stub subprocess.run, so the warning path must NOT reach a git probe when
    cwd is inside the pinned root."""
    import fno.paths as paths_mod

    abilities_dir = _make_plugin_root(tmp_path / "fno")
    monkeypatch.setenv("FNO_REPO_ROOT", str(abilities_dir))
    monkeypatch.chdir(abilities_dir)
    # subprocess.run stubbed WITHOUT stdout, like the CLI-wrapper tests. The
    # cwd-inside-resolved short-circuit must return before this is ever called;
    # if it isn't, accessing .stdout would raise.
    monkeypatch.setattr(
        paths_mod.subprocess, "run",
        lambda *a, **k: type("R", (), {"returncode": 0})(),
    )

    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    paths_mod.resolve_repo_root()
    assert "FNO_REPO_ROOT pins" not in capsys.readouterr().err


# ---------------------------------------------------------------------------
# resolve_canonical_repo_root() - config climbs to the main checkout
# ---------------------------------------------------------------------------


def test_resolve_canonical_repo_root_respects_abi_repo_root_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """FNO_REPO_ROOT pins the canonical resolver too (test-isolation hook).

    Same short-circuit as resolve_repo_root(), so the env-pinned test suite
    sees no git call and worktree==canonical (no behavior change).
    """
    import fno.paths as paths_mod

    expected = tmp_path / "canon"
    expected.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(expected))

    assert paths_mod.resolve_canonical_repo_root() == expected.resolve()


def test_resolve_canonical_repo_root_falls_back_when_git_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With no FNO_REPO_ROOT and git unavailable, fall back to resolve_repo_root()."""
    import fno.paths as paths_mod

    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    sentinel = tmp_path / "fallback"
    sentinel.mkdir()
    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: sentinel)

    def _boom(*_args: object, **_kwargs: object) -> object:
        raise FileNotFoundError("git not found")

    monkeypatch.setattr(paths_mod.subprocess, "run", _boom)

    assert paths_mod.resolve_canonical_repo_root() == sentinel


def test_resolve_canonical_repo_root_uses_git_worktree_list(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The main worktree path from `git worktree list` is the canonical root.

    Simulates a linked worktree: `git worktree list --porcelain` lists the main
    worktree first, and its `worktree <path>` line is the canonical working
    tree. The canonical dir carries a `.git` child so the working-tree gate in
    resolve_canonical_worktree() (which skips bare/separate-git-dir gitdir
    entries) accepts it (ab-91a004af worktree-resolution).
    """
    import fno.paths as paths_mod

    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    canonical = tmp_path / "canonical"
    linked = tmp_path / "linked"
    canonical.mkdir(parents=True)
    linked.mkdir(parents=True)
    # A real working tree has a `.git` child; the helper requires it.
    (canonical / ".git").mkdir()
    (linked / ".git").mkdir()

    class _Result:
        returncode = 0
        # Main worktree first (canonical), then a linked worktree.
        stdout = (
            f"worktree {canonical}\n"
            "HEAD 0000000000000000000000000000000000000000\n"
            "branch refs/heads/main\n"
            "\n"
            f"worktree {linked}\n"
            "HEAD 1111111111111111111111111111111111111111\n"
            "branch refs/heads/feature\n"
        )

    monkeypatch.setattr(paths_mod.subprocess, "run", lambda *a, **k: _Result())

    assert paths_mod.resolve_canonical_repo_root() == canonical.resolve()


# ---------------------------------------------------------------------------
# AC1-HP: Default paths resolve correctly
# ---------------------------------------------------------------------------


def test_graph_json_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: graph_json() returns ~/.fno/graph.json resolved to absolute."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import graph_json

    result = graph_json()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "graph.json"
    # Must be under the state_dir (default ~/.fno/)
    assert result.parent.name == ".fno"


def test_ledger_json_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: ledger_json() returns ~/.fno/ledger.json."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import ledger_json

    result = ledger_json()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "ledger.json"


def test_ledger_json_pinned_global_ignores_relative_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The ledger is cross-project and must NOT fork into a per-repo
    stray. A relative (project-/CWD-anchored) state_dir must not drag the
    ledger into the repo checkout; it stays anchored to the user-global
    ~/.fno. An absolute state_dir (the default and test sandboxes) is honored.
    """
    monkeypatch.chdir(tmp_path)
    _set_settings(
        monkeypatch, tmp_path, "schema_version: 1\nconfig:\n  state_dir: .fno/\n"
    )

    from fno.paths import ledger_json

    result = ledger_json()
    # Pinned to ~/.fno, NOT tmp_path/.fno (which is where a relative state_dir
    # would land graph.json/events under CWD).
    assert result == (Path.home() / ".fno" / "ledger.json").resolve()
    assert tmp_path not in result.parents


def test_briefs_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: briefs_dir() returns ~/.fno/briefs."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import briefs_dir

    result = briefs_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "briefs"


def test_fleet_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: fleet_dir() returns ~/.fno/fleet."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import fleet_dir

    result = fleet_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "fleet"


def test_postmortems_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: postmortems_dir() returns ~/.fno/postmortems."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import postmortems_dir

    result = postmortems_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "postmortems"


def test_worktrees_base_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: worktrees_base() returns ~/.fno/worktrees."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import worktrees_base

    result = worktrees_base()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "worktrees"


def test_memory_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: memory_dir() returns ~/.fno/memory."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import memory_dir

    result = memory_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "memory"


def test_hook_logs_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: hook_logs_dir() returns ~/.fno/hook-logs."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import hook_logs_dir

    result = hook_logs_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "hook-logs"


def test_plans_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: plans_dir() returns project-relative .fno/plans/ resolved absolute."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import plans_dir

    # Pass explicit project_root so test doesn't depend on git
    result = plans_dir(project_root=tmp_path)
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert "plans" in result.parts


def test_plans_dir_honors_project_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-FIX2: plans_dir(project_root=bar) must use bar, not CWD."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    # Set FNO_REPO_ROOT to a different location so CWD fallback would differ
    project_bar = tmp_path / "bar"
    project_bar.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path / "foo"))

    from fno.paths import plans_dir

    result = plans_dir(project_root=project_bar)
    # Must be anchored under project_bar, NOT under tmp_path/foo
    assert str(result).startswith(str(project_bar)), (
        f"plans_dir should be under project_bar={project_bar}, got {result}"
    )
    assert result == project_bar / ".fno" / "plans", (
        f"Expected {project_bar / '.fno' / 'plans'}, got {result}"
    )


def test_inbox_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: inbox_dir() returns project-relative .fno/inbox/ resolved absolute."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import inbox_dir

    result = inbox_dir(project_root=tmp_path)
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert "inbox" in result.parts or result.name == "inbox"


def test_state_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: state_dir() returns ~/.fno/ resolved to absolute."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import state_dir

    result = state_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == ".fno"


def test_config_file_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: config_file() returns ~/.fno/settings.yaml."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import config_file

    result = config_file()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == "settings.yaml"


# ---------------------------------------------------------------------------
# AC1-UI: No tilde in returned Path
# ---------------------------------------------------------------------------


def test_state_dir_no_tilde(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-UI: state_dir() returns a Path with no '~' in its string representation."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import state_dir

    result = state_dir()
    assert "~" not in str(result), f"Found '~' in path: {result}"


def test_graph_json_no_tilde(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-UI: graph_json() returns a Path with no '~' in its string representation."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import graph_json

    result = graph_json()
    assert "~" not in str(result), f"Found '~' in path: {result}"


# ---------------------------------------------------------------------------
# AC1-EDGE: Empty paths.* block derives all paths from state_dir
# ---------------------------------------------------------------------------


def test_custom_state_dir_propagates_to_graph_json(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: state_dir override propagates to graph_json when paths.graph_json unset."""
    custom_dir = str(tmp_path / "custom")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_dir}'\n  paths: {{}}\n",
    )

    from fno.paths import graph_json

    result = graph_json()
    assert result == Path(custom_dir).resolve() / "graph.json"


def test_custom_state_dir_propagates_to_briefs_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: state_dir override propagates to briefs_dir when paths.briefs_dir unset."""
    custom_dir = str(tmp_path / "custom")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_dir}'\n",
    )

    from fno.paths import briefs_dir

    result = briefs_dir()
    assert result == Path(custom_dir).resolve() / "briefs"


# ---------------------------------------------------------------------------
# AC1-FR: Cache lifetime (same object on second call)
# ---------------------------------------------------------------------------


def test_paths_cache_consistent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-FR: graph_json() returns same value even if settings file is modified."""
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    # Clear caches explicitly
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.paths import graph_json

    first = graph_json()
    # Modify the file - cached value should not change
    settings_file.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '/changed/'\n", encoding="utf-8"
    )
    second = graph_json()
    assert first == second, "graph_json() should be stable within a process"


# ---------------------------------------------------------------------------
# AC1-EDGE: {{ }} escape sequences
# ---------------------------------------------------------------------------


def test_double_brace_escape_in_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {{personal}} in state_dir becomes {personal} in resolved path."""
    target = tmp_path / "home" / "{personal}" / "fno"
    raw_dir = str(tmp_path / "home" / "{{personal}}" / "fno")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{raw_dir}'\n",
    )

    from fno.paths import state_dir

    result = state_dir()
    assert "{personal}" in str(result), (
        f"Expected literal {{personal}} in path, got: {result}"
    )
    assert "{{" not in str(result), f"Escape not resolved: {result}"


# ---------------------------------------------------------------------------
# AC1-EDGE: {vault} with obsidian disabled -> error at load (tested via config)
# ---------------------------------------------------------------------------


def test_vault_in_state_dir_with_obsidian_disabled_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {vault} in state_dir with obsidian disabled raises at load."""
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  state_dir: '{vault}/fno'\n  obsidian:\n    enabled: false\n",
    )
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    with pytest.raises(Exception, match=r"vault|obsidian"):
        load_settings()


# ---------------------------------------------------------------------------
# AC1-EDGE: {project} validation deferred to resolve time
# ---------------------------------------------------------------------------


def test_project_in_plans_dir_uses_root_basename(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {project} in plans_dir uses project_root.name (no redundant git call).

    Previously this raised ValueError for non-git dirs. The fix uses root.name
    directly since resolve_repo_root() already ran git rev-parse; the git
    re-run was redundant. Non-git dirs now resolve to the directory basename.
    """
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '.fno/plans/{project}'\n",
    )
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))

    from fno.paths import plans_dir

    # Resolves to <tmp_path>/.fno/plans/<tmp_path.name>
    result = plans_dir(project_root=tmp_path)
    assert result == (tmp_path / ".fno" / "plans" / tmp_path.name).resolve()


# ---------------------------------------------------------------------------
# AC1-EDGE: Unknown {foo} variable rejected at resolve time
# ---------------------------------------------------------------------------


def test_unknown_template_variable_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {foo} in a path raises a hard error at resolve time."""
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  state_dir: '/home/{foo}/abilities'\n",
    )

    from fno.paths import state_dir

    with pytest.raises(Exception, match=r"\{foo\}|unknown.*variable|unrecognized"):
        state_dir()


# ---------------------------------------------------------------------------
# AC1-HP: paths.* explicit override wins over state_dir derivation
# ---------------------------------------------------------------------------


def test_explicit_graph_json_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: paths.graph_json explicit value overrides state_dir derivation."""
    custom_json = str(tmp_path / "custom" / "g.json")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  paths:\n    graph_json: '{custom_json}'\n",
    )

    from fno.paths import graph_json

    result = graph_json()
    assert result == Path(custom_json).resolve()


def test_explicit_briefs_dir_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: paths.briefs_dir explicit value overrides state_dir derivation."""
    custom_dir = str(tmp_path / "my-briefs")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  paths:\n    briefs_dir: '{custom_dir}'\n",
    )

    from fno.paths import briefs_dir

    result = briefs_dir()
    assert result == Path(custom_dir).resolve()


# ---------------------------------------------------------------------------
# AC1-HP: {vault} resolves when obsidian.enabled: true
# ---------------------------------------------------------------------------


def test_vault_template_resolves_when_obsidian_enabled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: {vault} in plans_dir resolves correctly when obsidian.enabled: true."""
    vault_dir = str(tmp_path / "my-vault")
    # Use {vault} (single braces) in YAML - not Python f-string interpolation
    # The f-string uses {{ }} to produce literal braces in the resulting string
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  plans_dir: '{{vault}}/plans'\n"
        f"  obsidian:\n    enabled: true\n    vault: '{vault_dir}'\n",
    )

    from fno.paths import plans_dir

    result = plans_dir(project_root=tmp_path)
    assert str(result).startswith(vault_dir)
    assert "plans" in str(result)


# ---------------------------------------------------------------------------
# AC1-EDGE: fleet_dir, postmortems_dir, worktrees_base, memory_dir all derive from state_dir
# ---------------------------------------------------------------------------


def test_all_global_dirs_derive_from_custom_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: All global dirs derive from state_dir when paths.* block is empty."""
    custom_dir = str(tmp_path / "mystate")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_dir}'\n",
    )

    from fno.paths import (
        fleet_dir,
        ledger_json,
        memory_dir,
        postmortems_dir,
        worktrees_base,
    )

    base = Path(custom_dir).resolve()
    assert fleet_dir() == base / "fleet"
    assert postmortems_dir() == base / "postmortems"
    assert worktrees_base() == base / "worktrees"
    assert memory_dir() == base / "memory"
    assert ledger_json() == base / "ledger.json"


# ---------------------------------------------------------------------------
# AC1-HP: inbox_dir with explicit override
# ---------------------------------------------------------------------------


def test_explicit_inbox_dir_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: paths.inbox_dir explicit value overrides project-relative default."""
    custom_dir = str(tmp_path / "global-inbox")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  paths:\n    inbox_dir: '{custom_dir}'\n",
    )

    from fno.paths import inbox_dir

    result = inbox_dir()
    assert result == Path(custom_dir).resolve()


def test_inbox_dir_override_honors_project_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding D (P2): inbox_dir override with a relative path honors project_root.

    When paths.inbox_dir is set to a relative override, calling
    inbox_dir(project_root=X) must anchor the relative path to X, not CWD.
    """
    project_root = tmp_path / "myproject"
    project_root.mkdir()
    # Set a relative inbox_dir override (no / or ~ prefix)
    relative_override = "custom-inbox"
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  paths:\n    inbox_dir: '{relative_override}'\n",
    )

    from fno.paths import inbox_dir

    result = inbox_dir(project_root=project_root)
    expected = (project_root / relative_override).resolve()
    assert result == expected, (
        f"inbox_dir override must be anchored to project_root={project_root}, "
        f"expected {expected}, got {result}"
    )


# ---------------------------------------------------------------------------
# AC1-HP: config_file is always inside state_dir
# ---------------------------------------------------------------------------


def test_config_file_inside_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: config_file() returns the actual loaded settings.yaml path.

    Finding 3 (P1): config_file() must return the path the loader USED, not
    a re-derived path from state_dir. If FNO_CONFIG points to tmp_path/settings.yaml,
    config_file() must return that path - even if state_dir is overridden to something else.
    """
    settings_file = tmp_path / "settings.yaml"
    custom_dir = str(tmp_path / "mystate")
    settings_file.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_dir}'\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.paths import config_file

    # config_file() must return the actual path that was loaded, NOT custom_dir/settings.yaml
    result = config_file()
    assert result == settings_file.resolve(), (
        f"config_file() should return the loaded path {settings_file}, got {result}"
    )


def test_config_file_loaded_from_is_preferred_over_state_dir_derivation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC3-VERIFY: paths.config_file() prefers loaded_from over state_dir derivation.

    When FNO_CONFIG=/some/path/settings.yaml and that file sets state_dir=/other/,
    config_file() must return /some/path/settings.yaml, NOT /other/settings.yaml.
    This prevents the chicken-and-egg inconsistency between the loader and paths.
    """
    settings_file = tmp_path / "explicit-settings.yaml"
    other_dir = tmp_path / "other-state"
    settings_file.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: '{other_dir}/'\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    # Trigger load
    config_mod.load_settings()

    from fno.paths import config_file
    result = config_file()

    # Must be the LOADED path, not other_dir/settings.yaml
    assert result == settings_file.resolve(), (
        f"config_file() returned {result}, but should have returned {settings_file}"
    )
    assert result != (other_dir / "settings.yaml").resolve(), (
        "config_file() must not re-derive from state_dir when loaded_from is available"
    )


# ---------------------------------------------------------------------------
# Finding 1 (Gemini HIGH): _resolve anchors relative paths to project_root
# ---------------------------------------------------------------------------


def test_resolve_relative_path_anchors_to_project_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: _resolve('./plans/x', project_root=/foo) returns /foo/plans/x.

    A relative path template (no leading /, ~, $, or {}) should resolve
    relative to project_root when supplied, NOT to CWD.
    """
    import fno.paths as paths_mod

    project_root = tmp_path / "my_project"
    project_root.mkdir()

    # Call _resolve with a relative path and an explicit project_root
    result = paths_mod._resolve("./plans/x", project_root=project_root)
    assert result == (project_root / "plans" / "x").resolve()


def test_resolve_relative_path_without_dot_anchors_to_project_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: bare relative path (no ./) anchors to project_root."""
    import fno.paths as paths_mod

    project_root = tmp_path / "repo"
    project_root.mkdir()

    result = paths_mod._resolve("plans/x", project_root=project_root)
    assert result == (project_root / "plans" / "x").resolve()


# ---------------------------------------------------------------------------
# handoffs_dir() resolver (ab-3f6def07)
# ---------------------------------------------------------------------------


def test_handoffs_dir_default_with_obsidian_enabled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Default: <vault>/internal/<project>/handoffs/ when obsidian is enabled."""
    vault_dir = tmp_path / "my-vault"
    vault_dir.mkdir()
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\n"
        "config:\n"
        f"  obsidian:\n    enabled: true\n    vault: '{vault_dir}'\n"
        "  project:\n    id: 'myproj'\n",
    )

    from fno.paths import handoffs_dir

    result = handoffs_dir()
    assert result == (vault_dir / "internal" / "myproj" / "handoffs").resolve()


def test_handoffs_dir_explicit_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """paths.handoffs_dir explicit value overrides every default branch."""
    custom_dir = tmp_path / "my-handoffs"
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\n"
        "config:\n"
        f"  paths:\n    handoffs_dir: '{custom_dir}'\n",
    )

    from fno.paths import handoffs_dir

    result = handoffs_dir()
    assert result == custom_dir.resolve()


def test_handoffs_dir_fallback_when_no_vault(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No vault configured: state_dir/handoffs/<project>/."""
    custom_state = tmp_path / "mystate"
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: '{custom_state}'\n"
        "  project:\n    id: 'myproj'\n",
    )

    from fno.paths import handoffs_dir

    result = handoffs_dir()
    assert result == (custom_state / "handoffs" / "myproj").resolve()


def test_handoffs_dir_bare_vault_name_anchors_at_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Regression test for ab-347f6482.

    obsidian.vault is conventionally a bare vault name (e.g. 'myvault')
    mapping to ~/<name> - the semantics vault_root() already implements.
    _resolve()'s {vault} substitution returned the raw relative value, so
    the assembled path anchored at project_root (the current worktree)
    and every pre-promise handoff landed at a junk worktree-local path.
    """
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setenv("HOME", str(fake_home))
    worktree = tmp_path / "conductor" / "loc-ratchet"
    worktree.mkdir(parents=True)
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\n"
        "config:\n"
        "  obsidian:\n    enabled: true\n    vault: 'myvault'\n"
        "  project:\n    id: 'myproj'\n",
    )

    from fno.paths import handoffs_dir

    result = handoffs_dir(project_root=worktree)
    expected = (fake_home / "myvault" / "internal" / "myproj" / "handoffs").resolve()
    assert result == expected, f"bare vault name must anchor at $HOME, got {result}"
    assert not str(result).startswith(str(worktree)), (
        "handoffs_dir must never anchor inside the worktree"
    )


# ---------------------------------------------------------------------------
# x-2e75: stable project-folder identity for internal/<project>/ paths.
# When config.project.id is unset, derive the folder name from the git remote
# (stable across worktrees/clones) instead of the checkout basename, which
# sprawls N stray internal/<name>/ folders in a shared vault. Sanitized so a
# derived or configured name can never escape internal/<project>/.
# ---------------------------------------------------------------------------


def _git_init_with_remote(d: Path, url: str | None) -> None:
    """Init a git repo at ``d`` with an optional origin remote."""
    import subprocess

    d.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q"], cwd=d, check=True)
    if url is not None:
        subprocess.run(["git", "remote", "add", "origin", url], cwd=d, check=True)


_VAULT_SETTINGS = (
    "schema_version: 1\nconfig:\n"
    "  obsidian:\n    enabled: true\n    vault: '{vault}'\n"
)


@pytest.mark.parametrize(
    "url,expected",
    [
        ("git@github.com:org/footnote.git", "footnote"),
        ("https://github.com/org/footnote.git", "footnote"),
        ("https://github.com/org/footnote", "footnote"),
        ("/srv/git/repo.git", "repo"),
        ("git@github.com:org/footnote.git/", "footnote"),
        (r"C:\repos\footnote.git", None),  # backslash tail -> reject, fall to basename
        ("", None),
        ("   ", None),
    ],
)
def test_remote_url_to_slug(url: str, expected: str | None) -> None:
    """Parser takes the last '/'-or-':' segment and strips one trailing .git."""
    from fno.paths import _remote_url_to_slug

    assert _remote_url_to_slug(url) == expected


def test_handoffs_dir_uses_git_remote_slug_not_basename(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Scenario 1: git remote drives the folder name, not the checkout basename."""
    checkout = tmp_path / "fno-attest-placement"
    _git_init_with_remote(checkout, "git@github.com:org/footnote.git")
    vault = tmp_path / "vault"
    _set_settings(monkeypatch, tmp_path, _VAULT_SETTINGS.format(vault=vault))
    monkeypatch.setattr("fno.paths._warned_unset_project_id", False, raising=False)

    from fno.paths import handoffs_dir

    result = handoffs_dir(project_root=checkout)
    assert str(result).endswith("internal/footnote/handoffs")
    assert "fno-attest-placement" not in str(result)


def test_two_worktrees_same_remote_share_one_folder(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Scenario 2: two differently-named worktrees of one repo share one folder."""
    athens = tmp_path / "athens"
    milan = tmp_path / "milan-v1"
    _git_init_with_remote(athens, "git@github.com:org/footnote.git")
    _git_init_with_remote(milan, "git@github.com:org/footnote.git")
    vault = tmp_path / "vault"
    _set_settings(monkeypatch, tmp_path, _VAULT_SETTINGS.format(vault=vault))
    monkeypatch.setattr("fno.paths._warned_unset_project_id", False, raising=False)

    from fno.paths import observer_reports_dir

    ra = observer_reports_dir(project_root=athens)
    rb = observer_reports_dir(project_root=milan)
    assert ra == rb
    assert str(ra).endswith("internal/footnote/observer-reports")


def test_no_remote_falls_back_to_basename(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Scenario 3: no origin remote falls back to basename without crashing."""
    checkout = tmp_path / "scratch"
    _git_init_with_remote(checkout, None)  # no remote
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '.fno/plans/{project}'\n",
    )

    from fno.paths import plans_dir

    result = plans_dir(project_root=checkout)
    assert result.name == "scratch"


def test_traversal_project_id_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Scenario 4: a traversal-bearing config.project.id is rejected (raises)."""
    vault = tmp_path / "vault"
    _set_settings(
        monkeypatch,
        tmp_path,
        _VAULT_SETTINGS.format(vault=vault) + "  project:\n    id: '../../etc'\n",
    )

    from fno.paths import handoffs_dir

    with pytest.raises(ValueError):
        handoffs_dir(project_root=tmp_path)


def test_configured_project_id_honored_unchanged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Scenario 5: a configured project.id wins over the remote-derived slug."""
    checkout = tmp_path / "fno-attest-placement"
    _git_init_with_remote(checkout, "git@github.com:org/footnote.git")
    vault = tmp_path / "vault"
    _set_settings(
        monkeypatch,
        tmp_path,
        _VAULT_SETTINGS.format(vault=vault) + "  project:\n    id: 'fno'\n",
    )

    from fno.paths import handoffs_dir

    result = handoffs_dir(project_root=checkout)
    assert str(result).endswith("internal/fno/handoffs")


def test_unset_project_id_warns_once_per_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Scenario 6: the unset-id nudge fires at most once per process."""
    checkout = tmp_path / "fno-attest-placement"
    _git_init_with_remote(checkout, "git@github.com:org/footnote.git")
    vault = tmp_path / "vault"
    _set_settings(monkeypatch, tmp_path, _VAULT_SETTINGS.format(vault=vault))
    monkeypatch.setattr("fno.paths._warned_unset_project_id", False, raising=False)

    from fno.paths import handoffs_dir

    for _ in range(3):
        handoffs_dir(project_root=checkout)
    err = capsys.readouterr().err
    assert err.count("fno: warning:") == 1
    assert "config.project.id" in err


def test_project_template_uses_remote_slug_in_non_vault_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Blast radius: {project} in a plain config.paths.* value (no vault) also
    resolves to the stable remote slug, not the checkout basename."""
    checkout = tmp_path / "athens"
    _git_init_with_remote(checkout, "https://github.com/org/footnote.git")
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '.fno/plans/{project}'\n",
    )

    from fno.paths import plans_dir

    result = plans_dir(project_root=checkout)
    assert result.name == "footnote"
    assert "athens" not in result.name
