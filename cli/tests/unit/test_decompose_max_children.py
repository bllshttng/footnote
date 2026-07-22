"""Tests for the `max_children` epic-frontmatter cap on `fno backlog decompose`.

An optional `max_children: N` in an epic plan-doc's frontmatter is the author's
durable, per-epic child-count ceiling. It OVERRIDES `config.blueprint.max_prs_per_epic`
upward; an explicit `--max-prs` may only tighten it. A present-but-invalid value
(non-int, YAML bool, < 1) refuses decompose loud (fail-closed on value); an
unreadable doc falls back to current behavior (fail-safe on IO). Unset = byte-
identical to before this feature.

Covers US1-US5 / AC1-HP, AC2-HP, AC1-ERR, AC2-ERR, AC1-UI, AC1-EDGE, AC2-EDGE,
AC3-EDGE, AC1-FR from the plan.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "parent": None,
        "title": "default-title",
        "type": "feature",
        "project": "fno",
        "cwd": "/tmp/abilities",
        "priority": "p2",
        "domain": "code",
        "blocked_by": [],
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "has_brief": False,
        "compacted": False,
        "roadmap_id": None,
        "vision_path": None,
        "details": None,
        "size": None,
        "batch": None,
        "cost_usd": None,
        "cost_sessions": [],
        "plan_path": None,
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "artifact_url": None,
        "completion_note": None,
        "status": "idea",
        "created_at": "2026-01-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


@pytest.fixture
def graph_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Temp graph.json + epic doc; returns (write_frontmatter, read_entries).

    `write_frontmatter(**fields)` (re)writes the epic doc's YAML frontmatter so a
    test can declare `max_children` (or omit it). The epic's plan_path carries a
    `#anchor` fragment, exercising the plan_base strip on the read path.
    """
    import fno.graph._constants as gc
    import fno.graph.store as gs

    doc = tmp_path / "big.md"

    def write_frontmatter(**fields) -> None:
        lines = ["---", "title: Big epic", "status: draft"]
        for k, v in fields.items():
            lines.append(f"{k}: {v}")
        lines += ["---", "# body", ""]
        doc.write_text("\n".join(lines))

    write_frontmatter()  # default: no max_children

    g = tmp_path / "graph.json"
    epic = _node(
        "ab-epic0001",
        title="Epic: big thing",
        plan_path=f"{doc}#c1-anchor",
        priority="p1",
        project="fno",
        cwd=str(tmp_path),
        status="ready",
    )
    g.write_text(json.dumps({"entries": [epic]}) + "\n")

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    def read_entries():
        return json.loads(g.read_text())["entries"]

    return write_frontmatter, read_entries, doc


def _groups(n: int) -> str:
    return json.dumps(
        [
            {"slug": str(i), "title": f"Group {i}", "waves": str(i), "blocked_by_groups": []}
            for i in range(1, n + 1)
        ]
    )


def _invoke(args):
    from fno.cli import app

    return CliRunner().invoke(app, args)


def _decompose(n_groups: int, *extra):
    return _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups(n_groups), *extra]
    )


# -- AC1-HP: cap honored when set --


def test_ac1_hp_cap_honored_when_set(graph_env):
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=3)
    result = _decompose(3)
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 3


# -- AC2-HP: max_children overrides config default upward --


def test_ac2_hp_overrides_config_default_upward(graph_env):
    # Config default is 4; six groups would be refused WITHOUT max_children.
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=6)
    result = _decompose(6)  # no --max-prs
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 6


def test_default_still_clamps_without_max_children(graph_env):
    # Guards AC2-HP's premise: six groups DO overflow the config default of 4
    # when no max_children is declared.
    write_fm, read_entries, _ = graph_env
    before = read_entries()
    result = _decompose(6)
    assert result.exit_code != 0
    assert read_entries() == before


# -- AC1-ERR: overflow refuses, names the cap + source + overflow slugs, atomic --


def test_ac1_err_overflow_refuses_and_names_cap(graph_env):
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=3)
    before = read_entries()
    result = _decompose(4)
    assert result.exit_code != 0
    out = result.output
    assert "max_children=3" in out          # cap + source
    assert "big.md" in out                   # epic doc path
    assert "4" in out                        # overflow slug (the 4th group)
    assert read_entries() == before          # graph unchanged (pre-lock refusal)


# -- AC2-ERR: malformed cap value refuses loud (including YAML bool) --


@pytest.mark.parametrize(
    "bad",
    ['"two"', "0", "-1", "6.5", "true", "false", "[1, 2]"],
)
def test_ac2_err_malformed_cap_refuses(graph_env, bad):
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=bad)
    before = read_entries()
    result = _decompose(2)  # well under any sane cap
    assert result.exit_code != 0, f"{bad!r} should refuse"
    assert "max_children" in result.output
    assert read_entries() == before


def test_ac2_err_yaml_true_not_coerced_to_one(graph_env):
    # The bool-is-an-int-subclass trap: `true` must NOT pass as cap 1 and let a
    # single group through - it must refuse as an invalid value.
    write_fm, read_entries, _ = graph_env
    write_fm(max_children="true")
    before = read_entries()
    result = _decompose(1)  # would pass if true coerced to cap 1
    assert result.exit_code != 0
    assert "max_children" in result.output
    assert read_entries() == before


# -- AC1-EDGE: unset cap is byte-identical to today --


def test_ac1_edge_unset_uses_explicit_flag(graph_env):
    write_fm, read_entries, _ = graph_env
    # No max_children; explicit --max-prs 2 with 3 groups refuses (as before).
    before = read_entries()
    result = _decompose(3, "--max-prs", "2")
    assert result.exit_code != 0
    assert read_entries() == before


def test_ac1_edge_unset_two_under_default_passes(graph_env):
    write_fm, read_entries, _ = graph_env
    result = _decompose(2)  # 2 <= config default 4
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 2


# -- AC2-EDGE: cap equal to group count passes (inclusive ceiling) --


def test_ac2_edge_cap_equal_to_group_count_passes(graph_env):
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=3)
    result = _decompose(3)
    assert result.exit_code == 0, result.output


# -- AC3-EDGE: explicit --max-prs may tighten but not loosen --


def test_ac3_edge_flag_tightens(graph_env):
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=6)
    before = read_entries()
    result = _decompose(4, "--max-prs", "3")  # tighter cap 3, 4 groups -> refuse
    assert result.exit_code != 0
    assert "3" in result.output
    assert read_entries() == before


def test_ac3_edge_flag_cannot_loosen(graph_env):
    write_fm, read_entries, _ = graph_env
    write_fm(max_children=6)
    before = read_entries()
    result = _decompose(8, "--max-prs", "20")  # flag 20 cannot loosen cap 6
    assert result.exit_code != 0
    assert "max_children=6" in result.output
    assert read_entries() == before


# -- resolve_effective_cap: the one new piece of logic, unit-pinned --


def test_resolve_cap_precedence():
    from fno.graph._decompose import DecomposeError, resolve_effective_cap

    # Unset: byte-identical fallback (explicit else config default).
    assert resolve_effective_cap(None, None, 4)[0] == 4
    assert resolve_effective_cap(None, 2, 4)[0] == 2
    # Set: overrides config default upward.
    assert resolve_effective_cap(6, None, 4)[0] == 6
    # Set + tightening flag: min wins, source names the flag.
    cap, src = resolve_effective_cap(6, 3, 4)
    assert cap == 3 and "max-prs" in src
    # Set + loosening flag: cap holds, source names max_children.
    cap, src = resolve_effective_cap(6, 20, 4)
    assert cap == 6 and "max_children=6" in src
    # Fail-closed on invalid value, bool rejected first (not coerced to 1).
    for bad in (True, False, 0, -1, "6", 6.5, [1]):
        with pytest.raises(DecomposeError):
            resolve_effective_cap(bad, None, 4)


# -- AC1-FR: unreadable epic doc degrades to current behavior --


def test_ac1_fr_unreadable_doc_degrades(graph_env, tmp_path):
    write_fm, read_entries, doc = graph_env
    doc.unlink()  # plan_path now points at a missing file
    result = _decompose(2)  # 2 <= config default -> proceeds under fallback ceiling
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 2
