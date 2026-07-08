"""Tests for `fno config set` / fno.config.writer (ab-098967b4, US7).

Covers AC7-HP (set + reflected), AC7-ERR (invalid rejected, unchanged),
AC7-UI (stdout confirms value + scope), AC7-EDGE (lock-serialized, no
corruption across two writes), AC7-FR (mid-write failure leaves file intact).

Stage 3 (x-8526): the on-disk file is flat ``config.toml`` (no ``config:``
wrapper), so ``_read`` returns a flat dict and paths carry no ``config`` key.
"""
from __future__ import annotations

import tomllib

import pytest
from typer.testing import CliRunner

from fno.config.writer import ConfigSetError, set_config_value


def _read(tmp_path):
    return tomllib.loads((tmp_path / ".fno" / "config.toml").read_text())


def test_ac7_hp_set_writes_coerced_value(tmp_path):
    res = set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    assert res.value is False
    assert _read(tmp_path)["agents"]["a2a"]["auto"] is False


def test_set_int_coercion(tmp_path):
    res = set_config_value(
        "config.agents.a2a.turn_ceiling", "10", scope="project", repo_root=tmp_path
    )
    assert res.value == 10
    assert _read(tmp_path)["agents"]["a2a"]["turn_ceiling"] == 10


def test_ac7_err_invalid_rejected_unchanged(tmp_path):
    # Seed an existing valid value.
    set_config_value(
        "config.agents.a2a.turn_ceiling", "6", scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "config.toml").read_text()
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.agents.a2a.turn_ceiling", "0", scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 2
    assert "turn_ceiling" in str(exc.value)
    # File unchanged (AC7-ERR).
    assert (tmp_path / ".fno" / "config.toml").read_text() == before


def test_unknown_key_rejected(tmp_path):
    with pytest.raises(ConfigSetError) as exc:
        set_config_value("config.nope.bogus", "x", scope="project", repo_root=tmp_path)
    assert exc.value.exit_code == 1
    assert "unknown config key" in str(exc.value)
    assert not (tmp_path / ".fno" / "config.toml").exists()


def test_setting_a_block_rejected(tmp_path):
    with pytest.raises(ConfigSetError) as exc:
        set_config_value("config.agents.a2a", "x", scope="project", repo_root=tmp_path)
    assert "block" in str(exc.value)


def test_bad_bool_rejected(tmp_path):
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.agents.a2a.auto", "maybe", scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 2


def test_ac7_edge_two_writes_preserve_each_other(tmp_path):
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    set_config_value(
        "config.agents.a2a.turn_ceiling", "9", scope="project", repo_root=tmp_path
    )
    data = _read(tmp_path)
    # The second write preserved the first key (no clobber / corruption).
    assert data["agents"]["a2a"]["auto"] is False
    assert data["agents"]["a2a"]["turn_ceiling"] == 9


def test_preserves_unrelated_keys(tmp_path):
    # Seed a legacy wrapped settings.yaml; the writer migrates it to a flat
    # config.toml on first write, preserving unrelated top-level keys.
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        "schema_version: 1\nwork:\n  workspaces:\n    ws:\n      projects:\n"
        "      - name: p\n        path: /tmp/p\n",
        encoding="utf-8",
    )
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    data = _read(tmp_path)
    # Unrelated top-level keys survive the migrate + rewrite; the legacy yaml is
    # gone (hard cut).
    assert data["schema_version"] == 1
    assert data["work"]["workspaces"]["ws"]["projects"][0]["name"] == "p"
    assert data["agents"]["a2a"]["auto"] is False
    assert not settings.exists()


def test_ac7_fr_midwrite_failure_leaves_intact(tmp_path, monkeypatch):
    set_config_value(
        "config.agents.a2a.auto", "true", scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "config.toml").read_text()

    import fno.config.writer as writer_mod

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(writer_mod.tomli_w, "dumps", _boom)
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 1
    # Original intact, no leftover temp files (AC7-FR).
    assert (tmp_path / ".fno" / "config.toml").read_text() == before
    leftovers = list((tmp_path / ".fno").glob(".config.toml.tmp.*"))
    assert leftovers == []


def test_ac7_ui_cli_confirms_value_and_scope(tmp_path, monkeypatch):
    # Point the global settings path at a tmp file so scope=global is isolated.
    gpath = tmp_path / "global-settings.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    res = CliRunner().invoke(app, ["set", "config.agents.a2a.auto", "false"])
    assert res.exit_code == 0, res.output
    assert "config.agents.a2a.auto" in res.output
    assert "False" in res.output
    assert "global" in res.output
    written = tomllib.loads((gpath.parent / "config.toml").read_text())
    assert written["agents"]["a2a"]["auto"] is False


def test_ac7_err_cli_invalid_exit_nonzero(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "g.yaml"))
    from fno.config_cli import app

    res = CliRunner().invoke(app, ["set", "config.agents.a2a.turn_ceiling", "0"])
    assert res.exit_code == 2
    assert "error:" in res.output


def test_pep604_union_unwrap():
    """gemini review: _unwrap_optional handles both typing.Optional and the
    PEP 604 `X | None` syntax."""
    import typing

    from fno.config.writer import _unwrap_optional

    assert _unwrap_optional(int | None) is int
    assert _unwrap_optional(typing.Optional[int]) is int
    assert _unwrap_optional(int) is int


def test_set_writes_through_symlinked_config_to_canonical(tmp_path):
    """A worktree's .fno/config.toml is a symlink to the canonical checkout's
    real file (setup-worktree.sh links it). `config set --local` from the
    worktree must write THROUGH the symlink to canonical, preserving the link --
    NOT clobber it into a divergent regular file. `os.replace` (rename) onto a
    symlink replaces the link itself, not its referent, so the naive atomic
    write breaks the link and diverges from canonical.
    """
    import os

    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree"
    (canonical / ".fno").mkdir(parents=True)
    (worktree / ".fno").mkdir(parents=True)

    # Canonical holds the real file (flat config.toml), seeded with a value.
    canon_config = canonical / ".fno" / "config.toml"
    canon_config.write_text(
        "[agents.a2a]\nturn_ceiling = 6\n",
        encoding="utf-8",
    )
    # The worktree's config.toml is a symlink pointing at canonical's file.
    wt_config = worktree / ".fno" / "config.toml"
    wt_config.symlink_to(canon_config)

    res = set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=worktree
    )
    assert res.value is False

    # The symlink is preserved (not replaced by a regular file).
    assert wt_config.is_symlink()
    assert os.path.realpath(wt_config) == os.path.realpath(canon_config)

    # Canonical's real file received the new value AND kept the seeded one.
    canon_data = tomllib.loads(canon_config.read_text())
    assert canon_data["agents"]["a2a"]["auto"] is False
    assert canon_data["agents"]["a2a"]["turn_ceiling"] == 6


def test_locked_update_holds_lock_across_read_and_mutate(tmp_path):
    """TOCTOU regression (codex P2, PR #522): the lock must cover the whole
    read-modify-write cycle, not just the final os.replace. Otherwise two
    concurrent `set` writers both parse the same old config and the later replace
    clobbers the earlier writer's key. Proven by attempting a competing
    non-blocking flock from inside `mutate` (which runs after the read): it must
    be denied because `_locked_update` already holds the lock at that point.
    """
    import fcntl

    from fno.config.writer import _locked_update

    target = tmp_path / "config.toml"
    target.write_text("a = 1\n", encoding="utf-8")
    lock_path = target.with_suffix(target.suffix + ".lock")

    observed = {}

    def mutate(existing):
        # The read already happened; the lock must still be held here.
        observed["existing"] = dict(existing)
        with open(lock_path, "w") as competitor:
            try:
                fcntl.flock(competitor.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(competitor.fileno(), fcntl.LOCK_UN)
                observed["lock_held"] = False  # acquired -> NOT held (bug)
            except OSError:
                observed["lock_held"] = True  # denied -> held by _locked_update
        existing["b"] = 2
        return existing

    written = _locked_update(target, mutate)

    # mutate saw the on-disk content (read happened inside the locked cycle)...
    assert observed["existing"] == {"a": 1}
    # ...with the lock held throughout the read+mutate...
    assert observed["lock_held"] is True
    # ...and the merge landed without losing the pre-existing key.
    assert tomllib.loads(written.read_text()) == {"a": 1, "b": 2}


# --- list coercion (wizard must be able to set list keys) ---


def test_set_list_comma_separated(tmp_path):
    res = set_config_value(
        "config.review.external_reviewers",
        "gemini,codex",
        scope="project",
        repo_root=tmp_path,
    )
    assert res.value == ["gemini", "codex"]
    assert _read(tmp_path)["review"]["external_reviewers"] == [
        "gemini",
        "codex",
    ]


def test_set_list_json_array(tmp_path):
    res = set_config_value(
        "config.review.required_bots",
        '["chatgpt-codex-connector"]',
        scope="project",
        repo_root=tmp_path,
    )
    assert res.value == ["chatgpt-codex-connector"]


def test_set_list_empty_value(tmp_path):
    res = set_config_value(
        "config.review.external_reviewers", "", scope="project", repo_root=tmp_path
    )
    assert res.value == []


def test_set_list_single_item_round_trips(tmp_path):
    # Regression: a single reviewer must store as a 1-item list, not a bare
    # string that the model coercer would re-wrap.
    res = set_config_value(
        "config.review.external_reviewers", "gemini", scope="project", repo_root=tmp_path
    )
    assert res.value == ["gemini"]
    assert _read(tmp_path)["review"]["external_reviewers"] == ["gemini"]
