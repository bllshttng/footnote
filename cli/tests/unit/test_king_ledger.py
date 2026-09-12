"""``fno agents king ledger``: the page renders the court's fold, never re-derives it.

Same doctrine as the court read itself: an empty court and an unreadable
registry must not render the same page, and an unresolved fold states its
reason instead of showing an empty table.
"""
from __future__ import annotations

from pathlib import Path

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
            "omitted": 1,
            "nodes": [
                {
                    "id": "x-1",
                    "status": "in_progress",
                    "worker": "w1",
                    "pr_number": 7,
                    "sessions": ["s1"],
                }
            ],
        },
    }
    base.update(kw)
    return base


def _court(crowns, **kw):
    court = {
        "crowns": crowns,
        "conflicts": [],
        "registry_readable": True,
        "graph_readable": True,
        "summary": {
            "total": len(crowns) if crowns is not None else None,
            "manifest_only": 0,
            "sweep_ran": True,
            "disagreements": 0,
            "unknowns": 0,
            "splits": 0,
        },
    }
    court.update(kw)
    return court


def test_one_section_per_crown_names_scope_and_holder():
    from fno.king.ledger import render_ledger_html

    page = render_ledger_html(
        _court([_crown(), _crown(scope="e-9", holder="regent")])
    )
    assert page.count("<section") == 2
    assert "e-1" in page and "king" in page
    assert "e-9" in page and "regent" in page


def test_unresolved_fold_states_its_reason_in_place():
    from fno.king.ledger import render_ledger_html

    page = render_ledger_html(
        _court(
            [
                _crown(
                    scope_nodes={
                        "status": "unresolved",
                        "reason": "the fold could not run: boom",
                    }
                )
            ]
        )
    )
    assert "scope fold: unresolved - the fold could not run: boom" in page
    assert "<table" not in page


def test_empty_court_renders_the_measurement():
    from fno.king.ledger import render_ledger_html

    page = render_ledger_html(_court([]))
    assert "no live crowns" in page


def test_registry_unreadable_names_the_reason_never_a_healthy_page():
    from fno.king.ledger import render_ledger_html

    page = render_ledger_html(
        _court(
            None,
            registry_readable=False,
            graph_readable=None,
            summary={
                "total": None,
                "manifest_only": None,
                "sweep_ran": None,
                "disagreements": None,
                "unknowns": None,
                "splits": None,
                "reason": "registry unreadable: disk on fire",
            },
        )
    )
    assert "registry unreadable: disk on fire" in page
    assert "no live crowns" not in page


def test_hostile_fields_are_escaped():
    from fno.king.ledger import render_ledger_html

    page = render_ledger_html(
        _court(
            [
                _crown(
                    holder="<script>x</script>",
                    scope_nodes={
                        "status": "ok",
                        "counts": {"in_progress": 1},
                        "total": 1,
                        "omitted": 0,
                        "nodes": [
                            {
                                "id": "x-1",
                                "status": "in_progress",
                                "worker": "<img src=x onerror=1>",
                                "pr_number": 7,
                                "sessions": [],
                            }
                        ],
                    },
                )
            ]
        )
    )
    assert "<script>" not in page
    assert "&lt;script&gt;" in page
    assert "<img" not in page


def test_counts_render_in_lifecycle_order_with_leftovers_last():
    from fno.king.ledger import counts_line

    fold = {
        "status": "ok",
        "counts": {"zebra": 1, "done": 2, "in_progress": 1},
        "total": 4,
        "omitted": 0,
        "nodes": [],
    }
    assert counts_line(fold) == "in_progress 1, done 2, zebra 1"


def test_write_defaults_to_state_root_and_leaves_no_temp(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno.king.ledger import default_ledger_path, write_ledger

    path = write_ledger(_court([]))
    assert path == default_ledger_path()
    assert path.exists()
    assert list(path.parent.glob("*.tmp")) == []


def test_write_honors_out(tmp_path):
    from fno.king.ledger import write_ledger

    out = tmp_path / "sub" / "ledger.html"
    assert write_ledger(_court([]), out) == out
    assert out.exists()


def test_build_gathers_folds_and_skips_the_fold_when_no_crowns(monkeypatch):
    import fno.agents.court as court_mod

    from fno.king.ledger import build_ledger_data

    calls = {}

    def fake_gather(rows=None):
        calls["rows"] = rows
        return _court([_crown()])

    def fake_fold(crowns):
        calls["folded"] = True
        crowns[0]["scope_nodes"] = {"status": "unresolved", "reason": "stub"}

    monkeypatch.setattr(court_mod, "gather_court", fake_gather)
    monkeypatch.setattr(court_mod, "fold_scope_nodes", fake_fold)

    court = build_ledger_data(rows=["r7"])
    assert calls == {"rows": ["r7"], "folded": True}
    assert court["crowns"][0]["scope_nodes"]["reason"] == "stub"

    calls.clear()
    monkeypatch.setattr(court_mod, "gather_court", lambda rows=None: _court([]))
    build_ledger_data()
    assert calls == {}
