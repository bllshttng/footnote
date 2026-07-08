"""Integration tests for /blueprint single-doc mutation behavior.

Tests the mutate_doc.py script against real fixture files.

TDD discipline: these tests were written BEFORE implementation.
Each test maps to an acceptance criterion from the lean-blueprint plan.
"""
from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).parents[3]  # madison/
FIXTURES_DIR = REPO_ROOT / "cli" / "tests" / "fixtures" / "plans"
MUTATE_SCRIPT = REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"
GREENFIELD_FIXTURE = FIXTURES_DIR / "blueprint_input_greenfield.md"
BROWNFIELD_FIXTURE = FIXTURES_DIR / "blueprint_input_brownfield.md"


def _run_mutate(path: Path, *extra_args: str) -> subprocess.CompletedProcess:
    """Run mutate_doc.py against a given plan file path."""
    cmd = [sys.executable, str(MUTATE_SCRIPT), str(path), *extra_args]
    return subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
    )


def _copy_fixture(fixture: Path, tmp_path: Path) -> Path:
    """Copy a fixture file to a temp directory for mutation."""
    dest = tmp_path / fixture.name
    shutil.copy2(fixture, dest)
    return dest


def _load_frontmatter(path: Path) -> dict:
    """Extract YAML frontmatter from a plan file."""
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---"):
        return {}
    rest = text[4:]  # skip "---\n"
    close = rest.find("\n---")
    if close == -1:
        return {}
    fm_yaml = rest[:close]
    result = yaml.safe_load(fm_yaml)
    return result if isinstance(result, dict) else {}


def _section_hash(path: Path, section_name: str) -> str | None:
    """Return MD5 hash of a section body, or None if section absent."""
    text = path.read_text(encoding="utf-8")
    # Strip frontmatter
    if text.startswith("---"):
        close = text.find("\n---", 4)
        if close != -1:
            text = text[close + 4:]
            if text.startswith("\n"):
                text = text[1:]
    lines = text.splitlines()
    in_section = False
    section_lines = []
    for line in lines:
        if line.startswith("## ") and line[3:].rstrip() == section_name:
            in_section = True
            continue
        if in_section:
            if line.startswith("## "):
                break
            section_lines.append(line)
    if not in_section:
        return None
    body = "\n".join(section_lines)
    return hashlib.md5(body.encode()).hexdigest()


def _has_section(path: Path, section_name: str) -> bool:
    """Return True if the document has a ## section_name heading."""
    text = path.read_text(encoding="utf-8")
    return f"\n## {section_name}\n" in text or text.startswith(f"## {section_name}\n")


# ---------------------------------------------------------------------------
# AC1-HP: greenfield happy path
# ---------------------------------------------------------------------------


class TestAC1HPGreenfield:
    def test_status_transitions_to_ready(self, tmp_path):
        """AC1-HP: status=design, greenfield -> status becomes ready."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, f"Expected exit 0, got {result.returncode}\nstderr: {result.stderr}"
        fm = _load_frontmatter(doc)
        assert fm.get("status") == "ready", f"Expected status=ready, got {fm.get('status')}"

    def test_execution_strategy_section_added(self, tmp_path):
        """AC1-HP: Execution Strategy section is appended."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert _has_section(doc, "Execution Strategy"), "## Execution Strategy missing from doc"

    def test_kill_criteria_in_frontmatter(self, tmp_path):
        """AC1-HP: the default kill_criteria list is written to frontmatter in the
        canonical {name, predicate, reason} shape the engine + validate-plan.sh
        both read (the legacy flat one-key maps were invisible to both)."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        fm = _load_frontmatter(doc)
        kc = fm.get("kill_criteria")
        assert kc is not None, "kill_criteria missing from frontmatter"
        assert isinstance(kc, list), f"kill_criteria should be a list, got {type(kc)}"
        assert len(kc) == 2, f"Expected 2 default kill_criteria entries, got {len(kc)}"
        for entry in kc:
            assert {"name", "predicate", "reason"} <= entry.keys(), (
                f"entry missing required fields: {entry}"
            )

    def test_greenfield_no_file_ownership_map(self, tmp_path):
        """AC1-HP: greenfield mode does NOT add File Ownership Map."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert not _has_section(doc, "File Ownership Map"), \
            "File Ownership Map should NOT be present in greenfield mode"

    def test_greenfield_no_patterns_to_reuse(self, tmp_path):
        """AC1-HP: greenfield mode does NOT add Patterns to Reuse."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert not _has_section(doc, "Patterns to Reuse"), \
            "Patterns to Reuse should NOT be present in greenfield mode"

    def test_think_sections_unchanged(self, tmp_path):
        """AC1-HP: /think-owned sections are not modified."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        # Hash /think sections before mutation
        think_sections = ["Overview", "Architecture", "User Stories", "Failure Modes", "Acceptance Criteria"]
        before_hashes = {s: _section_hash(doc, s) for s in think_sections}
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        after_hashes = {s: _section_hash(doc, s) for s in think_sections}
        for section in think_sections:
            assert before_hashes[section] == after_hashes[section], \
                f"Section '{section}' was modified by /blueprint (should be read-only)"

    def test_execution_mode_in_frontmatter(self, tmp_path):
        """AC1-HP: execution_mode is set to 'mixed' in frontmatter."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        fm = _load_frontmatter(doc)
        assert fm.get("execution_mode") == "mixed", \
            f"Expected execution_mode=mixed, got {fm.get('execution_mode')}"

    def test_waves_in_frontmatter(self, tmp_path):
        """AC1-HP: waves is set in frontmatter as a list."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        fm = _load_frontmatter(doc)
        waves = fm.get("waves")
        assert waves is not None, "waves missing from frontmatter"
        assert isinstance(waves, list), f"waves should be a list, got {type(waves)}"


# ---------------------------------------------------------------------------
# AC1-HP: brownfield happy path
# ---------------------------------------------------------------------------


class TestAC1HPBrownfield:
    def test_brownfield_adds_file_ownership_map(self, tmp_path):
        """AC1-HP brownfield: File Ownership Map section is added."""
        doc = _copy_fixture(BROWNFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "brownfield")
        assert result.returncode == 0, f"Expected exit 0\nstderr: {result.stderr}"
        assert _has_section(doc, "File Ownership Map"), \
            "## File Ownership Map should be present in brownfield mode"

    def test_brownfield_adds_patterns_to_reuse(self, tmp_path):
        """AC1-HP brownfield: Patterns to Reuse section is added."""
        doc = _copy_fixture(BROWNFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "brownfield")
        assert result.returncode == 0, result.stderr
        assert _has_section(doc, "Patterns to Reuse"), \
            "## Patterns to Reuse should be present in brownfield mode"

    def test_brownfield_status_transitions_to_ready(self, tmp_path):
        """AC1-HP brownfield: status transitions to ready."""
        doc = _copy_fixture(BROWNFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "brownfield")
        assert result.returncode == 0, result.stderr
        fm = _load_frontmatter(doc)
        assert fm.get("status") == "ready"

    def test_brownfield_think_sections_unchanged(self, tmp_path):
        """AC1-HP brownfield: /think sections are not modified."""
        doc = _copy_fixture(BROWNFIELD_FIXTURE, tmp_path)
        think_sections = ["Overview", "Architecture", "User Stories", "Failure Modes", "Acceptance Criteria"]
        before_hashes = {s: _section_hash(doc, s) for s in think_sections}
        result = _run_mutate(doc, "--mode", "brownfield")
        assert result.returncode == 0, result.stderr
        after_hashes = {s: _section_hash(doc, s) for s in think_sections}
        for section in think_sections:
            assert before_hashes[section] == after_hashes[section], \
                f"Section '{section}' was modified by /blueprint"


# ---------------------------------------------------------------------------
# AC1-ERR: status=ready without --rewrite
# ---------------------------------------------------------------------------


class TestAC1ERR:
    def test_ready_without_rewrite_exits_1(self, tmp_path):
        """AC1-ERR: status=ready + no --rewrite -> exit 1."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        # First run to get to ready
        first = _run_mutate(doc, "--mode", "greenfield")
        assert first.returncode == 0, f"Setup failed: {first.stderr}"
        # Second run without --rewrite
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 1, \
            f"Expected exit 1 for re-run without --rewrite, got {result.returncode}"

    def test_ready_without_rewrite_stderr_message(self, tmp_path):
        """AC1-ERR: stderr message mentions 'ready' and 'rewrite'."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        _run_mutate(doc, "--mode", "greenfield")  # first run
        result = _run_mutate(doc, "--mode", "greenfield")
        assert "ready" in result.stderr, f"Expected 'ready' in stderr: {result.stderr}"
        assert "rewrite" in result.stderr, f"Expected 'rewrite' in stderr: {result.stderr}"

    def test_in_progress_exits_1_even_with_rewrite(self, tmp_path):
        """AC1-ERR: status=in_progress exits 1 even with --rewrite."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        # Manually set status to in_progress
        text = doc.read_text(encoding="utf-8")
        text = text.replace("status: design", "status: in_progress")
        doc.write_text(text, encoding="utf-8")
        result = _run_mutate(doc, "--mode", "greenfield", "--rewrite")
        assert result.returncode == 1, \
            f"Expected exit 1 for in_progress status, got {result.returncode}"


# ---------------------------------------------------------------------------
# AC1-ERR-rewrite: status=ready + --rewrite regenerates sections
# ---------------------------------------------------------------------------


class TestAC1ERRRewrite:
    def test_rewrite_succeeds_on_ready_doc(self, tmp_path):
        """AC1-ERR-rewrite: --rewrite on status=ready succeeds."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        first = _run_mutate(doc, "--mode", "greenfield")
        assert first.returncode == 0, first.stderr
        # Rewrite
        result = _run_mutate(doc, "--mode", "greenfield", "--rewrite")
        assert result.returncode == 0, \
            f"Expected exit 0 with --rewrite on ready doc, got {result.returncode}\nstderr: {result.stderr}"

    def test_rewrite_does_not_duplicate_sections(self, tmp_path):
        """AC1-ERR-rewrite: --rewrite replaces sections, not duplicates."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        _run_mutate(doc, "--mode", "greenfield")  # first run
        _run_mutate(doc, "--mode", "greenfield", "--rewrite")  # second run
        text = doc.read_text(encoding="utf-8")
        # Count occurrences of ## Execution Strategy heading
        count = text.count("\n## Execution Strategy\n")
        # May also appear at start of body
        if text.startswith("## Execution Strategy\n"):
            count += 1
        assert count == 1, f"Expected exactly 1 ## Execution Strategy, found {count}"

    def test_rewrite_preserves_think_sections(self, tmp_path):
        """AC1-ERR-rewrite: --rewrite does not touch /think-owned sections."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        _run_mutate(doc, "--mode", "greenfield")
        think_sections = ["Overview", "Architecture", "User Stories", "Failure Modes"]
        before_hashes = {s: _section_hash(doc, s) for s in think_sections}
        _run_mutate(doc, "--mode", "greenfield", "--rewrite")
        after_hashes = {s: _section_hash(doc, s) for s in think_sections}
        for section in think_sections:
            assert before_hashes[section] == after_hashes[section], \
                f"Section '{section}' modified during --rewrite"


# ---------------------------------------------------------------------------
# AC1-EDGE: missing ## Failure Modes
# ---------------------------------------------------------------------------


class TestAC1EDGE:
    def test_missing_failure_modes_exits_2(self, tmp_path):
        """AC1-EDGE: missing ## Failure Modes -> exit 2."""
        # Create a doc without ## Failure Modes
        doc = tmp_path / "no_failure_modes.md"
        doc.write_text(
            "---\nstatus: design\n---\n\n# Test\n\n## Overview\n\nSome overview.\n\n"
            "## User Stories\n\n**US1:** Some story.\n",
            encoding="utf-8",
        )
        result = _run_mutate(doc)
        assert result.returncode == 2, \
            f"Expected exit 2 for missing Failure Modes, got {result.returncode}\nstderr: {result.stderr}"

    def test_missing_failure_modes_stderr_message(self, tmp_path):
        """AC1-EDGE: stderr message mentions Failure Modes and /think."""
        doc = tmp_path / "no_failure_modes.md"
        doc.write_text(
            "---\nstatus: design\n---\n\n# Test\n\n## Overview\n\nSome overview.\n",
            encoding="utf-8",
        )
        result = _run_mutate(doc)
        assert "Failure Modes" in result.stderr, f"Expected 'Failure Modes' in stderr: {result.stderr}"


# ---------------------------------------------------------------------------
# AC1-FR: atomic write - doc unchanged on failure
# ---------------------------------------------------------------------------


class TestAC1FR:
    def test_no_emit_leaves_doc_unchanged(self, tmp_path):
        """AC1-FR: --no-emit (dry-run) does not modify the document."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        before_text = doc.read_text(encoding="utf-8")
        result = _run_mutate(doc, "--mode", "greenfield", "--no-emit")
        after_text = doc.read_text(encoding="utf-8")
        assert before_text == after_text, "Doc was modified despite --no-emit"
        assert result.returncode == 0, f"Expected exit 0 for --no-emit, got {result.returncode}"

    def test_no_emit_prints_output(self, tmp_path):
        """AC1-FR: --no-emit prints the proposed doc to stdout."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "greenfield", "--no-emit")
        assert result.returncode == 0, result.stderr
        # Should contain proposed execution strategy in stdout
        assert "Execution Strategy" in result.stdout, \
            f"Expected 'Execution Strategy' in --no-emit stdout output"


# ---------------------------------------------------------------------------
# AC3-HP: non-existent path -> redirect to /think
# ---------------------------------------------------------------------------


class TestAC3HP:
    def test_nonexistent_path_exits_1(self, tmp_path):
        """AC3-HP: non-existent path -> exit 1 with redirect message."""
        nonexistent = tmp_path / "does_not_exist.md"
        result = _run_mutate(nonexistent)
        assert result.returncode == 1, \
            f"Expected exit 1 for nonexistent path, got {result.returncode}"

    def test_nonexistent_path_stderr_mentions_think(self, tmp_path):
        """AC3-HP: redirect message mentions /think."""
        nonexistent = tmp_path / "does_not_exist.md"
        result = _run_mutate(nonexistent)
        assert "/think" in result.stderr or "think" in result.stderr.lower(), \
            f"Expected /think redirect in stderr: {result.stderr}"

    def test_nonexistent_path_stderr_mentions_path(self, tmp_path):
        """AC3-HP: redirect message includes the attempted path."""
        nonexistent = tmp_path / "does_not_exist.md"
        result = _run_mutate(nonexistent)
        assert str(nonexistent) in result.stderr or nonexistent.name in result.stderr, \
            f"Expected path in stderr: {result.stderr}"


# ---------------------------------------------------------------------------
# AC3-EDGE: feature-description string (no / in name) -> redirect
# ---------------------------------------------------------------------------


class TestAC3EDGE:
    def test_feature_description_exits_1(self):
        """AC3-EDGE: feature-description string (no /) -> exit 1."""
        cmd = [sys.executable, str(MUTATE_SCRIPT), "build a new feature"]
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            cwd=str(REPO_ROOT),
        )
        assert result.returncode == 1, \
            f"Expected exit 1 for feature description, got {result.returncode}"

    def test_feature_description_mentions_think(self):
        """AC3-EDGE: redirect message mentions /think for raw descriptions."""
        cmd = [sys.executable, str(MUTATE_SCRIPT), "build a new feature"]
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            cwd=str(REPO_ROOT),
        )
        stderr = result.stderr.lower()
        assert "think" in stderr, f"Expected 'think' redirect in stderr: {result.stderr}"


# ---------------------------------------------------------------------------
# Auto-detect greenfield/brownfield (--mode auto)
# ---------------------------------------------------------------------------


class TestAutoDetect:
    def test_auto_greenfield_on_nonexistent_paths(self, tmp_path):
        """Auto-detect: nonexistent Architecture paths -> greenfield (no File Ownership Map)."""
        doc = _copy_fixture(GREENFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "auto")
        assert result.returncode == 0, result.stderr
        # Greenfield fixture has nonexistent paths -> auto should detect greenfield
        assert not _has_section(doc, "File Ownership Map"), \
            "Should not have File Ownership Map in auto-greenfield mode"

    def test_auto_brownfield_on_existing_paths(self, tmp_path):
        """Auto-detect: existing Architecture paths -> brownfield (has File Ownership Map)."""
        doc = _copy_fixture(BROWNFIELD_FIXTURE, tmp_path)
        result = _run_mutate(doc, "--mode", "auto")
        assert result.returncode == 0, result.stderr
        # Brownfield fixture references cli/src/fno/plan/*.py which exist
        assert _has_section(doc, "File Ownership Map"), \
            "Should have File Ownership Map in auto-brownfield mode"


# ---------------------------------------------------------------------------
# Missing status in frontmatter -> exit 3
# ---------------------------------------------------------------------------


class TestMissingStatus:
    def test_missing_status_exits_3(self, tmp_path):
        """exit 3 for missing/invalid frontmatter status."""
        doc = tmp_path / "no_status.md"
        doc.write_text(
            "---\ncreated: 2026-05-18\n---\n\n# Test\n\n## Failure Modes\n\n**Boundaries**\n- boundary\n",
            encoding="utf-8",
        )
        result = _run_mutate(doc)
        assert result.returncode == 3, \
            f"Expected exit 3 for missing status, got {result.returncode}\nstderr: {result.stderr}"


# ---------------------------------------------------------------------------
# User Stories parser: multiple recognized formats
# ---------------------------------------------------------------------------


def _doc_with_us_section(us_section: str) -> str:
    """Build a minimal valid design doc whose User Stories body is `us_section`."""
    return (
        "---\nstatus: design\n---\n\n# Test\n\n"
        "## Overview\n\nSome overview text.\n\n"
        "## User Stories\n\n"
        f"{us_section.rstrip()}\n\n"
        "## Failure Modes\n\n**Boundaries**\n- some boundary\n"
    )


def _read_strategy_yaml(doc_text: str) -> dict:
    """Extract the YAML block from the ## Execution Strategy section."""
    marker = "## Execution Strategy"
    idx = doc_text.find(marker)
    assert idx != -1, "Execution Strategy section missing"
    after = doc_text[idx + len(marker):]
    fence_start = after.find("```yaml")
    assert fence_start != -1, "yaml fence missing"
    fence_end = after.find("```", fence_start + len("```yaml"))
    yaml_body = after[fence_start + len("```yaml"):fence_end]
    parsed = yaml.safe_load(yaml_body)
    assert isinstance(parsed, dict), f"expected dict, got {type(parsed)}"
    return parsed


class TestUserStoryParserFormats:
    """Parser must handle the three User Story shapes /think emits.

    Bug: pre-fix parser only matched `**USN:** desc` inline-bold. Heading-style
    stories from `/think` got silently dropped, and the parser emitted a single
    default task with a WARNING — clobbering Execution Strategy with placeholder
    content.
    """

    def test_inline_bold_format_still_parses(self, tmp_path):
        """Regression: original `**US1:** desc` format keeps working."""
        doc = tmp_path / "bold.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1:** As a developer, I want feature A.\n\n"
                "**US2:** As an operator, I want feature B.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert "no ## User Stories entries found" not in result.stderr, \
            f"Parser fell through to default task: {result.stderr}"
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 2, \
            f"Expected 2 tasks, got {len(strategy['tasks'])}: {strategy['tasks']}"

    def test_heading_format_with_colon_separator(self, tmp_path):
        """h3 heading style: `### US1: Title` with body paragraph."""
        doc = tmp_path / "heading_colon.md"
        doc.write_text(
            _doc_with_us_section(
                "### US1: Loop trips structurally when stuck\n\n"
                "Body description for US1.\n\n"
                "### US2: Help escalation\n\n"
                "Body description for US2.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert "no ## User Stories entries found" not in result.stderr, \
            f"Parser missed h3-heading stories: {result.stderr}"
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 2, \
            f"Expected 2 tasks from h3 colon style, got {len(strategy['tasks'])}"

    def test_heading_format_with_emdash_and_compound_ids(self, tmp_path):
        """h3 heading + em-dash + compound IDs (US4c.1): the format that broke us."""
        doc = tmp_path / "heading_emdash.md"
        doc.write_text(
            _doc_with_us_section(
                "### US4c.1 — operator spawns a codex worker\n\n"
                "> As an operator, I want to spawn a codex agent.\n\n"
                "### US4c.2 — operator follows up on a codex worker\n\n"
                "> As an operator, I want to deliver a follow-up.\n\n"
                "### US4c.3 — operator runs a dangerous codex agent\n\n"
                "> As an operator running a trusted greenfield repo, I want yolo mode.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert "no ## User Stories entries found" not in result.stderr, \
            f"Parser missed compound-id h3 stories: {result.stderr}"
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 3, \
            f"Expected 3 tasks from h3 em-dash style, got {len(strategy['tasks'])}"
        titles = [t["title"] for t in strategy["tasks"]]
        # Heading tail-text should drive the title, not the placeholder
        assert any("codex" in t.lower() for t in titles), \
            f"Heading title text not propagated to task titles: {titles}"

    def test_mixed_formats_in_same_doc(self, tmp_path):
        """Mixed bold + heading: parser should handle both without double-counting."""
        doc = tmp_path / "mixed.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1:** Inline bold story.\n\n"
                "### US2: Heading story\n\n"
                "Body for US2.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 2, \
            f"Mixed-format doc should yield 2 tasks, got {len(strategy['tasks'])}"

    def test_truly_empty_user_stories_still_warns(self, tmp_path):
        """Negative case: no stories at all should still emit the WARNING + default task."""
        doc = tmp_path / "empty.md"
        doc.write_text(
            _doc_with_us_section("Just some prose with no story markers at all.\n"),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert "no ## User Stories entries found" in result.stderr, \
            "Genuine empty section should still emit warning"
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 1, \
            "Genuine empty should fall through to single default task"

    def test_empty_bold_marker_does_not_steal_next_story(self, tmp_path):
        """Codex P2 / Gemini HIGH: `**US1:**` followed by `**US2:** desc` must
        not collapse into a single US1 entry; US2 must keep its own task.
        """
        doc = tmp_path / "empty_bold_steals.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1:**\n\n"
                "**US2:** real description here.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        # US1 has no description -> skipped. US2 must still be discovered.
        task_notes = {t["notes"] for t in strategy["tasks"]}
        assert "Implement US2." in task_notes, \
            f"Empty US1 swallowed US2's description; tasks={strategy['tasks']}"

    def test_empty_heading_does_not_steal_next_bold_story(self, tmp_path):
        """Gemini HIGH: `### US1` (no title) followed by `**US2:** desc` must
        not let the heading-empty fallback consume US2's line as US1's title.
        """
        doc = tmp_path / "empty_heading_steals.md"
        doc.write_text(
            _doc_with_us_section(
                "### US1\n\n"
                "**US2:** real description for US2.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        # US1 has no descriptive content -> skipped; US2 must own its task.
        notes = [t["notes"] for t in strategy["tasks"]]
        assert "Implement US2." in notes, \
            f"Empty heading stole next bold story's content; notes={notes}"
        # US1 must NOT appear -- if it did, it would mean the empty-heading
        # fallback stole US2's content as US1's title.
        assert "Implement US1." not in notes, \
            f"Empty heading was added as a task by stealing next story: notes={notes}"
        assert len(strategy["tasks"]) == 1, \
            f"Expected exactly 1 task (US2 only), got {len(strategy['tasks'])}: {strategy['tasks']}"

    def test_multiline_bold_description_recovered_from_next_paragraph(self, tmp_path):
        """Codex P2 / Gemini MEDIUM (converged): `**US1:**` with description on
        the next paragraph must still be parsed as a real story, not dropped.
        """
        doc = tmp_path / "multiline_bold.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1:**\n\n"
                "Description for US1 lives on the next paragraph.\n\n"
                "**US2:**\n\n"
                "Description for US2 also on next paragraph.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        assert "no ## User Stories entries found" not in result.stderr, \
            f"Parser dropped multi-line bold stories: {result.stderr}"
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 2, \
            f"Expected 2 multi-line bold tasks, got {len(strategy['tasks'])}: {strategy['tasks']}"
        titles = [t["title"] for t in strategy["tasks"]]
        assert any("US1" in t for t in titles) or "Description for US1" in titles[0], \
            f"US1's next-paragraph description not used as title: {titles}"

    def test_multiline_bold_does_not_steal_next_anchor(self, tmp_path):
        """Multi-line bold recovery must NOT swallow the next story's marker.

        `**US1:**` with no description, then `**US2:** desc` -- the recovery
        loop must break at the next anchor and leave US1 unmatched (it has no
        real content), not consume US2's line.
        """
        doc = tmp_path / "multiline_bold_no_steal.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1:**\n\n"
                "**US2:** real desc for US2.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        notes = [t["notes"] for t in strategy["tasks"]]
        assert "Implement US2." in notes, \
            f"Multi-line recovery dropped US2; notes={notes}"
        assert "Implement US1." not in notes, \
            f"Empty US1 was added by stealing US2's line; notes={notes}"

    def test_bold_reference_without_delimiter_is_not_an_anchor(self, tmp_path):
        """Codex P2 (third pass): bold prose like `**US1 baseline constraints**`
        must NOT be accepted as a US1 story anchor, because first-occurrence-wins
        dedup would then drop the real `**US1:** ...` story later in the doc.
        """
        doc = tmp_path / "bold_prose_reference.md"
        doc.write_text(
            _doc_with_us_section(
                "Some prose mentioning **US1 baseline constraints** as a reference.\n\n"
                "**US1:** the real story description.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 1, \
            f"Expected 1 task (the real US1 story), got {len(strategy['tasks'])}: {strategy['tasks']}"
        title = strategy["tasks"][0]["title"]
        assert "real story" in title.lower(), \
            f"Bold prose reference hijacked US1 anchor; title={title!r}"

    def test_bold_with_dash_separator_title_still_parses(self, tmp_path):
        """Regression: `**US1 - Title.** description` (dash-separator style) still works."""
        doc = tmp_path / "bold_dash.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1 - Target-loop hot path.** As a target loop, I want fast reads.\n\n"
                "**US2 - Transparent fallback.** As a developer, I want graceful degradation.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 2, \
            f"Dash-separator bold should yield 2 tasks, got {len(strategy['tasks'])}"
        notes = [t["notes"] for t in strategy["tasks"]]
        assert "Implement US1." in notes and "Implement US2." in notes, \
            f"Expected US1 and US2 notes; got {notes}"

    def test_document_order_wins_on_duplicate_ids(self, tmp_path):
        """Gemini MEDIUM: when the same ID appears in two formats, the one that
        appears FIRST in the document wins (matches docstring contract).
        """
        # US1 first as inline bold, then again as h3 heading later.
        doc = tmp_path / "dup_bold_first.md"
        doc.write_text(
            _doc_with_us_section(
                "**US1:** bold first wins.\n\n"
                "### US1: heading later (should be ignored).\n\n"
                "Body for the heading.\n"
            ),
            encoding="utf-8",
        )
        result = _run_mutate(doc, "--mode", "greenfield")
        assert result.returncode == 0, result.stderr
        strategy = _read_strategy_yaml(doc.read_text(encoding="utf-8"))
        assert len(strategy["tasks"]) == 1, \
            f"Duplicate ID should collapse to 1 task, got {len(strategy['tasks'])}"
        title = strategy["tasks"][0]["title"]
        assert "bold first wins" in title, \
            f"Expected the FIRST occurrence (bold) to win, got title={title!r}"
