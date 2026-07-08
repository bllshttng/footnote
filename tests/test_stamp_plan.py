#!/usr/bin/env python3
"""Tests for the in-package fno.plan._stamp module (was scripts/lib/stamp-plan.py).

Run: python3 tests/test_stamp_plan.py   OR   pytest tests/test_stamp_plan.py

The module lives in the cli/ package, so this test adds cli/src to sys.path
before importing it; the subprocess paths run `python3 -m fno.plan._stamp`.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CLI_SRC = REPO_ROOT / "cli" / "src"
if str(CLI_SRC) not in sys.path:
    sys.path.insert(0, str(CLI_SRC))

from fno.plan import _stamp as stamp_plan  # noqa: E402

# Subprocess invocations run the module via `python3 -m fno.plan._stamp` so
# the child resolves the in-package module. Prepend cli/src to PYTHONPATH so the
# child finds fno even when this test runs standalone from the repo root.
STAMP_MODULE_ARGS = [sys.executable, "-m", "fno.plan._stamp"]


def _stamp_env():
    import os
    env = os.environ.copy()
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = str(CLI_SRC) + (os.pathsep + existing if existing else "")
    return env


# Realistic kill_criteria block, exact shape used by
# internal/fno/plans/2026-04-27-autocorrect.md.
FRONTMATTER_WITH_KILL_CRITERIA = """---
title: Autocorrect
slug: autocorrect
created: 2026-04-27
scope: single-project
project: fno
expected_url_count: 1
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 10
    reason: Too many iterations - planning likely wrong for a 5-pt feature
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: Same test failing 3+ iterations - root cause unclear
  - name: scope_creep
    predicate: files_outside(plan_path) > 12
    reason: Touching too many files - check scope
updated: 2026-04-27T23:33
---

# Body content
"""


def test_parse_kill_criteria_does_not_raise():
    """Regression test for ab-666e4ff3.

    The stdlib-only parser previously rejected block-list-of-mappings
    (the shape kill_criteria uses) with `Malformed frontmatter at line N:
    nested structures are not supported under key 'kill_criteria'`.
    The fix should switch to opaque-verbatim preservation (RawBlock) on
    the first non-`- ` continuation line under a bare key.
    """
    fields, _block, _rest = stamp_plan.parse_frontmatter(
        FRONTMATTER_WITH_KILL_CRITERIA
    )
    assert "kill_criteria" in fields, (
        f"kill_criteria key missing from parsed fields. Got keys: {list(fields.keys())}"
    )
    val = fields["kill_criteria"]
    assert isinstance(val, stamp_plan.RawBlock), (
        f"kill_criteria value should be RawBlock, got {type(val).__name__}: {val!r}"
    )
    assert "iteration_ceiling" in val.text
    assert "stuck_test" in val.text
    assert "scope_creep" in val.text
    assert "files_outside(plan_path) > 12" in val.text


def test_serialize_round_trip_preserves_kill_criteria():
    """parse -> serialize -> parse should be stable on kill_criteria block."""
    fields, _, _ = stamp_plan.parse_frontmatter(FRONTMATTER_WITH_KILL_CRITERIA)
    serialized = stamp_plan.serialize_frontmatter(fields)
    rewrapped = f"---\n{serialized}\n---\n# Body content\n"
    fields2, _, _ = stamp_plan.parse_frontmatter(rewrapped)

    assert isinstance(fields2["kill_criteria"], stamp_plan.RawBlock)
    assert "iteration_ceiling" in fields2["kill_criteria"].text
    assert "stuck_test" in fields2["kill_criteria"].text
    assert "scope_creep" in fields2["kill_criteria"].text

    # The well-known scalars preserved correctly
    assert fields2.get("title") == "Autocorrect"
    assert fields2.get("expected_url_count") == "1"


def test_block_list_of_scalars_still_works():
    """The existing block-list-of-scalars path must still produce list[str].

    Linters sometimes normalize inline `urls: [a, b]` to block form:
        urls:
          - a
          - b
    The parser already supports that path; this test confirms the fix
    does not break it.
    """
    fm = """---
shipped_at: 2026-04-29T19:00:00Z
urls:
  - https://example.com/pull/1
  - https://example.com/pull/2
session_ids:
  - session-aaa
status: shipped
---

# Body
"""
    fields, _, _ = stamp_plan.parse_frontmatter(fm)
    assert isinstance(fields["urls"], list)
    assert fields["urls"] == [
        "https://example.com/pull/1",
        "https://example.com/pull/2",
    ]
    assert isinstance(fields["session_ids"], list)
    assert fields["session_ids"] == ["session-aaa"]


def test_stamp_subcommand_against_plan_with_kill_criteria():
    """End-to-end: cmd_stamp on a real plan-shaped file with kill_criteria."""
    with tempfile.TemporaryDirectory() as td:
        plan_file = Path(td) / "plan.md"
        plan_file.write_text(FRONTMATTER_WITH_KILL_CRITERIA)

        result = subprocess.run(
            [
                *STAMP_MODULE_ARGS,
                "stamp",
                "--plan-path",
                str(plan_file),
                "--session-id",
                "test-session-id-12345",
                "--url",
                "https://example.com/pull/777",
            ],
            capture_output=True,
            text=True,
            env=_stamp_env(),
        )
        assert result.returncode == 0, (
            f"stamp exited rc={result.returncode}\n"
            f"stdout: {result.stdout}\n"
            f"stderr: {result.stderr}"
        )

        # Re-read the file: kill_criteria must be preserved AND the new
        # stamp fields must be present.
        text = plan_file.read_text()
        assert "kill_criteria:" in text
        assert "iteration_ceiling" in text
        assert "scope_creep" in text
        assert "shipped_at:" in text
        assert "test-session-id-12345" in text
        assert "https://example.com/pull/777" in text
        assert "status: shipped" in text


def test_block_mapping_of_mappings_projects_shape():
    """The cross-project plans use a block-mapping-of-mappings under `projects:`.

    Real example: internal/fno/plans/2026-04-27-feels-active-influence-and-derivatives.md.
    Shape: a bare key followed by indented sub-keys, each with their own indented children:

        projects:
          fno:
            repo: ~/code/me/abilities
            order: 1
          chingu:
            repo: ~/code/me/chingu
            order: 2

    The parser triggers RawBlock on the first indented non-`- ` line. Verifies
    that this shape is preserved end-to-end by stamp + graduate without raising.
    """
    fm = """---
title: Cross-project plan
created: 2026-04-27
scope: cross-project
projects:
  fno:
    repo: ~/code/me/abilities
    order: 1
  chingu:
    repo: ~/code/me/chingu
    order: 2
expected_url_count: 2
---

# Body
"""
    fields, _, _ = stamp_plan.parse_frontmatter(fm)
    assert isinstance(fields["projects"], stamp_plan.RawBlock), (
        f"projects should be RawBlock, got {type(fields['projects']).__name__}"
    )
    assert "fno:" in fields["projects"].text
    assert "chingu:" in fields["projects"].text
    assert "repo: ~/code/me/abilities" in fields["projects"].text

    # round-trip preserves the projects map intact
    serialized = stamp_plan.serialize_frontmatter(fields)
    assert "fno:" in serialized
    assert "chingu:" in serialized
    assert "repo: ~/code/me/chingu" in serialized


def test_graduate_preserves_kill_criteria():
    """Stamp then graduate must round-trip kill_criteria.

    Mirrors megawalk's bare-loop sequence (stamp post-ship, graduate post-
    merge) on a kill_criteria-bearing plan. Catches regressions where
    cmd_graduate's parse->serialize cycle drops or corrupts the RawBlock.
    """
    with tempfile.TemporaryDirectory() as td:
        plan_file = Path(td) / "plan.md"
        plan_file.write_text(FRONTMATTER_WITH_KILL_CRITERIA)

        # Stamp first (this is the 38d9559-fixed path)
        r = subprocess.run(
            [
                *STAMP_MODULE_ARGS,
                "stamp",
                "--plan-path",
                str(plan_file),
                "--session-id",
                "s1",
                "--url",
                "https://example.com/pull/1",
            ],
            capture_output=True,
            text=True,
            env=_stamp_env(),
        )
        assert r.returncode == 0, f"stamp failed: {r.stderr}"

        # Graduate (must also handle the RawBlock without dropping it)
        r = subprocess.run(
            [
                *STAMP_MODULE_ARGS,
                "graduate",
                "--plan-path",
                str(plan_file),
            ],
            capture_output=True,
            text=True,
            env=_stamp_env(),
        )
        assert r.returncode == 0, f"graduate failed: {r.stderr}"

        text = plan_file.read_text()
        assert "iteration_ceiling" in text, "kill_criteria lost during graduate"
        assert "scope_creep" in text, "kill_criteria second item lost during graduate"
        assert "files_outside(plan_path) > 12" in text, "kill_criteria predicate lost"
        assert "status: done" in text, "graduate did not flip status"


def test_no_regression_on_plain_frontmatter():
    """Frontmatter without any block lists should parse identically to before."""
    fm = """---
title: Simple
created: 2026-04-29
status: planned
---

# Body
"""
    fields, _, _ = stamp_plan.parse_frontmatter(fm)
    assert fields == {
        "title": "Simple",
        "created": "2026-04-29",
        "status": "planned",
    }


def test_rawblock_repr_handles_whitespace_only_text():
    """RawBlock.__repr__ must not raise IndexError on whitespace-only text.

    `splitlines()` returns an empty list for strings like '   ' (no
    newline), even though the string is truthy. The repr fallback must
    guard against that.
    """
    # Whitespace-only (no newline) - splitlines() returns []
    rb_ws = stamp_plan.RawBlock("   ")
    assert "RawBlock(" in repr(rb_ws)

    # Empty string - already short-circuits but verify
    rb_empty = stamp_plan.RawBlock("")
    assert "RawBlock(" in repr(rb_empty)

    # Normal multi-line - first line shown
    rb_multi = stamp_plan.RawBlock("first\nsecond")
    assert "first" in repr(rb_multi)


def test_read_plan_file_strips_group_fragment():
    """Regression for ab-4fe26343.

    Epic-decomposition group nodes carry plan_path of the form
    `<doc>#group-<slug>`. The `#group-<slug>` fragment selects a section
    of the shared design doc; it is not part of any real filesystem path.
    read_plan_file must resolve the underlying doc rather than raising
    FileNotFoundError on the literal fragment path.
    """
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: shipped\nurls: [https://x/1]\n---\n# body\n")

        # Path carries a #group-<slug> fragment that does not exist on disk.
        fragment_path = Path(str(doc) + "#group-backend")
        target, fields, _rest = stamp_plan.read_plan_file(fragment_path)

        assert target == doc
        assert fields["status"] == "shipped"


def test_read_plan_file_literal_hash_filename_wins():
    """A real filename containing '#' must resolve as-is, not get stripped.

    The fragment fallback only triggers when the literal path is absent, so
    correctness for genuine '#'-bearing filenames is preserved.
    """
    with tempfile.TemporaryDirectory() as td:
        weird = Path(td) / "weird#group-x.md"
        weird.write_text("---\nstatus: shipped\n---\n# body\n")

        target, fields, _rest = stamp_plan.read_plan_file(weird)
        assert target == weird
        assert fields["status"] == "shipped"


def test_read_plan_file_non_group_fragment_fails_fast():
    """A non-`#group-` fragment must NOT be stripped (Codex P2 fail-fast).

    A typo like `spec#draft.md` that does not exist must raise rather than
    silently resolving to an existing `spec` and stamping the wrong plan.
    """
    with tempfile.TemporaryDirectory() as td:
        real = Path(td) / "spec"
        real.write_text("---\nstatus: shipped\n---\n# body\n")

        typo = Path(td) / "spec#draft.md"  # does not exist; not a #group- fragment
        try:
            stamp_plan.read_plan_file(typo)
        except FileNotFoundError:
            pass
        else:
            raise AssertionError("expected FileNotFoundError for non-#group- fragment")


def test_read_plan_file_strips_only_trailing_group_fragment():
    """rpartition preserves an earlier '#' in a real filename."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "a#b.md"
        doc.write_text("---\nstatus: shipped\n---\n# body\n")

        target, _fields, _rest = stamp_plan.read_plan_file(
            Path(str(doc) + "#group-backend")
        )
        assert target == doc


def test_read_plan_file_group_fragment_without_base_file_fails_fast():
    """`#group-` suffix only works when the stripped base path actually exists."""
    with tempfile.TemporaryDirectory() as td:
        missing = Path(td) / "missing.md#group-api"
        try:
            stamp_plan.read_plan_file(missing)
        except FileNotFoundError:
            pass
        else:
            raise AssertionError("expected FileNotFoundError when stripped base path is absent")


def test_graduate_with_group_fragment_path():
    """End-to-end: `graduate` on a `<doc>#group-<slug>` path flips shipped->done."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: shipped\nurls: [https://x/1]\nexpected_url_count: 1\n---\n# body\n")

        result = subprocess.run(
            [
                *STAMP_MODULE_ARGS, "graduate",
                "--plan-path", str(doc) + "#group-backend",
            ],
            capture_output=True, text=True, env=_stamp_env(),
        )
        assert result.returncode == 0, result.stderr
        assert "status: done" in doc.read_text()


def _run_set_expected(plan_path: str, count, extra=None):
    """Helper: invoke `set-expected` via the CLI and return the CompletedProcess."""
    argv = [
        *STAMP_MODULE_ARGS, "set-expected",
        "--plan-path", plan_path, "--count", str(count),
    ]
    if extra:
        argv += extra
    return subprocess.run(argv, capture_output=True, text=True, env=_stamp_env())


def test_set_expected_writes_count():
    """set-expected writes expected_url_count into existing frontmatter (AC0-HP)."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\ntitle: Epic\nstatus: draft\n---\n# body\n")
        r = _run_set_expected(str(doc), 3)
        assert r.returncode == 0, r.stderr
        assert "expected_url_count: 3" in doc.read_text()


def test_set_expected_overwrites_existing():
    """set-expected is authoritative: it overwrites an existing count (AC2-FR)."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: draft\nexpected_url_count: 2\n---\n# body\n")
        r = _run_set_expected(str(doc), 4)
        assert r.returncode == 0, r.stderr
        text = doc.read_text()
        assert "expected_url_count: 4" in text
        assert "expected_url_count: 2" not in text


def test_set_expected_creates_frontmatter_when_absent():
    """A doc with no frontmatter gains a block, body preserved (AC2-EDGE)."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("# Just a body\n\nsome prose\n")
        r = _run_set_expected(str(doc), 2)
        assert r.returncode == 0, r.stderr
        text = doc.read_text()
        assert text.startswith("---\n")
        assert "expected_url_count: 2" in text
        assert "# Just a body" in text
        assert "some prose" in text


def test_set_expected_resolves_group_fragment():
    """set-expected resolves a `<doc>#group-<slug>` path to the base doc."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: draft\n---\n# body\n")
        r = _run_set_expected(str(doc) + "#group-backend", 5)
        assert r.returncode == 0, r.stderr
        assert "expected_url_count: 5" in doc.read_text()


def test_set_expected_missing_doc_nonzero():
    """A missing base doc makes set-expected exit non-zero (AC1-ERR upstream)."""
    with tempfile.TemporaryDirectory() as td:
        missing = Path(td) / "nope.md"
        r = _run_set_expected(str(missing), 3)
        assert r.returncode != 0
        assert r.stderr.strip()


def test_set_expected_rejects_count_below_one():
    """--count must be >= 1; zero or negative is rejected."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: draft\n---\n# body\n")
        r = _run_set_expected(str(doc), 0)
        assert r.returncode != 0
        assert "expected_url_count: 0" not in doc.read_text()


def test_set_expected_malformed_frontmatter_nonzero_unchanged():
    """Nested/unparseable frontmatter: non-zero exit, doc byte-unchanged (AC2-ERR)."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        # A line indented at top level with no parent = the parser's nested-error path.
        original = "---\nstatus: draft\n  stray: nested\n---\n# body\n"
        doc.write_text(original)
        r = _run_set_expected(str(doc), 3)
        assert r.returncode != 0
        assert doc.read_text() == original


def test_stamp_first_writer_wins_expected_url_count():
    """stamp must NOT lower an existing expected_url_count (first-writer-wins, AC1-FR).

    decompose writes expected_url_count=3 up front; a per-group ship that
    stamps --expected-url-count 1 must leave the 3 intact.
    """
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: draft\nexpected_url_count: 3\n---\n# body\n")
        r = subprocess.run(
            [
                *STAMP_MODULE_ARGS, "stamp",
                "--plan-path", str(doc),
                "--session-id", "sid-1",
                "--url", "https://x/pr/1",
                "--expected-url-count", "1",
            ],
            capture_output=True, text=True, env=_stamp_env(),
        )
        assert r.returncode == 0, r.stderr
        text = doc.read_text()
        assert "expected_url_count: 3" in text
        assert "expected_url_count: 1" not in text


def test_stamp_sets_expected_when_absent():
    """When the field is absent, stamp still sets it (non-decomposed plans, regression)."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: draft\n---\n# body\n")
        r = subprocess.run(
            [
                *STAMP_MODULE_ARGS, "stamp",
                "--plan-path", str(doc),
                "--session-id", "sid-1",
                "--url", "https://x/pr/1",
                "--expected-url-count", "2",
            ],
            capture_output=True, text=True, env=_stamp_env(),
        )
        assert r.returncode == 0, r.stderr
        assert "expected_url_count: 2" in doc.read_text()


def test_stamp_overwrites_malformed_expected_url_count():
    """First-writer-wins treats a non-integer existing count as absent (self-heal).

    A corrupted expected_url_count would otherwise survive forever and make
    graduate fall back to 1; stamp must overwrite it with a valid value.
    """
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        doc.write_text("---\nstatus: draft\nexpected_url_count: not-a-number\n---\n# body\n")
        r = subprocess.run(
            [
                *STAMP_MODULE_ARGS, "stamp",
                "--plan-path", str(doc),
                "--session-id", "sid-1",
                "--url", "https://x/pr/1",
                "--expected-url-count", "2",
            ],
            capture_output=True, text=True, env=_stamp_env(),
        )
        assert r.returncode == 0, r.stderr
        text = doc.read_text()
        assert "expected_url_count: 2" in text
        assert "not-a-number" not in text


def test_stamp_rejects_expected_url_count_below_one():
    """stamp must reject --expected-url-count < 1 (would make graduate fire at 0 URLs)."""
    with tempfile.TemporaryDirectory() as td:
        doc = Path(td) / "design.md"
        original = "---\nstatus: draft\n---\n# body\n"
        doc.write_text(original)
        r = subprocess.run(
            [
                *STAMP_MODULE_ARGS, "stamp",
                "--plan-path", str(doc),
                "--session-id", "sid-1",
                "--url", "https://x/pr/1",
                "--expected-url-count", "0",
            ],
            capture_output=True, text=True, env=_stamp_env(),
        )
        assert r.returncode == 2, r.stderr
        # The doc must be untouched (no partial stamp written).
        assert doc.read_text() == original


def test_valid_count_helper():
    """_valid_count: integer >= 1 is valid; everything else is not."""
    assert stamp_plan._valid_count("3") is True
    assert stamp_plan._valid_count("1") is True
    assert stamp_plan._valid_count(2) is True
    assert stamp_plan._valid_count("0") is False
    assert stamp_plan._valid_count("-1") is False
    assert stamp_plan._valid_count("abc") is False
    assert stamp_plan._valid_count("") is False
    assert stamp_plan._valid_count(None) is False


def _run_standalone() -> int:
    failed = 0
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS  {name}")
            except AssertionError as exc:
                failed += 1
                print(f"FAIL  {name}\n      {exc}")
            except Exception as exc:
                failed += 1
                print(f"ERROR {name}\n      {type(exc).__name__}: {exc}")
    return failed


if __name__ == "__main__":
    sys.exit(_run_standalone())
