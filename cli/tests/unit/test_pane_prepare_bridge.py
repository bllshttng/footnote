"""The Rust-to-Python renderer bridge, asserted as a contract.

`fno mux pane send` (Rust) shells to `fno mail pane-prepare` (Python) for every
non-raw send, and FAILS CLOSED when that call cannot run. Fail-closed is right,
and it is also why this needs a test: rename the command or the flag and every
existing test still passes while non-raw pane sends refuse at runtime, quietly,
because a refusal is what the design asks for when the hop is unavailable.

So this reads the argv the Rust source actually builds and asserts the Python
CLI accepts it. It is a positive marker on the exact tokens that cross the
boundary, not a check that something somewhere still works.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.mail.cli import mail_app

REPO = Path(__file__).resolve().parents[3]
MUX_CLI = REPO / "crates" / "fno" / "src" / "mux_cli.rs"


def _bridge_argv() -> list[str]:
    """The literal argv `prepare_pane_bytes` passes to the child."""
    src = MUX_CLI.read_text(encoding="utf-8")
    block = re.search(
        r"fn prepare_pane_bytes.*?\.args\(\[(.*?)\]\)", src, re.S
    )
    assert block, "prepare_pane_bytes no longer builds a literal .args([...])"
    return re.findall(r'"([^"]+)"', block.group(1))


def _declared_options() -> set[str]:
    """Every option string `mail pane-prepare` declares."""
    from typer.main import get_command

    group = get_command(mail_app)
    prepare_cmd = group.commands["pane-prepare"]  # type: ignore[attr-defined]
    opts: set[str] = set()
    for param in prepare_cmd.params:
        opts.update(getattr(param, "opts", []) or [])
        opts.update(getattr(param, "secondary_opts", []) or [])
    assert opts, "pane-prepare declares no options; the introspection broke"
    return opts


def test_the_rust_bridge_names_a_command_this_cli_has():
    argv = _bridge_argv()
    # The shape it has always had: `mail pane-prepare` plus flags.
    assert argv[:2] == ["mail", "pane-prepare"], argv

    result = CliRunner().invoke(mail_app, ["pane-prepare", "--help"])
    assert result.exit_code == 0, (
        f"the Rust bridge shells to `mail pane-prepare`, which this CLI no "
        f"longer accepts: {result.output}"
    )


@pytest.mark.parametrize("flag", ["--session-id", "--pane"])
def test_the_rust_bridge_flags_still_exist(flag):
    """Each flag the bridge passes must be one the command still takes.

    `--session-id` in particular was renamed once already, with `--session`
    kept as a hidden deprecated alias. A second rename that forgot the Rust
    caller would refuse every non-raw send and no test would say so.
    """
    assert flag in _bridge_argv(), f"{flag} is no longer in the bridge argv"
    # Introspect the parameters, never the rendered help. Typer draws help in a
    # Rich box that wraps to the terminal width, so a narrower CI terminal
    # splits a long flag across lines and a substring search reports it missing.
    # The parameter list is what the command actually accepts, and it does not
    # depend on how wide the screen is.
    assert flag in _declared_options(), (
        f"`mail pane-prepare` no longer accepts {flag}; the Rust bridge passes it"
    )
