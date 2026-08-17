"""Tests for `fno config get <dotted.key>` (ab-e9c81ed3, C1 fallback read).

Lets a skill / LLM caller read a single resolved config value without
re-implementing settings traversal. Used by /blueprint to resolve the
config.blueprint.max_prs_per_epic decomposition ceiling fallback.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner


def _write_settings(tmp_path: Path, content: str) -> Path:
    d = tmp_path / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    f = d / "settings.yaml"
    f.write_text(content, encoding="utf-8")
    return f


def _run(args, tmp_path, monkeypatch, settings_content):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    f = _write_settings(tmp_path, settings_content)
    monkeypatch.setenv("FNO_CONFIG", str(f))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    from fno.cli import app

    return CliRunner().invoke(app, args)


def test_get_default_value(tmp_path, monkeypatch):
    r = _run(
        ["config", "get", "config.blueprint.max_prs_per_epic"],
        tmp_path, monkeypatch, "schema_version: 1\n",
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "4"


def test_get_overridden_value(tmp_path, monkeypatch):
    r = _run(
        ["config", "get", "config.blueprint.max_prs_per_epic"],
        tmp_path, monkeypatch,
        "schema_version: 1\nconfig:\n  blueprint:\n    max_prs_per_epic: 9\n",
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "9"


def test_get_unknown_key_exits_nonzero(tmp_path, monkeypatch):
    r = _run(
        ["config", "get", "config.blueprint.no_such_field"],
        tmp_path, monkeypatch, "schema_version: 1\n",
    )
    assert r.exit_code != 0
    assert "no_such_field" in r.output or "unknown" in r.output.lower()


def test_get_scalar_top_level(tmp_path, monkeypatch):
    r = _run(
        ["config", "get", "schema_version"],
        tmp_path, monkeypatch, "schema_version: 1\n",
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "1"


# ---------------------------------------------------------------------------
# config.agents.confirm posture knob (ab-27541df5, US4; namespace moved from
# config.dispatch.confirm to config.agents.confirm in ab-f1b0ccd1)
# ---------------------------------------------------------------------------


def test_agents_confirm_resolves_default_auto(tmp_path, monkeypatch):
    """AC4-HP: a settings.yaml with no agents block resolves to `auto`."""
    r = _run(
        ["config", "get", "config.agents.confirm"],
        tmp_path, monkeypatch, "schema_version: 1\n",
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "auto"


def test_agents_confirm_override(tmp_path, monkeypatch):
    """An explicit posture is read back verbatim."""
    r = _run(
        ["config", "get", "config.agents.confirm"],
        tmp_path, monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    confirm: never\n",
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "never"


def test_agents_confirm_invalid_enum_fails_read(tmp_path, monkeypatch):
    """AC4-FR: a typo (`atuo`) fails the read; never silently relaxes to never."""
    r = _run(
        ["config", "get", "config.agents.confirm"],
        tmp_path, monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    confirm: atuo\n",
    )
    assert r.exit_code != 0
    assert r.stdout.strip() != "never"


# ---------------------------------------------------------------------------
# Optional leading `config.` prefix (x-8b64 E): `review.required_bots` is
# retried as `config.review.required_bots`. The review gate defaults to that
# key but the shorthand used to error "unknown config key".
# ---------------------------------------------------------------------------

_BOTS_SETTINGS = (
    "schema_version: 1\nconfig:\n  review:\n    required_bots:\n"
    "      - chatgpt-codex-connector\n"
)


def test_get_review_required_bots_shorthand(tmp_path, monkeypatch):
    """`review.required_bots` (no `config.` prefix) resolves."""
    r = _run(
        ["config", "get", "review.required_bots"],
        tmp_path, monkeypatch, _BOTS_SETTINGS,
    )
    assert r.exit_code == 0, r.output
    assert "chatgpt-codex-connector" in r.stdout


def test_get_review_required_bots_full_path_still_works(tmp_path, monkeypatch):
    """The explicit `config.` prefix is unchanged."""
    r = _run(
        ["config", "get", "config.review.required_bots"],
        tmp_path, monkeypatch, _BOTS_SETTINGS,
    )
    assert r.exit_code == 0, r.output
    assert "chatgpt-codex-connector" in r.stdout


def test_get_unknown_key_without_prefix_still_errors(tmp_path, monkeypatch):
    """The prefix fallback must not mask a genuinely unknown key."""
    r = _run(
        ["config", "get", "review.no_such_field"],
        tmp_path, monkeypatch, "schema_version: 1\n",
    )
    assert r.exit_code != 0
    assert "no_such_field" in r.output or "unknown" in r.output.lower()


# ---------------------------------------------------------------------------
# Source printing (x-4be1): `fno config get` names WHICH FILE decided the
# value. The reported defect: home config set auto_merge.enabled=false, the
# project set true, and `config get` answered True with no indication that the
# project layer had silently overridden the operator kill switch. The rename
# alone does not fix that; the source line does. Value stays ALONE on stdout
# (normalize.sh pipes the whole stream through tr); the source line, including
# an overrides clause exactly when a lower-precedence file also sets the key,
# is stderr-only.
# ---------------------------------------------------------------------------


def _pin_two_layers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, project: str, global_: str
) -> None:
    """Project + global config.toml chain (worktree==canonical==tmp project)."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    proj = tmp_path / "proj" / ".fno" / "config.toml"
    proj.parent.mkdir(parents=True, exist_ok=True)
    proj.write_text(project, encoding="utf-8")
    glob = tmp_path / "global-config.toml"
    glob.write_text(global_, encoding="utf-8")

    import fno.paths as paths_mod
    from fno import config as config_mod

    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: tmp_path / "proj")
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: tmp_path / "proj")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(glob))
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_get_prints_deciding_file_and_overridden_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The reported defect, end to end: value on stdout; decider + overridden
    home file on stderr."""
    _pin_two_layers(
        tmp_path,
        monkeypatch,
        project="[auto_merge]\nenabled = true\n",
        global_="[auto_merge]\nenabled = false\n",
    )
    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "get", "auto_merge.enabled"])
    assert r.exit_code == 0
    # stdout carries the value and NOTHING else (normalize.sh pipes it whole).
    assert r.stdout.strip() == "True"
    assert "source:" not in r.stdout
    # stderr names the project file as decider and the home file as overridden.
    assert f"source: {(tmp_path / 'proj' / '.fno' / 'config.toml')}" in r.stderr
    assert f"overrides {(tmp_path / 'global-config.toml')}" in r.stderr


def test_get_source_line_without_lower_override_has_no_overrides_clause(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The overrides clause appears exactly when a lower file also sets the key."""
    _pin_two_layers(
        tmp_path,
        monkeypatch,
        project="[auto_merge]\nenabled = true\n",
        global_="schema_version = 1\n",
    )
    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "get", "auto_merge.enabled"])
    assert r.exit_code == 0
    assert "source:" in r.stderr
    assert "overrides" not in r.stderr


def test_get_default_value_reports_no_source_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No file sets the key -> the value is a built-in default; the source line
    says so instead of naming a file that did not decide anything."""
    _pin_two_layers(
        tmp_path,
        monkeypatch,
        project="schema_version = 1\n",
        global_="schema_version = 1\n",
    )
    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "get", "auto_merge.merge_strategy"])
    assert r.exit_code == 0
    assert r.stdout.strip() == "merge"
    assert "default" in r.stderr
    assert "source:" in r.stderr


def test_get_json_carries_source_and_overrides(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """--json: one object with key/value/source/overrides for machine callers."""
    _pin_two_layers(
        tmp_path,
        monkeypatch,
        project="[auto_merge]\nenabled = true\n",
        global_="[auto_merge]\nenabled = false\n",
    )
    import json as _json

    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "get", "auto_merge.enabled", "--json"])
    assert r.exit_code == 0
    payload = _json.loads(r.stdout)
    assert payload["key"] == "auto_merge.enabled"
    assert payload["value"] is True
    assert payload["source"].endswith("proj/.fno/config.toml")
    assert len(payload["overrides"]) == 1
    assert payload["overrides"][0].endswith("global-config.toml")


def test_get_reports_file_holding_legacy_spelling(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A value that arrived through the deprecated dispatch.auto_merge reports
    the file that actually holds it, not a phantom canonical file."""
    _pin_two_layers(
        tmp_path,
        monkeypatch,
        project="[dispatch]\nauto_merge = true\n",
        global_="[auto_merge]\ngrant = \"none\"\n",
    )
    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "get", "auto_merge.grant"])
    assert r.exit_code == 0
    assert r.stdout.strip() == "dispatch"
    assert f"source: {(tmp_path / 'proj' / '.fno' / 'config.toml')}" in r.stderr
    # The global's canonical 'none' was overridden by the project's legacy true.
    assert "overrides" in r.stderr


def test_get_block_key_does_not_claim_one_decider(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A block get merges leaves from several files; naming one decider for the
    whole block would recreate the confusion this command exists to end."""
    _pin_two_layers(
        tmp_path,
        monkeypatch,
        project="[auto_merge]\nenabled = true\n",
        global_="[auto_merge]\nmerge_strategy = \"squash\"\n",
    )
    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "get", "auto_merge"])
    assert r.exit_code == 0
    assert "mixed" in r.stderr
    assert "overrides" not in r.stderr
    # And the pointer suggests a leaf query.
    assert "auto_merge.enabled" in r.stderr
