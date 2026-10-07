"""Tests for fno.paths typed resolver.

Task 1.2: Create paths.py typed resolver with template substitution.
Task 1.4: Round out coverage for all AC items.

All tests use tmp_path + monkeypatch isolation. An autouse fixture pins
FNO_REPO_ROOT to tmp_path so resolve_repo_root() is isolated
(feedback_fno_repo_root_leaks_between_tests memory entry).
"""
from __future__ import annotations

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
    # Clear paths caches (resolve_repo_root now @cached)
    yield
    # Clear again after test to avoid pollution
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


def test_resolve_repo_root_respects_fno_repo_root_env(
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


def test_fno_repo_root_warns_when_pinned_to_fno_from_foreign_repo(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """FNO_REPO_ROOT pinned at the fno plugin root + a different cwd repo
    emits a one-line stderr heads-up (the silent-wrong-project footgun)."""
    import fno.paths as paths_mod

    fno_dir = _make_plugin_root(tmp_path / "fno")
    other_repo = tmp_path / "acme-web"
    other_repo.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(fno_dir))
    # cwd (the pytest cwd) is not inside fno_dir, so the same-repo
    # short-circuit does not fire; the (stubbed) git probe reports other_repo.
    monkeypatch.setattr(paths_mod.subprocess, "run", _fake_git_toplevel(other_repo))

    result = paths_mod.resolve_repo_root()

    assert result == fno_dir.resolve()  # warning is non-fatal
    err = capsys.readouterr().err
    assert "FNO_REPO_ROOT pins" in err
    assert str(fno_dir.resolve()) in err


def test_fno_repo_root_no_warning_when_cwd_is_inside_the_pinned_repo(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Running inside the fno plugin root with FNO_REPO_ROOT pointing at
    it is not a footgun - same repo, no warning, and no git subprocess.

    This is the regression that broke the CLI-wrapper tests: those globally
    stub subprocess.run, so the warning path must NOT reach a git probe when
    cwd is inside the pinned root."""
    import fno.paths as paths_mod

    fno_dir = _make_plugin_root(tmp_path / "fno")
    monkeypatch.setenv("FNO_REPO_ROOT", str(fno_dir))
    monkeypatch.chdir(fno_dir)
    # subprocess.run stubbed WITHOUT stdout, like the CLI-wrapper tests. The
    # cwd-inside-resolved short-circuit must return before this is ever called;
    # if it isn't, accessing .stdout would raise.
    monkeypatch.setattr(
        paths_mod.subprocess, "run",
        lambda *a, **k: type("R", (), {"returncode": 0})(),
    )

    paths_mod.resolve_repo_root()
    assert "FNO_REPO_ROOT pins" not in capsys.readouterr().err


# ---------------------------------------------------------------------------
# resolve_canonical_repo_root() - config climbs to the main checkout
# ---------------------------------------------------------------------------


def test_resolve_canonical_repo_root_falls_back_when_git_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With no FNO_REPO_ROOT and git unavailable, fall back to resolve_repo_root()."""
    import fno.paths as paths_mod

    # Leave any real repo so the filesystem short-circuit in
    # resolve_canonical_worktree() stays out of the way and the stubbed
    # subprocess path is what runs.
    monkeypatch.chdir(tmp_path)
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

    # Leave any real repo so the filesystem short-circuit in
    # resolve_canonical_worktree() stays out of the way and the stubbed
    # porcelain parse is what runs.
    monkeypatch.chdir(tmp_path)
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
    # Must be under the state_dir (default ~/.fno/), in the db/ folder
    assert result.parent.name == "db"
    assert result.parent.parent.name == ".fno"


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


def test_global_events_json_pinned_global_ignores_relative_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    first = tmp_path / "clone-a"
    second = tmp_path / "clone-b"
    first.mkdir()
    second.mkdir()
    _set_settings(
        monkeypatch, tmp_path, "schema_version: 1\nconfig:\n  state_dir: .fno/\n"
    )

    from fno.paths import global_events_json

    monkeypatch.chdir(first)
    first_path = global_events_json()
    monkeypatch.chdir(second)
    second_path = global_events_json()

    assert first_path == second_path == (Path.home() / ".fno" / "events.jsonl").resolve()


def test_global_events_json_pins_relative_ledger_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    first = tmp_path / "clone-a"
    second = tmp_path / "clone-b"
    first.mkdir()
    second.mkdir()
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  paths:\n    ledger_json: state/ledger.json\n",
    )

    from fno.paths import global_events_json, ledger_json

    monkeypatch.chdir(first)
    first_paths = (ledger_json(), global_events_json())
    monkeypatch.chdir(second)
    second_paths = (ledger_json(), global_events_json())

    expected_ledger = (Path.home() / ".fno" / "state" / "ledger.json").resolve()
    assert first_paths == second_paths == (
        expected_ledger,
        expected_ledger.parent / "events.jsonl",
    )


def test_state_dir_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: state_dir() returns ~/.fno/ resolved to absolute."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import state_dir

    result = state_dir()
    assert isinstance(result, Path)
    assert result.is_absolute()
    assert result.name == ".fno"


def test_graphql_quota_lock_ignores_project_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """One GitHub identity needs one machine lock across all repositories."""
    machine_home = tmp_path / "home"
    monkeypatch.setenv("HOME", str(machine_home))
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{tmp_path / 'project-state'}'\n",
    )

    from fno.paths import graphql_quota_lock

    assert graphql_quota_lock() == machine_home / ".fno" / "locks" / "github-graphql-quota.lock"


# ---------------------------------------------------------------------------
# AC1-UI: No tilde in returned Path
# ---------------------------------------------------------------------------


def test_state_dir_no_tilde(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-UI: state_dir() returns a Path with no '~' in its string representation."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths import state_dir

    result = state_dir()
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
    assert result == Path(custom_dir).resolve() / "db" / "graph.json"


# ---------------------------------------------------------------------------
# AC1-FR: Cache lifetime (same object on second call)
# ---------------------------------------------------------------------------


def test_paths_cache_sees_a_settings_edit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The settings cache key fingerprints the file contents (x-b545): a
    same-key rewrite IS seen in-process, with no cache_clear. The edited
    state_dir must stay inside the sandbox so the hermetic guard permits it."""
    changed_dir = tmp_path / "changed"
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno.paths import graph_json

    first = graph_json()
    # Rewrite the file - the cache reparses and the new state_dir resolves.
    settings_file.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: '{changed_dir}'\n",
        encoding="utf-8",
    )
    second = graph_json()
    assert second == changed_dir.resolve() / "db" / "graph.json"
    assert first != second, "a settings edit must invalidate the keyed cache"


# ---------------------------------------------------------------------------
# AC1-EDGE: {{ }} escape sequences
# ---------------------------------------------------------------------------


def test_double_brace_escape_in_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {{personal}} in state_dir becomes {personal} in resolved path."""
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
# AC1-EDGE: Unknown {foo} variable rejected at resolve time
# ---------------------------------------------------------------------------


def test_unknown_template_variable_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {foo} in a path raises a hard error at resolve time."""
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  state_dir: '/home/{foo}/fno'\n",
    )

    from fno.paths import state_dir

    with pytest.raises(Exception, match=r"\{foo\}|unknown.*variable|unrecognized"):
        state_dir()


# ---------------------------------------------------------------------------
# AC1-HP: paths.* explicit override wins over state_dir derivation
# ---------------------------------------------------------------------------


def test_graph_json_uses_state_dir_even_if_the_removed_override_is_present(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The retired paths.graph_json key cannot move the store anchor."""
    custom_dir = str(tmp_path / "state")
    custom_json = str(tmp_path / "custom" / "g.json")
    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_dir}'\n  paths:\n    graph_json: '{custom_json}'\n",
    )

    from fno.paths import graph_json

    result = graph_json()
    assert result == Path(custom_dir).resolve() / "db" / "graph.json"


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
        global_events_json,
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
    assert global_events_json() == base / "events.jsonl"


# ---------------------------------------------------------------------------
# AC1-HP: inbox_dir with explicit override
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# x-fc25: FNO_STATE_DIR - the pinned carrier for the state root
# ---------------------------------------------------------------------------


def test_state_dir_honors_fno_state_dir_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The env carrier moves the state root ahead of the config default.

    seal_state_root pins this var around a forwarded HOME, so a worker on a
    non-claude oauth_dir account resolves the same graph.json its parent did.
    """
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "pinned"))

    from fno.paths import graph_json, state_dir

    assert state_dir() == (tmp_path / "pinned").resolve()
    assert graph_json() == (tmp_path / "pinned").resolve() / "db" / "graph.json"


def test_state_dir_empty_carrier_falls_back_to_config(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An empty value counts as unset, matching the FNO_AGENTS_HOME idiom."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_STATE_DIR", "")

    from fno.paths import state_dir

    assert state_dir().name == ".fno"


def test_locks_dir_honors_fno_state_dir_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """locks_dir honors the carrier: it is config-free, so both stamp and
    append writers still agree under it without loading settings."""
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "pinned"))

    from fno.paths import locks_dir

    assert locks_dir() == (tmp_path / "pinned").resolve() / "locks"


def test_ledger_json_honors_fno_state_dir_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A sealed worker's ledger follows the pinned root, never the raw
    relative-config fallback that would strand it under the moved HOME."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "pinned"))

    from fno.paths import ledger_json

    assert ledger_json() == (tmp_path / "pinned").resolve() / "ledger.json"


def test_migrate_from_checkout_refuses_the_state_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-EDGE: the state root is not a checkout journal. A session whose
    cwd was $HOME outside a checkout once moved the GLOBAL journal into a fake
    space behind a MOVED-TO pointer at the top level of the state root; the
    refusal keeps the pointer and the fake-space move from ever recurring."""
    state = tmp_path / ".fno"
    state.mkdir()
    old = state / "events.jsonl"
    old.write_text("global rows\n")
    new = tmp_path / "spaces" / "x" / "events.jsonl"
    monkeypatch.setenv("FNO_STATE_DIR", str(state))

    from fno.paths import migrate_from_checkout

    assert migrate_from_checkout(old, new) is False
    assert old.exists()
    assert not (state / "MOVED-TO").exists()


def test_agents_registry_path_follows_declared_agents_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: with FNO_AGENTS_HOME declared, a bare write_registry lands in
    the declared home, never the config state_dir (the 2026-09-27 probe
    overwrote the live registry through exactly this gap)."""
    from fno.agents.registry import AgentEntry, write_registry

    _set_settings(
        monkeypatch,
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{tmp_path / '.fno'}'\n",
    )
    declared = tmp_path / "other" / "agents"
    declared.mkdir(parents=True)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(declared))

    entry = AgentEntry(
        name="leader",
        cwd="/tmp/x",
        log_path="/tmp/x/log",
        harness="claude",
        harness_session_id="aaaaaaaa-0000-0000-0000-111111111111",
    )
    write_registry([entry])

    assert (declared / "registry.json").is_file()
    assert not (tmp_path / ".fno" / "agents" / "registry.json").exists()
