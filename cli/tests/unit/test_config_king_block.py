"""``config.king`` reign keys: defaults present, degradation fail-safe (AC19-20).

The texts arm a self-injected /loop and /goal, so a bad value must degrade to
the default and be nameable, never raise: a config typo has no business ending
a reign at load time.
"""
from __future__ import annotations

from pathlib import Path

from fno.config import KING_CHECKIN_TEXT, KING_GOAL_TEXT, KingBlock


def test_ac19_defaults_with_no_config() -> None:
    block = KingBlock()
    assert block.checkin_interval == "30m"
    assert block.checkin_text == KING_CHECKIN_TEXT
    assert block.goal_text == KING_GOAL_TEXT
    # The defaults are the skill's own texts: they name the check-in body and
    # the never-clear rule, so a fresh install runs with no config.
    assert "reign check-in" in block.checkin_text
    assert "NoProgress" in block.goal_text


def test_ac19_registry_answers_config_get(tmp_path: Path, monkeypatch) -> None:
    """`fno config get king.checkin_interval` answers 30m from bare defaults."""
    from fno.config import load_settings
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    settings = load_settings()
    assert settings.king.checkin_interval == "30m"


def test_ac20_bad_interval_degrades_to_default() -> None:
    block = KingBlock(checkin_interval="not a duration")
    assert block.checkin_interval == "30m"
    # Valid shapes pass through verbatim.
    assert KingBlock(checkin_interval="15m").checkin_interval == "15m"
    assert KingBlock(checkin_interval="2h").checkin_interval == "2h"
    assert KingBlock(checkin_interval=45).checkin_interval == "30m"  # type: ignore[arg-type]


def test_ac20_blank_texts_degrade_and_do_not_raise() -> None:
    block = KingBlock(checkin_text="   ", goal_text=None)
    assert block.checkin_text == KING_CHECKIN_TEXT
    assert block.goal_text == KING_GOAL_TEXT
    custom = KingBlock(checkin_text="custom body", goal_text="custom goal")
    assert custom.checkin_text == "custom body"
    assert custom.goal_text == "custom goal"


def test_registry_lists_the_three_keys() -> None:
    from fno.config.registry import FIELD_META

    for key in (
        "king.checkin_interval",
        "king.checkin_text",
        "king.goal_text",
    ):
        assert key in FIELD_META
