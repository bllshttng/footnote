"""Tests for the starship provenance-module installer (node x-84a8).

Covers the plan's acceptance criteria:
  AC(happy)  the snippet appends to starship.toml (creating it if absent)
  AC(error)  a declined offer leaves starship.toml untouched, snippet path printed
  idempotent a second install detects the module and skips (no duplicate)
  boundary   a missing snippet installs nothing

Run: cd cli && uv run pytest src/fno/setup/test_starship.py -v
"""
from __future__ import annotations

from pathlib import Path

from fno.setup.starship import _MODULE_MARKER, install_starship_module


def _snippet(tmp: Path) -> Path:
    p = tmp / "starship-fno.toml"
    p.write_text(f"# banner\n{_MODULE_MARKER}\nwhen = '[ -n \"$FNO_NODE\" ]'\n")
    return p


def test_appends_into_new_config(tmp_path: Path) -> None:
    snippet = _snippet(tmp_path)
    tgt = tmp_path / ".config" / "starship.toml"  # parent does not exist yet

    result = install_starship_module(snippet, tgt)

    assert result.action == "appended"
    assert tgt.is_file()
    assert _MODULE_MARKER in tgt.read_text()


def test_appends_after_existing_config_without_fusing(tmp_path: Path) -> None:
    snippet = _snippet(tmp_path)
    tgt = tmp_path / "starship.toml"
    tgt.write_text("[character]\nsuccess_symbol = '>'")  # no trailing newline

    install_starship_module(snippet, tgt)

    body = tgt.read_text()
    assert "[character]" in body and _MODULE_MARKER in body
    # A blank line separates the prior content from the appended module.
    assert "\n\n# banner" in body


def test_idempotent_skips_when_already_present(tmp_path: Path) -> None:
    snippet = _snippet(tmp_path)
    tgt = tmp_path / "starship.toml"

    install_starship_module(snippet, tgt)
    second = install_starship_module(snippet, tgt)

    assert second.action == "already"
    assert tgt.read_text().count(_MODULE_MARKER) == 1  # no duplicate module


def test_missing_snippet_installs_nothing(tmp_path: Path) -> None:
    result = install_starship_module(tmp_path / "nope.toml", tmp_path / "starship.toml")
    assert result.action == "missing-snippet"
    assert not (tmp_path / "starship.toml").exists()


def test_default_snippet_resolves_colocated() -> None:
    from fno.setup.starship import default_snippet_source

    src = default_snippet_source()
    assert src is not None and src.is_file()
    assert _MODULE_MARKER in src.read_text()


def test_wizard_capstone_opt_in(tmp_path: Path) -> None:
    """The setup capstone prompts, default No; declined leaves the file absent
    and prints the snippet path, accepted appends the module."""
    from fno.setup_cli import offer_starship_module

    snippet = _snippet(tmp_path)
    tgt = tmp_path / "starship.toml"
    printed: list[str] = []

    declined = offer_starship_module(
        confirm_fn=lambda _m: False, echo_fn=printed.append,
        snippet_path=snippet, target_toml=tgt,
    )
    assert declined["installed"] is False
    assert not tgt.exists()
    assert any(str(snippet) in line for line in printed)  # path printed for manual use

    accepted = offer_starship_module(
        confirm_fn=lambda _m: True, snippet_path=snippet, target_toml=tgt,
    )
    assert accepted["installed"] is True
    assert _MODULE_MARKER in tgt.read_text()
