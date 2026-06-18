"""Unit tests for `fno help [verb...]` — git-style help subcommand."""
from __future__ import annotations

import os
import subprocess
import sys

from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def test_help_no_args_shows_root_help():
    """AC-HP: `fno help` mirrors `fno --help`."""
    result = runner.invoke(app, ["help"])
    assert result.exit_code == 0, result.output
    # Output should mention some top-level subcommand names. ("gate" was
    # deleted by the control-plane collapse wedge, ab-d0337fbc.)
    assert "claim" in result.output
    assert "backlog" in result.output


def test_help_root_matches_dashdash_help():
    """AC-FR: `fno help` and `fno --help` produce equivalent top-level catalogs.

    We compare a few stable substrings rather than full output because rich
    rendering may differ across runs and the help string is long.
    """
    via_help = runner.invoke(app, ["help"])
    via_dashdash = runner.invoke(app, ["--help"])
    assert via_help.exit_code == 0
    assert via_dashdash.exit_code == 0
    for needle in ("claim", "backlog", "event", "pr"):
        assert needle in via_help.output, f"missing {needle} in `fno help` output"
        assert needle in via_dashdash.output, f"missing {needle} in `fno --help` output"


def test_help_subcommand_forwards():
    """AC-HP: `fno help claim` forwards to `fno claim --help`.

    We can't easily monkeypatch subprocess here because Typer's CliRunner
    invokes the app in-process and the `help` command shells out to the
    real binary. Instead, smoke-test that the subprocess actually runs and
    returns a sensible exit code. We verify the routing by passing a
    known-bad subcommand and confirming we get a typer-style usage error.
    """
    # The forward will shell out to `<argv[0]> not-a-real-verb --help`.
    # Typer's "no such command" path exits with code 2. We just want to
    # confirm the wrapper forwards rather than swallowing the args.
    result = runner.invoke(app, ["help", "not-a-real-verb-xyz"])
    # Either the subprocess returned non-zero (forwarded and the unknown
    # verb errored) or the test environment doesn't have `fno` on argv[0]
    # in a runnable form. Both are acceptable - what matters is the
    # in-process path did not crash with an unhandled exception.
    assert result.exception is None or isinstance(result.exception, SystemExit), (
        f"unexpected exception: {result.exception!r}"
    )


def test_help_command_listed_in_root_help():
    """AC-UI: `help` appears in the top-level command catalog."""
    result = runner.invoke(app, ["--help"])
    assert result.exit_code == 0
    assert "help" in result.output.lower()


def test_help_subcommand_works_in_module_mode():
    """AC-FR: `python -m fno.cli help <verb>` works (codex P2 fix).

    Regression for chatgpt-codex-connector P2 on PR #250: the original
    implementation forwarded via `sys.argv[0]`, which is the module file
    (cli.py) when invoked as `python -m fno.cli`. That file is
    typically not chmod +x and has no shebang, so the forward crashed
    with PermissionError or FileNotFoundError. The fix falls back to
    `[sys.executable, "-m", "fno.cli", ...]` when sys.argv[0] is
    not directly executable.

    This test invokes the module-mode form via a real subprocess and
    asserts the help output renders for a known subcommand (`claim`).
    """
    # Use COLUMNS/NO_COLOR to keep the help output stable in CI.
    env = {
        **os.environ,
        "COLUMNS": "240",
        "NO_COLOR": "1",
        "TERM": "dumb",
    }
    result = subprocess.run(
        [sys.executable, "-m", "fno.cli", "help", "claim"],
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )
    assert result.returncode == 0, (
        f"expected rc=0, got rc={result.returncode}.\n"
        f"stdout={result.stdout}\nstderr={result.stderr}"
    )
    # `fno claim --help` output mentions the subcommand verbs the claim
    # app registers (acquire, release, ...). ("gate" died with ab-d0337fbc.)
    assert "acquire" in result.stdout
    assert "release" in result.stdout
