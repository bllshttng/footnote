"""Per-task ``blocked_by`` edges in the Execution Strategy parser.

Task identity is normalized in one place (``fno.plan.brief``), so the edge
key must survive that normalization for the orchestrator to see it, and
``validate_task_edges`` is the shared unknown-id + cycle check both the
parser's callers and validate-plan.sh consume.
"""
from __future__ import annotations

import pytest

from fno.plan.brief import (
    BriefParseError,
    list_task_ids,
    parse_execution_strategy,
    validate_task_edges,
)


def _strategy_yaml(tasks: str) -> str:
    return f"```yaml\ntasks:\n{tasks}\nwaves:\n  - wave: 1\n    tasks: ['1.1']\n```"


class TestParseBlockedBy:
    def test_declared_edge_survives_normalization(self):
        yaml_text = _strategy_yaml(
            "  - id: '1.1'\n"
            "    title: Setup\n"
            "  - id: '2.1'\n"
            "    title: Follow-up\n"
            "    blocked_by: ['1.1']\n"
        )
        parsed = parse_execution_strategy(yaml_text)
        by_id = {t["id"]: t for t in parsed["tasks"]}
        assert by_id["2.1"]["blocked_by"] == ["1.1"]
        assert by_id["1.1"]["blocked_by"] == []

    def test_missing_key_defaults_to_empty(self):
        parsed = parse_execution_strategy(
            _strategy_yaml("  - id: '1.1'\n    title: Setup\n")
        )
        assert parsed["tasks"][0]["blocked_by"] == []

    def test_non_list_refused(self):
        with pytest.raises(BriefParseError, match="blocked_by must be a list"):
            parse_execution_strategy(
                _strategy_yaml("  - id: '1.1'\n    blocked_by: '1.2'\n")
            )

    def test_non_string_item_refused(self):
        with pytest.raises(BriefParseError, match="must be a non-empty task id"):
            parse_execution_strategy(
                _strategy_yaml("  - id: '1.1'\n    blocked_by: [7]\n")
            )


class TestValidateTaskEdges:
    def _parsed(self, *rows: tuple[str, list[str]]) -> dict:
        return {
            "tasks": [
                {"id": tid, "blocked_by": deps} for tid, deps in rows
            ]
        }

    def test_clean_edges_return_no_errors(self):
        parsed = self._parsed(("1.1", []), ("2.1", ["1.1"]))
        assert validate_task_edges(parsed) == []

    def test_unknown_dependency_named_with_both_ids(self):
        parsed = self._parsed(("2.1", ["9.9"]))
        assert validate_task_edges(parsed) == [
            "task 2.1 blocked_by unknown task 9.9"
        ]

    def test_cycle_names_every_id_on_it(self):
        parsed = self._parsed(("1.1", ["1.2"]), ("1.2", ["1.1"]))
        errors = validate_task_edges(parsed)
        assert len(errors) == 1
        assert "cycle" in errors[0]
        assert "1.1" in errors[0] and "1.2" in errors[0]

    def test_self_edge_is_a_cycle(self):
        parsed = self._parsed(("1.1", ["1.1"]))
        assert any("cycle" in e and e.count("1.1") >= 2 for e in validate_task_edges(parsed))

    def test_ids_read_from_parsed_tasks(self):
        parsed = {
            "tasks": [
                {"id": "1.1", "blocked_by": []},
                {"id": "2.1", "blocked_by": ["1.1"]},
            ]
        }
        assert set(list_task_ids(parsed)) == {"1.1", "2.1"}
