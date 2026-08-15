"""Attention order: the Python list lane matches the shared fixture.

The fixture in schemas/agents-attention-order.json is the one contract three
independent sorters assert against (the mux ranker in crates/fno, the daemon
projection in crates/fno-agents, and this serializer); none of them can share
code with the others, so the file is what keeps the three honest.
"""
import json
import pathlib

from fno.agents.format import attention_sort_key
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


def test_humanize_age_renders_absent_as_question_mark_not_zero():
    assert _humanize_age(None) == "   ?"
