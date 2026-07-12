"""Unit tests for the running-mux-server staleness detector (x-e6dd, x-1a85).

`stale_mux_servers()` flags LIVE mux sessions on a stale WIRE VERSION - the
running server predates the installed binary's PROTO_VERSION, so a new client's
handshake is rejected. The precise signal is the ``stale`` field
``fno mux ls --json`` computes from each server's ``.ver`` sidecar (x-1a85),
which replaced the older ``socket mtime < binary mtime`` heuristic (a
wire-agnostic false alarm that fired after every reinstall). The mux ls call is
injected via ``runner`` so the JSON contract is exercised without a real server.
"""
from __future__ import annotations

import json
from types import SimpleNamespace

from fno import update


def test_stale_flags_only_live_stale_wire_servers(monkeypatch, tmp_path):
    binary = tmp_path / "fno"
    binary.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)

    def fake_run(cmd, **kw):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(
                [
                    {"session": "old", "state": "live", "stale": True},
                    {"session": "cur", "state": "live", "stale": False},
                    {"session": "pre", "state": "live", "stale": True, "wire_version": None},
                    {"session": "dead", "state": "stale"},  # not live -> never flagged
                ]
            ),
        )

    # Only LIVE + stale rows; a current-wire live server and a dead socket are excluded.
    assert update.stale_mux_servers(runner=fake_run) == ["old", "pre"]


def test_stale_empty_when_no_mux_binary(monkeypatch):
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: None)
    monkeypatch.setattr(update.shutil, "which", lambda _x: None)
    assert update.stale_mux_servers() == []


def test_stale_empty_on_error_or_bad_json(monkeypatch, tmp_path):
    binary = tmp_path / "fno"
    binary.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)

    assert (
        update.stale_mux_servers(runner=lambda cmd, **kw: SimpleNamespace(returncode=1, stdout=""))
        == []
    )
    assert (
        update.stale_mux_servers(
            runner=lambda cmd, **kw: SimpleNamespace(returncode=0, stdout="not json")
        )
        == []
    )


def test_live_mux_sessions_parses_live_only(monkeypatch, tmp_path):
    binary = tmp_path / "fno"
    binary.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)

    def fake_run(cmd, **kw):
        return SimpleNamespace(
            returncode=0,
            stdout='[{"session":"a","state":"live"},'
            '{"session":"b","state":"stale"},'
            '{"session":"c","state":"live"}]',
        )

    assert update._live_mux_sessions(runner=fake_run) == ["a", "c"]


def test_live_mux_sessions_empty_on_error_or_bad_json(monkeypatch, tmp_path):
    binary = tmp_path / "fno"
    binary.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)

    assert (
        update._live_mux_sessions(
            runner=lambda cmd, **kw: SimpleNamespace(returncode=1, stdout="")
        )
        == []
    )
    assert (
        update._live_mux_sessions(
            runner=lambda cmd, **kw: SimpleNamespace(returncode=0, stdout="not json")
        )
        == []
    )
