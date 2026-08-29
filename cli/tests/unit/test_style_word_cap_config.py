"""The rule 7 word cap becomes per-surface and config-driven.

Three properties are load-bearing here.

``style.py`` stays PURE. The module promises "no filesystem, no state, no
network" and every test in ``test_style_rules.py`` leans on it, so ``check``
takes the number as a keyword and the caller is what reads config. The tests
below pass ``word_cap`` explicitly for that reason.

The SURFACE still decides whether a cap applies at all; config only decides
what the number is. A surface outside ``CAPPED_SURFACES`` carries no cap at any
configured value, which is the case a naive "read config, apply cap"
implementation gets wrong.

An absent ``[style]`` block behaves exactly as today. That is the guarantee
that lets this land without touching any existing project.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import typer

from fno import style


def _words(n: int) -> str:
    """``n`` masked words that break no rule except, possibly, rule 7.

    One physical line (rule 6) of five-word sentences (rules 1 and 2). A flat
    run of ``n`` words would trip the 25-word paragraph cap first, and then a
    cap test would be reading a rule 1 refusal.
    """
    full, remainder = divmod(n, 5)
    sentences = ["word word word word word." for _ in range(full)]
    if remainder:
        sentences.append(" ".join("word" for _ in range(remainder)) + ".")
    return " ".join(sentences)


def _write_settings(tmp_path: Path, content: str) -> Path:
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    return settings_file


def _load(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, content: str):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return config_mod.load_settings()


@pytest.fixture(autouse=True)
def _clear_settings_cache():
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


# --- the pure surface --------------------------------------------------------


def test_a_lower_cap_refuses_and_the_refusal_names_the_number_enforced():
    """A refusal that names 80 while enforcing 40 teaches the sender nothing."""
    violations = style.check(_words(60), surface="mail", word_cap=40)
    wordcap = next(v for v in violations if v.rule == 7)
    assert "40" in wordcap.detail
    assert "60" in wordcap.detail


def test_a_higher_cap_permits_a_body_the_default_would_refuse():
    assert 7 not in {v.rule for v in style.check(_words(120), surface="mail", word_cap=200)}


def test_an_absent_word_cap_keeps_the_module_default():
    violations = style.check(_words(81), surface="mail")
    wordcap = next(v for v in violations if v.rule == 7)
    assert str(style.MESSAGE_WORD_CAP) in wordcap.detail


def test_an_uncapped_surface_ignores_any_configured_number():
    """The surface decides WHETHER; config decides only WHAT."""
    assert 7 not in {v.rule for v in style.check(_words(81), surface="pr-body", word_cap=10)}
    assert 7 not in {v.rule for v in style.check(_words(81), surface="markdown", word_cap=10)}


def test_the_encounter_surface_carries_the_cap():
    assert 7 in {v.rule for v in style.check(_words(81), surface="encounter")}
    assert "encounter" in style.CAPPED_SURFACES
    assert "mail" in style.CAPPED_SURFACES


def test_word_count_is_still_the_one_counter():
    """A second count implementation lets the refusal and the budget disagree."""
    body = _words(60)
    violations = style.check(body, surface="mail", word_cap=40)
    wordcap = next(v for v in violations if v.rule == 7)
    assert str(style.word_count(body)) in wordcap.detail


# --- the config leaves -------------------------------------------------------


def test_no_style_block_resolves_to_todays_numbers(tmp_path, monkeypatch):
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.style.word_cap.mail == style.MESSAGE_WORD_CAP
    assert settings.style.word_cap.encounter == style.MESSAGE_WORD_CAP
    assert settings.style.pair_budget_words == 80


def test_a_configured_cap_reads_back_per_surface(tmp_path, monkeypatch):
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  style:\n    word_cap:\n      mail: 40\n      encounter: 120\n",
    )
    assert settings.style.word_cap.mail == 40
    assert settings.style.word_cap.encounter == 120


def test_a_cap_below_one_is_refused_by_the_model(tmp_path, monkeypatch):
    from pydantic import ValidationError

    with pytest.raises(ValidationError):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  style:\n    word_cap:\n      mail: 0\n",
        )


def test_every_new_leaf_has_a_registry_entry():
    """CI fails on registry incompleteness; catch it here instead of there."""
    from fno.config.registry import FIELD_META

    for path in ("style.word_cap.mail", "style.word_cap.encounter", "style.pair_budget_words"):
        assert path in FIELD_META, f"{path} has no FIELD_META entry"


# --- the callers that resolve the number -------------------------------------


def test_the_mail_send_refusal_names_the_configured_cap(tmp_path, monkeypatch, capsys):
    """AC8. The number the sender is refused against is the number they set."""
    _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  style:\n    word_cap:\n      mail: 40\n",
    )
    monkeypatch.delenv("FNO_STYLE_ENFORCE", raising=False)

    from fno.mail import cli as mail_cli

    with pytest.raises(typer.Exit) as exc:
        mail_cli._enforce_style(_words(60))
    assert exc.value.exit_code == 1
    assert "40" in capsys.readouterr().err


def test_an_absent_style_block_leaves_mail_sending_byte_identical(tmp_path, monkeypatch):
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    monkeypatch.delenv("FNO_STYLE_ENFORCE", raising=False)

    from fno.mail import cli as mail_cli

    # 80 masked words is exactly at today's cap, so it must still pass.
    mail_cli._enforce_style(_words(80))


def test_the_rolling_pair_budget_reads_its_cap_from_config(tmp_path, monkeypatch):
    """The window instrument moves with the per-message cap or it binds first."""
    from fno.mail import budget

    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "state"))
    monkeypatch.setattr(budget, "_ledger_path", lambda pair: tmp_path / f"{pair}.json")

    with pytest.raises(budget.BudgetRefused) as exc:
        budget.reserve(
            sender="alpha",
            recipient="beta",
            words=50,
            msg_id="m1",
            cap=40,
        )
    assert exc.value.cap == 40
    assert "cap=40" in exc.value.marker()
