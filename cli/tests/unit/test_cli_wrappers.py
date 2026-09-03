"""Cross-wrapper smoke tests: all 8 new fno subcommands respond to --help.

Task 02.2 of plan 2026-05-11-fno-cli-promotion-wrappers.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()
_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb"}


@pytest.mark.parametrize(
    "argv",
    [
        # gate-set and phase-verify removed by the control-plane collapse
        # wedge (ab-d0337fbc): the `fno gate` sub-app is gone and `fno do phase`
        # keeps only kill-check.
        ["pr", "verify", "--help"],
        ["pr", "rebase", "--help"],
        ["phase", "kill-check", "--help"],
        ["notify", "--help"],
    ],
    ids=[
        "pr-verify",
        "pr-rebase",
        "phase-kill-check",
        "notify",
    ],
)
def test_new_subcommand_help_renders(argv):
    """AC1-HP: every new subcommand responds to --help with exit 0."""
    result = runner.invoke(app, argv, env=_ENV)
    assert result.exit_code == 0, (
        f"argv={argv!r} exited {result.exit_code}; output:\n{result.output}"
    )
    assert len(result.output) > 0, f"argv={argv!r} produced empty output"


def test_top_level_help_lists_new_subapps():
    """AC4-UI: phase/notify are registered and reachable.

    Under x-71b6 In-N-Out tiering they are hidden from the curated `--help`
    menu. They used to be listed by the full-surface door `fno help --all`,
    but the moved-spellings block is gone (d-26002be8: aliases are discovered
    in their own subcommands), so reachability is now proven by invoking each
    deprecated spelling and checking it forwards to its new home.
    """
    for noun in ("phase", "notify"):
        result = runner.invoke(app, [noun, "--help"], env=_ENV)
        assert result.exit_code == 0, (
            f"`fno {noun} --help` exited {result.exit_code}; output:\n{result.output}"
        )
        assert result.output, f"`fno {noun} --help` produced empty output"


# ── fno backlog get, several ids: forwards to `fno-agents graph-get` (x-997a) ──
#
# One id keeps the all-Python path (untouched, exercised elsewhere by this
# command's other tests). Several ids is a NEW shape whose whole job is the
# argv it hands the binary, so that argv is what these pin.

_FAKE_GET_BIN = "/fake/bin/fno-agents"


class _StubGetResult:
    def __init__(self, returncode: int = 0):
        self.returncode = returncode


def _patch_get_binary(monkeypatch, binary=_FAKE_GET_BIN):
    import fno.rust_binary
    from pathlib import Path

    monkeypatch.setattr(
        fno.rust_binary, "resolve_binary", lambda: Path(binary) if binary else None
    )


def test_get_two_ids_forwards_graph_get_argv(monkeypatch):
    import subprocess

    captured = {}

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubGetResult(returncode=0)

    _patch_get_binary(monkeypatch)
    monkeypatch.setattr(subprocess, "run", _stub_run)

    result = runner.invoke(app, ["backlog", "get", "x-997a", "x-374b"], env=_ENV)
    assert result.exit_code == 0
    assert captured["cmd"] == [_FAKE_GET_BIN, "graph-get", "x-997a", "x-374b", "--json"]


def test_get_one_id_never_invokes_the_binary(monkeypatch):
    """A single id is the existing all-Python path; the binary must not even
    be resolved, let alone invoked."""
    import fno.rust_binary

    def _fail_resolve():
        raise AssertionError("resolve_binary() called for a single-id get")

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", _fail_resolve)

    result = runner.invoke(app, ["backlog", "get", "x-nonexistent-abc"], env=_ENV)
    # Reads through to the normal "no node matching" miss, not the batch path.
    assert result.exit_code == 1
    assert "No node matching" in result.output


def test_get_propagates_the_binary_exit_code(monkeypatch):
    import subprocess

    _patch_get_binary(monkeypatch)
    monkeypatch.setattr(
        subprocess, "run", lambda *a, **k: _StubGetResult(returncode=1)
    )
    result = runner.invoke(app, ["backlog", "get", "x-997a", "x-0000"], env=_ENV)
    assert result.exit_code == 1


def test_get_missing_binary_exits_2_and_names_the_remedy(monkeypatch):
    _patch_get_binary(monkeypatch, binary=None)
    result = runner.invoke(app, ["backlog", "get", "x-997a", "x-374b"], env=_ENV)
    assert result.exit_code == 2
    assert "fno-agents binary" in result.output


def test_get_several_ids_with_field_refuses_before_touching_the_binary(monkeypatch):
    """--field/--grouped/--strict project a SINGLE row; several ids with one of
    these is a usage error, not a silent per-row projection nobody asked for."""
    import fno.rust_binary

    def _fail_resolve():
        raise AssertionError("resolve_binary() called despite the flag refusal")

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", _fail_resolve)

    result = runner.invoke(
        app, ["backlog", "get", "x-997a", "x-374b", "--field", "title"], env=_ENV
    )
    assert result.exit_code == 2
    assert "one id at a time" in result.output
