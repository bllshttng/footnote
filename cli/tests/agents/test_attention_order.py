"""Attention order: the Python list lane matches the shared fixture.

The fixture in schemas/agents-attention-order.json is the one contract three
independent sorters assert against (the mux ranker in crates/fno, the daemon
projection in crates/fno-agents, and this serializer); none of them can share
code with the others, so the file is what keeps the three honest.
"""
import json
import pathlib

from fno.agents.format import attention_sort_key, render_json, render_table
from fno.agents.session_truth import _humanize_age

FIXTURE = (
    pathlib.Path(__file__).resolve().parents[3] / "schemas" / "agents-attention-order.json"
)


def _fixture() -> tuple[list[dict], list[str]]:
    data = json.loads(FIXTURE.read_text())
    return data["rows"], data["expected_order"]


def test_python_serializer_sorts_rows_in_the_shared_attention_order():
    rows, expected = _fixture()
    ordered = sorted(rows, key=attention_sort_key)
    assert [r["name"] for r in ordered] == expected


def test_humanize_age_is_fixed_width_and_covers_days():
    for secs in (12, 2700, 10800, 345600):
        assert len(_humanize_age(secs)) == 4, secs
    assert _humanize_age(12) == " 12s"
    assert _humanize_age(2700) == " 45m"
    assert _humanize_age(10800) == "  3h"
    assert _humanize_age(345600) == "  4d"


def test_humanize_age_caps_the_day_count_at_three_digits():
    # 1000+ days would otherwise render "1000d" (5 chars), breaking the
    # fixed-width-4 invariant the column exists to hold.
    assert _humanize_age(1000 * 86400) == "999d"
    assert len(_humanize_age(1000 * 86400)) == 4


def test_humanize_age_renders_absent_as_question_mark_not_zero():
    assert _humanize_age(None) == "   ?"


def test_render_json_emits_rows_in_the_shared_attention_order():
    rows, expected = _fixture()
    payload = json.loads(render_json(rows, filters_applied={}))
    assert [r["name"] for r in payload["agents"]] == expected


def test_render_table_emits_rows_in_the_shared_attention_order():
    # render_table sorts internally too (not just render_json) - a plain TTY
    # `fno agents list` must not fall back to registry order.
    rows, expected = _fixture()
    output = render_table(rows, terminal_width=200)
    positions = [output.index(name) for name in expected]
    assert positions == sorted(positions)
