"""``fno agents king ledger``: Python resolves the court and the paths; the
native reign-ledger verb owns the page assembly (the king-history split).

The renderer's own truth lives in the Rust tests; these pin the Python-side
plumbing: the court gather + fold, the binary relay's argv, and the refusal
when the binary is missing.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _crown(**kw):
    base = {
        "holder": "king",
        "level": 2,
        "scope": "e-1",
        "grantor": "human",
        "status": "busy",
        "agree": True,
        "reason": None,
        "crown_source": "row",
        "scope_nodes": {
            "status": "ok",
            "counts": {"in_progress": 1, "done": 2},
            "total": 3,
            "omitted": 0,
            "nodes": [],
        },
    }
    base.update(kw)
    return base


def test_build_gathers_folds_and_skips_the_fold_when_no_crowns(monkeypatch):
    import fno.agents.court as court_mod

    from fno.king.ledger import build_ledger_data

    calls = {}

    def fake_gather(rows=None):
        calls["rows"] = rows
        return {"crowns": [_crown()], "summary": {}}

    def fake_fold(crowns):
        calls["folded"] = True
        crowns[0]["scope_nodes"]["status"] = "unresolved"

    monkeypatch.setattr(court_mod, "gather_court", fake_gather)
    monkeypatch.setattr(court_mod, "fold_scope_nodes", fake_fold)

    court = build_ledger_data(rows=["r7"])
    assert calls == {"rows": ["r7"], "folded": True}
    assert court["crowns"][0]["scope_nodes"]["status"] == "unresolved"

    calls.clear()
    monkeypatch.setattr(court_mod, "gather_court", lambda rows=None: {"crowns": []})
    build_ledger_data()
    assert calls == {}


def test_default_ledger_path_is_the_state_dir_sibling(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno.king.ledger import default_ledger_path

    assert default_ledger_path() == tmp_path / ".fno" / "reign.html"


def test_relay_hands_the_native_renderer_court_graph_and_out(
    tmp_path, monkeypatch
):
    from fno.king import ledger as ledger_module
    from fno.king.ledger import write_ledger

    court = {"crowns": [_crown()], "summary": {"total": 1}}
    seen = {}

    class Proc:
        returncode = 0
        stdout = ""
        stderr = ""

    def fake_run(argv, **_kwargs):
        seen["argv"] = argv
        seen["court_on_disk"] = json.loads(
            Path(argv[argv.index("--court-json") + 1]).read_text(encoding="utf-8")
        )
        Path(argv[argv.index("--out") + 1]).write_text("<html></html>", encoding="utf-8")
        return Proc()

    stub = tmp_path / "stub-fno-agents"
    stub.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    stub.chmod(0o755)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: str(stub)
    )
    monkeypatch.setattr(ledger_module.subprocess, "run", fake_run)

    out = tmp_path / "page.html"
    assert write_ledger(court, out) == out
    argv = seen["argv"]
    assert argv[1] == "reign-ledger"
    assert seen["court_on_disk"] == court
    assert "--graph" in argv
    assert out.exists()


def test_relay_refuses_when_the_binary_is_missing(monkeypatch, tmp_path):
    from fno.king.ledger import write_ledger

    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)

    with pytest.raises(RuntimeError, match="binary"):
        write_ledger({"crowns": []}, tmp_path / "x.html")


def test_relay_surfaces_a_native_failure(tmp_path, monkeypatch):
    from fno.king.ledger import write_ledger

    class Proc:
        returncode = 1
        stdout = ""
        stderr = "graph unreadable: no such file"

    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: "/bin/true"
    )
    monkeypatch.setattr(
        "fno.king.ledger.subprocess",
        type("M", (), {"run": staticmethod(lambda argv, **k: Proc())}),
    )

    with pytest.raises(RuntimeError, match="graph unreadable"):
        write_ledger({"crowns": []}, tmp_path / "x.html")