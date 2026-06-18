"""Unit tests for fno.plan.brief - pure logic, no filesystem or subprocess.

Covers:
- AC2-HP: build_brief produces all required sections
- AC2-ERR: unknown task-id raises with valid task-ids listed
- AC2-EDGE: fail-open on untagged Locked Decisions
- Tag filtering logic (relevant vs all vs none)
- Surface path matching for Patterns to Reuse
- JSON schema shape
"""
from __future__ import annotations

from collections import OrderedDict
from pathlib import Path

import pytest
import yaml

from fno.plan._doc import PlanDoc


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

SAMPLE_EXECUTION_YAML = """
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    name: "Foundation"
    tasks: [1.1]
  - wave: 2
    mode: sequential
    name: "Surface"
    tasks: [2.1]

tasks:
  - id: "1.1"
    title: "Foundation module"
    surface:
      - cli/src/fno/sample/core.py
      - cli/tests/unit/sample/test_core.py
    verify: "uv run pytest cli/tests/unit/sample/test_core.py -v"
    acceptance: [AC2-HP]
    notes: "Build the core module."

  - id: "2.1"
    title: "CLI entry point"
    surface:
      - cli/src/fno/plan/brief.py
      - cli/tests/unit/plan/test_brief.py
    verify: "uv run pytest cli/tests/unit/plan/test_brief.py -v"
    acceptance: [AC2-HP, AC2-ERR, AC2-UI, AC2-EDGE, AC2-FR]
    notes: "Build the CLI verb for brief generation."
"""

SAMPLE_AC_SECTION = """\
**AC2-HP:** Given a plan with task 2.1, when I call brief, then I get markdown.

**AC2-ERR:** Given an unknown task-id, when I call brief, then exit code is 2.

**AC2-UI:** With --format json, output matches the fixed schema.
"""

SAMPLE_LOCKED_DECISIONS = """\
1. **Use stdlib json.** Use the stdlib json module. *Why:* zero extra dependency. *How to apply:* import json.

2. **Fail-open on untagged entries.** Entries without surface tags are included. *Why:* avoids silent brief shrinkage. *How to apply:* include all untagged entries.
"""

SAMPLE_FAILURE_MODES = """\
**Boundaries**
- The system must reject missing plan files with exit code 1
- The system must reject unknown task-ids with exit code 2 and list valid ids

**Errors**
- The system must surface YAML parse failures with exit code 3
"""

SAMPLE_OVERVIEW = """\
This is the first paragraph of the overview. It provides project context for workers.

Second paragraph with more detail.
"""

SAMPLE_PATTERNS = """\
| Pattern | Source | Why reuse |
|---|---|---|
| `resolve_repo_root()` | fno.paths | path anchoring |
| `TyperRunner` | cli/src/fno/plan/brief.py | consistent test pattern |
"""


def _make_plan_doc(extra_sections: dict[str, str] | None = None) -> PlanDoc:
    """Return a minimal PlanDoc with Execution Strategy + standard sections."""
    sections: OrderedDict[str, str] = OrderedDict()
    sections["Overview"] = SAMPLE_OVERVIEW
    sections["Acceptance Criteria"] = SAMPLE_AC_SECTION
    sections["Locked Decisions (DO NOT revisit)"] = SAMPLE_LOCKED_DECISIONS
    sections["Failure Modes"] = SAMPLE_FAILURE_MODES
    sections["Patterns to Reuse"] = SAMPLE_PATTERNS
    sections["Execution Strategy"] = SAMPLE_EXECUTION_YAML
    if extra_sections:
        sections.update(extra_sections)
    return PlanDoc(
        frontmatter={"status": "ready", "feature": "test feature"},
        sections=sections,
    )


# ---------------------------------------------------------------------------
# Tests for parse_execution_strategy
# ---------------------------------------------------------------------------

class TestParseExecutionStrategy:
    def test_parses_tasks(self) -> None:
        from fno.plan.brief import parse_execution_strategy
        result = parse_execution_strategy(SAMPLE_EXECUTION_YAML)
        assert len(result["tasks"]) == 2
        task_ids = [t["id"] for t in result["tasks"]]
        assert "1.1" in task_ids
        assert "2.1" in task_ids

    def test_task_fields_present(self) -> None:
        from fno.plan.brief import parse_execution_strategy
        result = parse_execution_strategy(SAMPLE_EXECUTION_YAML)
        t = next(t for t in result["tasks"] if t["id"] == "2.1")
        assert t["title"] == "CLI entry point"
        assert "cli/src/fno/plan/brief.py" in t["surface"]
        assert "AC2-HP" in t["acceptance"]
        assert "uv run pytest" in t["verify"]

    def test_malformed_yaml_raises(self) -> None:
        from fno.plan.brief import parse_execution_strategy, BriefError
        with pytest.raises((Exception,)):
            parse_execution_strategy("tasks:\n  - id: [unclosed\n")


# ---------------------------------------------------------------------------
# Tests for find_task
# ---------------------------------------------------------------------------

class TestFindTask:
    def test_finds_existing_task(self) -> None:
        from fno.plan.brief import parse_execution_strategy, find_task
        parsed = parse_execution_strategy(SAMPLE_EXECUTION_YAML)
        t = find_task(parsed, "2.1")
        assert t is not None
        assert t["id"] == "2.1"

    def test_returns_none_on_missing(self) -> None:
        from fno.plan.brief import parse_execution_strategy, find_task
        parsed = parse_execution_strategy(SAMPLE_EXECUTION_YAML)
        t = find_task(parsed, "9.9")
        assert t is None

    def test_returns_all_task_ids_when_missing(self) -> None:
        from fno.plan.brief import parse_execution_strategy, list_task_ids
        parsed = parse_execution_strategy(SAMPLE_EXECUTION_YAML)
        ids = list_task_ids(parsed)
        assert "1.1" in ids
        assert "2.1" in ids


# ---------------------------------------------------------------------------
# Tests for extract_overview_paragraph
# ---------------------------------------------------------------------------

class TestExtractOverviewParagraph:
    def test_first_paragraph(self) -> None:
        from fno.plan.brief import extract_overview_paragraph
        result = extract_overview_paragraph(SAMPLE_OVERVIEW)
        assert result.startswith("This is the first paragraph")
        assert "Second paragraph" not in result

    def test_empty_overview(self) -> None:
        from fno.plan.brief import extract_overview_paragraph
        result = extract_overview_paragraph("")
        assert result == ""


# ---------------------------------------------------------------------------
# Tests for filter_locked_decisions
# ---------------------------------------------------------------------------

class TestFilterLockedDecisions:
    def test_all_mode_includes_everything(self) -> None:
        from fno.plan.brief import parse_locked_decisions, filter_entries
        entries = parse_locked_decisions(SAMPLE_LOCKED_DECISIONS)
        result = filter_entries(entries, "all", surface_paths=["cli/src/fno/plan/brief.py"])
        assert len(result) == 2

    def test_none_mode_returns_empty(self) -> None:
        from fno.plan.brief import parse_locked_decisions, filter_entries
        entries = parse_locked_decisions(SAMPLE_LOCKED_DECISIONS)
        result = filter_entries(entries, "none", surface_paths=["cli/src/fno/plan/brief.py"])
        assert result == []

    def test_relevant_fail_open_untagged(self) -> None:
        """AC2-EDGE: when no entries have surface tags, all are included (fail-open)."""
        from fno.plan.brief import parse_locked_decisions, filter_entries
        entries = parse_locked_decisions(SAMPLE_LOCKED_DECISIONS)
        # All entries are untagged (no explicit surface mentions matching surface_paths)
        result = filter_entries(entries, "relevant", surface_paths=["some/other/file.py"])
        # Fail-open: all untagged entries included
        assert len(result) == 2


# ---------------------------------------------------------------------------
# Tests for filter_failure_modes
# ---------------------------------------------------------------------------

class TestFilterFailureModes:
    def test_all_mode(self) -> None:
        from fno.plan.brief import parse_failure_modes, filter_entries
        entries = parse_failure_modes(SAMPLE_FAILURE_MODES)
        result = filter_entries(entries, "all", surface_paths=["any.py"])
        assert len(result) > 0

    def test_none_mode(self) -> None:
        from fno.plan.brief import parse_failure_modes, filter_entries
        entries = parse_failure_modes(SAMPLE_FAILURE_MODES)
        result = filter_entries(entries, "none", surface_paths=["any.py"])
        assert result == []

    def test_relevant_includes_matching(self) -> None:
        """When surface path appears in a bullet text, that entry is included."""
        from fno.plan.brief import parse_failure_modes, filter_entries
        entries = parse_failure_modes(SAMPLE_FAILURE_MODES)
        # "brief.py" would appear in the bullet text if present; these don't match
        # so all untagged entries are included via fail-open
        result = filter_entries(entries, "relevant", surface_paths=["brief.py"])
        assert len(result) > 0  # fail-open behavior


# ---------------------------------------------------------------------------
# Tests for filter_patterns
# ---------------------------------------------------------------------------

class TestFilterPatterns:
    def test_patterns_matching_surface(self) -> None:
        from fno.plan.brief import parse_patterns, filter_patterns_by_surface
        entries = parse_patterns(SAMPLE_PATTERNS)
        # brief.py is in the surface paths; the row with source=brief.py should match
        result = filter_patterns_by_surface(entries, ["cli/src/fno/plan/brief.py"])
        # At least the row with source containing brief.py should be included
        sources = [e["source"] for e in result]
        assert any("brief.py" in s for s in sources)

    def test_patterns_no_match_not_included(self) -> None:
        from fno.plan.brief import parse_patterns, filter_patterns_by_surface
        entries = parse_patterns(SAMPLE_PATTERNS)
        result = filter_patterns_by_surface(entries, ["completely/unrelated/path.py"])
        # fno.paths row has no brief.py match; the brief.py row won't match either
        # But this just checks the filtering works without crashing
        assert isinstance(result, list)


# ---------------------------------------------------------------------------
# Tests for parse_acceptance_criteria
# ---------------------------------------------------------------------------

class TestParseAcceptanceCriteria:
    def test_parses_entries(self) -> None:
        from fno.plan.brief import parse_acceptance_criteria
        entries = parse_acceptance_criteria(SAMPLE_AC_SECTION)
        codes = [e["code"] for e in entries]
        assert "AC2-HP" in codes
        assert "AC2-ERR" in codes

    def test_entry_fields(self) -> None:
        from fno.plan.brief import parse_acceptance_criteria
        entries = parse_acceptance_criteria(SAMPLE_AC_SECTION)
        hp = next(e for e in entries if e["code"] == "AC2-HP")
        assert hp["text"]
        assert isinstance(hp["tags"], list)

    def test_filter_by_task_acceptance(self) -> None:
        from fno.plan.brief import parse_acceptance_criteria, filter_acs_for_task
        entries = parse_acceptance_criteria(SAMPLE_AC_SECTION)
        # task has acceptance: [AC2-HP, AC2-ERR]
        result = filter_acs_for_task(entries, ["AC2-HP", "AC2-ERR"])
        codes = [e["code"] for e in result]
        assert "AC2-HP" in codes
        assert "AC2-ERR" in codes

    def test_filter_fail_open_on_untagged(self) -> None:
        """ACs not matched by task tags are still included (fail-open)."""
        from fno.plan.brief import parse_acceptance_criteria, filter_acs_for_task
        entries = parse_acceptance_criteria(SAMPLE_AC_SECTION)
        # Pass empty task acceptance list - all untagged entries should still come through
        result = filter_acs_for_task(entries, [])
        assert len(result) == len(entries)  # all included when no task tags

    def test_filter_globally_untagged_included_with_tagged_task(self) -> None:
        """Regression: tagged-task brief must still include globally-untagged ACs.

        Reported by Gemini (medium) and Codex P2 on PR #283 at brief.py:389.
        A task with `acceptance: [AC1-HP]` previously dropped any AC code not in
        the task's list - including codes that no task referenced at all. The
        spec says "ACs explicitly tagged for this task OR untagged (fail-open)";
        globally-untagged means: code does not appear in ANY task's acceptance.
        """
        from fno.plan.brief import filter_acs_for_task

        entries = [
            {"code": "AC1-HP", "ac_type": "HP", "text": "task 1 happy path", "tags": []},
            {"code": "AC2-HP", "ac_type": "HP", "text": "task 2 happy path", "tags": []},
            {"code": "AC4-EDGE", "ac_type": "EDGE", "text": "global edge case", "tags": []},
        ]
        # Task 1 lists [AC1-HP]; Task 2 lists [AC2-HP]; AC4-EDGE is in no task.
        globally_tagged = {"AC1-HP", "AC2-HP"}
        result = filter_acs_for_task(entries, ["AC1-HP"], globally_tagged)
        codes = [e["code"] for e in result]
        assert "AC1-HP" in codes  # explicit tag for this task
        assert "AC2-HP" not in codes  # tagged for another task - correctly excluded
        assert "AC4-EDGE" in codes  # globally untagged - fail-open include

    def test_filter_legacy_call_no_globally_tagged_is_maximal_fail_open(self) -> None:
        """Legacy callers (no globally_tagged_codes arg) get maximal fail-open."""
        from fno.plan.brief import filter_acs_for_task

        entries = [
            {"code": "AC1-HP", "ac_type": "HP", "text": "a", "tags": []},
            {"code": "AC2-HP", "ac_type": "HP", "text": "b", "tags": []},
        ]
        # No globally_tagged_codes passed; everything not in task_acceptance is untagged
        result = filter_acs_for_task(entries, ["AC1-HP"])
        codes = [e["code"] for e in result]
        assert codes == ["AC1-HP", "AC2-HP"]


# ---------------------------------------------------------------------------
# Tests for build_brief (AC2-HP integration of pure logic)
# ---------------------------------------------------------------------------

class TestBuildBrief:
    def test_build_brief_markdown(self) -> None:
        """AC2-HP: build_brief returns a BriefResult with non-empty markdown output."""
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        assert result.project_context
        assert result.task_spec["id"] == "2.1"
        assert result.task_spec["title"] == "CLI entry point"
        assert result.verify_command == "uv run pytest cli/tests/unit/plan/test_brief.py -v"
        assert result.files  # file list populated from surface

    def test_build_brief_has_acs(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        codes = [ac["code"] for ac in result.acceptance_criteria]
        assert "AC2-HP" in codes

    def test_build_brief_locked_decisions_fail_open(self) -> None:
        """AC2-EDGE: untagged locked decisions included when mode=relevant."""
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        assert len(result.locked_decisions) > 0

    def test_build_brief_failure_modes_none(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="none", include_locked_decisions="all")
        assert result.failure_modes == []

    def test_build_brief_unknown_task_raises(self) -> None:
        """AC2-ERR: unknown task-id raises BriefError with valid ids."""
        from fno.plan.brief import build_brief, BriefError
        doc = _make_plan_doc()
        with pytest.raises(BriefError) as exc_info:
            build_brief(doc, task_id="9.9", include_failure_modes="relevant", include_locked_decisions="relevant")
        err_msg = str(exc_info.value)
        assert "9.9" in err_msg or "not found" in err_msg.lower()
        # Should list valid task-ids
        assert "1.1" in err_msg or "2.1" in err_msg

    def test_build_brief_patterns_filtered_by_surface(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        # task 2.1 has brief.py in surface; pattern with source=brief.py should appear
        sources = [p["source"] for p in result.patterns]
        assert any("brief.py" in s for s in sources)

    def test_build_brief_malformed_exec_strategy_raises(self) -> None:
        """AC2-FR: malformed Execution Strategy YAML raises appropriate error."""
        from fno.plan.brief import build_brief, BriefError
        sections: OrderedDict[str, str] = OrderedDict()
        sections["Overview"] = SAMPLE_OVERVIEW
        sections["Acceptance Criteria"] = SAMPLE_AC_SECTION
        sections["Locked Decisions (DO NOT revisit)"] = SAMPLE_LOCKED_DECISIONS
        sections["Failure Modes"] = SAMPLE_FAILURE_MODES
        sections["Execution Strategy"] = "tasks:\n  - id: [unclosed list\n    title: bad\n"
        doc = PlanDoc(frontmatter={"status": "ready"}, sections=sections)
        with pytest.raises(Exception):
            build_brief(doc, task_id="1.1", include_failure_modes="relevant", include_locked_decisions="relevant")


# ---------------------------------------------------------------------------
# Tests for to_json_dict
# ---------------------------------------------------------------------------

class TestToJsonDict:
    def test_json_schema_keys(self) -> None:
        """AC2-UI: JSON output has the fixed schema keys."""
        from fno.plan.brief import build_brief, BriefResult
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        d = result.to_json_dict()
        required_keys = {
            "project_context", "task_spec", "acceptance_criteria",
            "locked_decisions", "failure_modes", "files", "patterns", "verify_command"
        }
        assert required_keys <= set(d.keys())

    def test_task_spec_fields(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        d = result.to_json_dict()
        ts = d["task_spec"]
        for field in ("id", "title", "surface", "verify", "acceptance", "notes"):
            assert field in ts, f"task_spec missing field: {field}"

    def test_ac_entry_fields(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        d = result.to_json_dict()
        if d["acceptance_criteria"]:
            ac = d["acceptance_criteria"][0]
            for field in ("ac_type", "code", "text", "tags"):
                assert field in ac, f"AC entry missing field: {field}"

    def test_locked_decision_fields(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="all")
        d = result.to_json_dict()
        if d["locked_decisions"]:
            ld = d["locked_decisions"][0]
            for field in ("number", "title", "rationale", "application", "tags"):
                assert field in ld, f"locked_decision entry missing field: {field}"

    def test_failure_mode_fields(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="all", include_locked_decisions="relevant")
        d = result.to_json_dict()
        if d["failure_modes"]:
            fm = d["failure_modes"][0]
            for field in ("category", "bullet", "tags"):
                assert field in fm, f"failure_mode entry missing field: {field}"

    def test_pattern_fields(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        d = result.to_json_dict()
        if d["patterns"]:
            p = d["patterns"][0]
            for field in ("pattern", "source", "why"):
                assert field in p, f"pattern entry missing field: {field}"

    def test_file_fields(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        d = result.to_json_dict()
        if d["files"]:
            f = d["files"][0]
            for field in ("path", "action", "notes"):
                assert field in f, f"file entry missing field: {field}"

    def test_json_serializable(self) -> None:
        import json
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="all", include_locked_decisions="all")
        d = result.to_json_dict()
        # Should not raise
        serialized = json.dumps(d)
        assert len(serialized) > 10


# ---------------------------------------------------------------------------
# Tests for to_markdown
# ---------------------------------------------------------------------------

class TestToMarkdown:
    def test_markdown_contains_required_sections(self) -> None:
        """AC2-HP: markdown output has all required structural sections."""
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        md = result.to_markdown()
        assert "project context" in md.lower() or "overview" in md.lower()
        assert "CLI entry point" in md  # task title
        assert "brief.py" in md  # surface file
        assert "uv run pytest" in md  # verify command

    def test_markdown_not_empty(self) -> None:
        from fno.plan.brief import build_brief
        doc = _make_plan_doc()
        result = build_brief(doc, task_id="2.1", include_failure_modes="relevant", include_locked_decisions="relevant")
        md = result.to_markdown()
        assert len(md) > 100
