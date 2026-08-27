"""Behavioral tests that EXECUTE the board's JavaScript.

Every other dashboard test asserts on the emitted source as a string. Review of
PR 1208 found four JS defects that shape of test cannot see, because all four
are behavior: group headers counting the unfiltered set, an empty project
selection blanking the board, positional bar colours sliding, and a stale
persisted selection hiding every row. These run the real code and read the
real result.

Node is a declared CI dependency (``.github/workflows/cli-ci.yml`` sets up
Node 22), so a missing interpreter is a FAILURE here, never a skip. A skipped
guard is an absence, and an absence proves nothing.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

from fno.graph.render_html import render_graph_html

HARNESS = Path(__file__).parent / "dashboard_harness.mjs"


def _entry(eid: str, **kw) -> dict:
    base = {
        "id": eid,
        "title": kw.pop("title", eid),
        "status": kw.pop("status", "ready"),
        "priority": kw.pop("priority", "p2"),
        "type": kw.pop("type", "feature"),
        "project": kw.pop("project", "fno"),
        "created_at": "2026-01-01T00:00:00Z",
    }
    base.update(kw)
    return base


def _run(tmp_path: Path, entries: list[dict], **env) -> dict:
    node = shutil.which("node")
    assert node, (
        "node is missing. It is a declared CI dependency (cli-ci.yml sets up "
        "Node 22); this guard fails rather than skipping, because a skipped "
        "guard proves nothing."
    )
    board = tmp_path / "board.html"
    render_graph_html(entries, board)
    result = subprocess.run(
        [node, str(HARNESS), str(board)],
        capture_output=True,
        text=True,
        env={**os.environ, **{k: str(v) for k, v in env.items()}},
        timeout=60,
    )
    assert result.returncode == 0, f"harness failed:\n{result.stderr}"
    return json.loads(result.stdout)


def test_the_harness_actually_runs_the_board(tmp_path: Path):
    """Positive control for every test below.

    If the harness silently produced an empty board, each assertion about
    hidden rows would pass for the wrong reason. Prove it renders first.
    """
    out = _run(tmp_path, [_entry("ab-00000001"), _entry("ab-00000002")])
    assert out["totalRows"] == 2
    assert out["visibleRows"] == 2
    assert out["shown"] == "2 of 2 nodes shown"
    assert out["groups"] and out["groups"][0]["open"] == "true"


def test_a_filtered_group_hides_and_its_count_follows_the_filter(tmp_path: Path):
    """Headers counted NODES, not the filtered set.

    Thirteen headers each claimed the whole graph while three rows showed, and
    an all-filtered group still painted as a visible empty section.
    """
    entries = [
        _entry("ab-0pen0001", title="Open one"),
        _entry("ab-d0ne0001", title="Closed one", status="done"),
    ]
    out = _run(tmp_path, entries)

    # done is unpressed on first paint, so only the open row shows.
    assert out["visibleRows"] == 1
    assert out["visibleIds"] == ["ab-0pen0001"]
    assert out["shown"] == "1 of 2 nodes shown"
    # The count describes what is visible, not the group's whole membership.
    assert [str(g["count"]) for g in out["groups"]] == ["1"]
    # Both rows stay in the DOM. Hiding by deletion is the failure this
    # whole design exists to prevent.
    assert out["totalRows"] == 2


def test_deselecting_the_last_project_chip_does_not_blank_the_board(tmp_path: Path):
    """An empty selection reads as no filter, matching the status chips."""
    out = _run(
        tmp_path,
        [_entry("ab-a0000001", project="alpha"), _entry("ab-b0000001", project="beta")],
        BOARD_ACTION="toggleProject:alpha",
        BOARD_ACTION_TIMES=2,
    )
    after = out["after"]
    assert after["visibleRows"] == 2, "toggling the last chip off blanked the board"
    assert all(c["pressed"] == "false" for c in after["projectChips"])
    # And the blank must not survive a reload. Two guards hold this: the
    # match predicate treats an empty set as no filter, and the write-back
    # records active=false. Assert the PERSISTED half explicitly, or a
    # control that breaks only the write-back passes on the other guard.
    assert json.loads(after["persisted"]) == {"selected": [], "active": False}


def test_a_stale_persisted_selection_does_not_blank_the_board(tmp_path: Path):
    """localStorage is origin-wide, so one key is shared by every board.

    A selection saved on the global board names projects a scoped board has
    never heard of. Restored as-is, those match nothing and every row hides.
    """
    out = _run(
        tmp_path,
        [_entry("ab-c0000001", project="fno")],
        BOARD_SEED_PROJECTS=json.dumps({"selected": ["ghost"], "active": True}),
    )
    assert out["visibleRows"] == 1, "a stale selection hid every row"
    assert [c["project"] for c in out["projectChips"]] == ["fno"]


def test_a_persisted_selection_that_still_matches_is_honored(tmp_path: Path):
    """The paired half: intersecting must not throw away a VALID selection."""
    out = _run(
        tmp_path,
        [_entry("ab-d0000001", project="alpha"), _entry("ab-e0000001", project="beta")],
        BOARD_SEED_PROJECTS=json.dumps({"selected": ["alpha"], "active": True}),
    )
    assert out["visibleIds"] == ["ab-d0000001"]
    pressed = {c["project"]: c["pressed"] for c in out["projectChips"]}
    assert pressed == {"alpha": "true", "beta": "false"}


def test_bar_segments_carry_their_own_status_colour(tmp_path: Path):
    """Colours were positional nth-child, and a zero-count status emits no
    segment, so every survivor slid into the wrong slot."""
    out = _run(
        tmp_path,
        [_entry("ab-f0000001", status="blocked"), _entry("ab-f0000002", status="ready")],
    )
    bar = out["groups"][0]["bar"]
    assert "background:var(--blocked)" in bar
    assert "background:var(--ready)" in bar
    # One segment per PRESENT status, never a placeholder for an absent one.
    assert bar.count("<i ") == 2


def test_only_non_feature_types_get_a_badge(tmp_path: Path):
    """feature is 87 percent of the graph; a badge on it says nothing.

    The pair is the test. Asserting only that a bug is badged would pass on a
    renderer that badges everything.
    """
    out = _run(
        tmp_path,
        [
            _entry("ab-11110001", type="feature"),
            _entry("ab-11110002", type="bug"),
            _entry("ab-11110003", type="epic"),
            _entry("ab-11110004", type="chore"),
        ],
    )
    html = {r["id"]: r["html"] for r in out["rowHtml"]}
    assert "t-bug" in html["ab-11110002"]
    assert "t-epic" in html["ab-11110003"]
    assert "t-other" in html["ab-11110004"]
    assert "pill t-" not in html["ab-11110001"], "feature must carry no type badge"
    # Positive control: the feature row DID render, so the absence above is
    # about the badge and not about a missing row.
    assert 'class="rt"' in html["ab-11110001"]


def test_a_parent_row_rolls_up_its_children_and_links_to_them(tmp_path: Path):
    """The epic view: a count, a rollup bar, and a link that names a real row."""
    entries = [
        _entry("ab-ep000001", title="The Epic", type="epic"),
        _entry("ab-ki000001", title="Open kid", parent="ab-ep000001"),
        _entry("ab-ki000002", title="Done kid", parent="ab-ep000001", status="done"),
    ]
    out = _run(tmp_path, entries, BOARD_ACTION="expand:ab-ep000001")

    epic = next(r for r in out["rowHtml"] if r["id"] == "ab-ep000001")
    assert "1/2" in epic["html"], "parent row must state open/total children"
    assert 'class="kids"' in epic["html"]

    detail = out["detail"]
    assert "Children" in detail
    assert 'href="#ab-ki000001"' in detail, "a child must be an in-page anchor"
    assert 'href="#ab-ki000002"' in detail
    # Every anchor target must be a row that exists in this same document,
    # or the link lands nowhere.
    ids = {r["id"] for r in out["rowHtml"]}
    assert {"ab-ki000001", "ab-ki000002"} <= ids


def test_a_child_names_its_parent_even_when_the_parent_is_out_of_scope(tmp_path: Path):
    """Ancestry survives scoping, through the same context_entries seam that
    keeps a cross-project blocker visible."""
    entries = [
        _entry("ab-ep000002", title="Foreign epic", type="epic", project="other"),
        _entry("ab-ki000003", title="Scoped kid", parent="ab-ep000002", project="fno"),
    ]
    board = tmp_path / "scoped.html"
    render_graph_html(entries, board, project="fno")
    text = board.read_text()
    assert "Scoped kid" in text
    assert "ab-ep000002" in text, "the out-of-scope parent must still be named"
    assert "Foreign epic" in text


def test_an_anchor_only_links_an_id_that_has_a_row(tmp_path: Path):
    """Relations come from the whole graph; the board renders a subset.

    A blocker that closed long ago, or a child in another project, has no row.
    Linking it produced an anchor that set the hash and revealed nothing, which
    reads as a broken page rather than as work that is simply not on this board.
    """
    entries = [
        _entry("ab-11000001", title="On the board", blocked_by=["ab-11000002"]),
        _entry("ab-11000003", title="Child here", parent="ab-11000001"),
    ]
    out = _run(tmp_path, entries, BOARD_ACTION="expand:ab-11000001")
    detail = out["detail"]

    assert 'href="#ab-11000003"' in detail, "a rendered child MUST be a link"
    assert 'href="#ab-11000002"' not in detail, (
        "an id with no row must not be a link: the anchor would reveal nothing"
    )
    assert "ab-11000002" in detail, "it must still be NAMED, just not linked"


def test_an_anchor_reveals_a_row_the_active_filter_hides(tmp_path: Path):
    """A link can name a row the current filter hides. An anchor to a hidden
    element scrolls nowhere, so the jump has to reveal its target."""
    entries = [
        _entry("ab-12000001", project="web", title="alpha target"),
        _entry("ab-12000002", project="fno", title="zulu other"),
    ]
    out = _run(
        tmp_path,
        entries,
        BOARD_QUERY="?project=fno",
        BOARD_HASH="#ab-12000001",
    )
    assert out["revealedVisible"] is True, (
        "the anchored row stayed hidden, so the jump lands on nothing"
    )


def test_the_reveal_exemption_is_dropped_on_the_next_reader_action(tmp_path: Path):
    """The exemption lasts exactly one render: its own.

    Holding it forever is the opposite defect. The row then counts toward every
    later group header and total, which is the same lie the visible-count
    invariant exists to prevent, and no action short of a reload clears it.
    """
    entries = [
        _entry("ab-12000001", title="alpha target"),
        _entry("ab-12000002", title="zulu other"),
    ]
    out = _run(
        tmp_path,
        entries,
        BOARD_HASH="#ab-12000001",
        BOARD_ACTION="search:zulu",
    )
    assert out["after"]["visibleRows"] == 1, (
        f"a pinned row inflated the count: {out['after']['visibleRows']} visible "
        "when only one row matches the query"
    )
    assert out["after"]["revealedVisible"] is False
    assert out["after"]["shown"].startswith("1 "), out["after"]["shown"]


def test_the_stat_tiles_carry_their_status(tmp_path: Path):
    """The .stat.is-* emphasis CSS was added and never wired: className was
    assigned unconditionally, so Ready and Blocked rendered identically to
    Total and the third slot of every statRows tuple sat unused."""
    out = _run(tmp_path, [_entry("ab-13000001")])
    classes = out["statClasses"]
    assert "stat is-ready" in classes, f"no Ready emphasis: {classes}"
    assert "stat is-blocked" in classes, f"no Blocked emphasis: {classes}"
    assert "stat" in classes, "Total must stay unemphasised"


def test_a_status_outside_the_vocabulary_still_renders_a_chip(tmp_path: Path):
    """`.pill` carried only typography, so a priority, a size, a PR number and
    any status with no `.s-*` rule rendered as loose uppercase text."""
    from fno.graph.render_html import _DASHBOARD_CSS

    base = [
        line for line in _DASHBOARD_CSS.splitlines() if line.startswith(".pill {")
    ]
    assert base, "the .pill base rule vanished"
    block = _DASHBOARD_CSS.split(".pill {", 1)[1].split("}", 1)[0]
    assert "background:" in block, (
        "the neutral chip base is gone; every unmodified pill renders as bare text"
    )


def test_a_graph_value_named_after_an_object_member_is_inert(tmp_path: Path):
    """TYPE_CLASSES and TERMINAL are keyed on values the GRAPH supplies.

    On a plain object, a type named `constructor` inherits Object.prototype's
    member and puts a function in a class attribute; a child status named
    `toString` counts as terminal and silently shifts a parent's rollup.
    """
    entries = [
        _entry("ab-15000001", title="Hostile type", type="constructor"),
        _entry("ab-15000002", title="Parent", type="epic"),
        _entry("ab-15000003", title="Odd child", status="toString",
               parent="ab-15000002"),
    ]
    out = _run(tmp_path, entries, BOARD_ACTION="expand:ab-15000002")
    html = "".join(r["html"] for r in out["rowHtml"])

    assert "function" not in html.lower(), (
        "an Object.prototype member reached a class attribute"
    )
    assert "native code" not in html.lower()
    # The child is not terminal, so the parent reads 1 of 1 still open.
    assert "1/1" in out["detail"] or "1/1" in html, out["detail"][:400]
