"""Tests for `fno config get <dotted.key>` (ab-e9c81ed3, C1 fallback read).

Lets a skill / LLM caller read a single resolved config value without
re-implementing settings traversal. Used by /blueprint to resolve the
config.blueprint.max_prs_per_epic decomposition ceiling fallback.
"""
from __future__ import annotations

from pathlib import Path

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
    assert r.output.strip() == "4"


def test_get_overridden_value(tmp_path, monkeypatch):
    r = _run(
        ["config", "get", "config.blueprint.max_prs_per_epic"],
        tmp_path, monkeypatch,
        "schema_version: 1\nconfig:\n  blueprint:\n    max_prs_per_epic: 9\n",
    )
    assert r.exit_code == 0, r.output
    assert r.output.strip() == "9"


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
    assert r.output.strip() == "1"


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
    assert r.output.strip() == "auto"


def test_agents_confirm_override(tmp_path, monkeypatch):
    """An explicit posture is read back verbatim."""
    r = _run(
        ["config", "get", "config.agents.confirm"],
        tmp_path, monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    confirm: never\n",
    )
    assert r.exit_code == 0, r.output
    assert r.output.strip() == "never"


def test_agents_confirm_invalid_enum_fails_read(tmp_path, monkeypatch):
    """AC4-FR: a typo (`atuo`) fails the read; never silently relaxes to never."""
    r = _run(
        ["config", "get", "config.agents.confirm"],
        tmp_path, monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    confirm: atuo\n",
    )
    assert r.exit_code != 0
    assert r.output.strip() != "never"


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
    assert "chatgpt-codex-connector" in r.output


def test_get_review_required_bots_full_path_still_works(tmp_path, monkeypatch):
    """The explicit `config.` prefix is unchanged."""
    r = _run(
        ["config", "get", "config.review.required_bots"],
        tmp_path, monkeypatch, _BOTS_SETTINGS,
    )
    assert r.exit_code == 0, r.output
    assert "chatgpt-codex-connector" in r.output


def test_get_unknown_key_without_prefix_still_errors(tmp_path, monkeypatch):
    """The prefix fallback must not mask a genuinely unknown key."""
    r = _run(
        ["config", "get", "review.no_such_field"],
        tmp_path, monkeypatch, "schema_version: 1\n",
    )
    assert r.exit_code != 0
    assert "no_such_field" in r.output or "unknown" in r.output.lower()
