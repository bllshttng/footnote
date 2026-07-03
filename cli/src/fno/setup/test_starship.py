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


def test_non_utf8_target_degrades_to_missing(tmp_path: Path) -> None:
    """A target starship.toml with non-UTF-8 bytes must not crash the wizard:
    the read is caught and reported as missing-snippet."""
    snippet = _snippet(tmp_path)
    tgt = tmp_path / "starship.toml"
    tgt.write_bytes(b"\xff\xfe\x00\x01")  # invalid UTF-8

    result = install_starship_module(snippet, tgt)

    assert result.action == "missing-snippet"


def test_default_snippet_resolves_colocated() -> None:
    from fno.setup.starship import default_shell_snippet_source, default_snippet_source

    src = default_snippet_source()
    assert src is not None and src.is_file()
    assert _MODULE_MARKER in src.read_text()

    # The portable shell renderer ships alongside it.
    sh = default_shell_snippet_source()
    assert sh is not None and sh.is_file()
    assert "FNO_NODE" in sh.read_text() and "PS1" in sh.read_text()


def test_shell_source_line_appends_and_is_idempotent(tmp_path: Path) -> None:
    """The portable renderer adds one `source` line to the rc, idempotently, and
    never rewrites existing content."""
    from fno.setup.starship import install_shell_source_line

    snippet = tmp_path / "prompt-fno.sh"
    snippet.write_text("# portable\n")
    rc = tmp_path / ".zshrc"
    rc.write_text("export FOO=1")  # no trailing newline

    first = install_shell_source_line(snippet, rc)
    assert first.action == "appended"
    body = rc.read_text()
    assert "export FOO=1" in body and f'source "{snippet}"' in body

    second = install_shell_source_line(snippet, rc)
    assert second.action == "already"
    assert rc.read_text().count(f'source "{snippet}"') == 1  # no duplicate


def test_shell_source_line_missing_snippet(tmp_path: Path) -> None:
    from fno.setup.starship import install_shell_source_line

    res = install_shell_source_line(tmp_path / "nope.sh", tmp_path / ".zshrc")
    assert res.action == "missing-snippet"
    assert not (tmp_path / ".zshrc").exists()


def test_shell_source_line_non_utf8_rc_degrades(tmp_path: Path) -> None:
    """A shell rc with non-UTF-8 bytes degrades to missing-snippet, no crash."""
    from fno.setup.starship import install_shell_source_line

    snippet = tmp_path / "prompt-fno.sh"
    snippet.write_text("# portable\n")
    rc = tmp_path / ".zshrc"
    rc.write_bytes(b"\xff\xfe\x00\x01")  # invalid UTF-8

    res = install_shell_source_line(snippet, rc)
    assert res.action == "missing-snippet"


def test_wizard_capstone_offers_both_renderers(tmp_path: Path) -> None:
    """The capstone prompts for each renderer (default No): declined leaves both
    files absent and prints their paths; accepted installs both."""
    from fno.setup_cli import offer_prompt_provenance

    star = _snippet(tmp_path)
    sh = tmp_path / "prompt-fno.sh"
    sh.write_text("# portable\n")
    toml_tgt = tmp_path / "starship.toml"
    rc_tgt = tmp_path / ".zshrc"
    printed: list[str] = []

    declined = offer_prompt_provenance(
        confirm_fn=lambda _m: False, echo_fn=printed.append,
        snippet_path=star, target_toml=toml_tgt,
        shell_snippet_path=sh, shell_rc=rc_tgt,
    )
    assert declined == {"starship": None, "shell": None}
    assert not toml_tgt.exists() and not rc_tgt.exists()
    assert any(str(star) in line for line in printed)
    assert any(str(sh) in line for line in printed)

    accepted = offer_prompt_provenance(
        confirm_fn=lambda _m: True,
        snippet_path=star, target_toml=toml_tgt,
        shell_snippet_path=sh, shell_rc=rc_tgt,
    )
    assert accepted["starship"].action == "appended"
    assert accepted["shell"].action == "appended"
    assert _MODULE_MARKER in toml_tgt.read_text()
    assert f'source "{sh}"' in rc_tgt.read_text()
