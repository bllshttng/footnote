"""Tests for fno.evals.isolation - detective attribution check.

Task 2.2: Isolation - preventive path overrides + detective attribution check.

Matches the -k evals_isolation filter.

Covers:
- AC3-HP:   clean run, no eval ids in fake state files -> clean verdict, isolation="clean"
- AC3-ERR:  eval session id planted in fake ledger -> violated + nonzero + path/id listed
- AC3-UI:   verdict line "isolation: clean|violated" appears in run output
- AC3-EDGE: fake ledger contains a DIFFERENT concurrent session id -> still clean
- AC3-FR:   guidance names file, id, line number, escape explanation
- missing real-state files -> clean (skip, not error)
- session-id collection from manifest + events.jsonl + transcript glob
- transcript_path row fill (first transcript found)
- isolation field in runner row reflects check result ("clean" or "violated")
- violation causes nonzero even when assertions passed
"""
from __future__ import annotations

import json
import os
import re
import stat
from pathlib import Path
from typing import Any

import shutil

import pytest

# run_tasks shells out to git (init/add/commit inside eval workdirs); skip
# the module gracefully in environments without the git CLI (gemini review,
# PR #451). Workdirs are git-inited by the runner itself, so repo membership
# of the test CWD is irrelevant - only CLI availability matters.
pytestmark = pytest.mark.skipif(
    shutil.which("git") is None,
    reason="git CLI not available",
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _write_jsonl_line(path: Path, data: dict) -> None:
    """Append one JSON line to *path*, creating parent dirs as needed."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(data) + "\n")


def _fake_state_paths(tmp_path: Path) -> dict[str, Path]:
    """Return a dict of injectable fake real-state paths under tmp_path.

    All files are absent by default - tests create the ones they need.
    """
    base = tmp_path / "fake-real-state"
    base.mkdir(parents=True, exist_ok=True)
    return {
        "ledger_json": base / ".fno" / "ledger.json",
        "graph_json": base / ".fno" / "graph.json",
        "repo_events_jsonl": base / "repo" / ".fno" / "events.jsonl",
        "global_events_jsonl": base / ".fno" / "events.jsonl",
        "memory_dir": base / ".fno" / "memory",
        "corrections_log": base / ".claude" / "corrections.log",
    }


# ---------------------------------------------------------------------------
# AC3-HP: clean run - no eval ids anywhere -> clean verdict
# ---------------------------------------------------------------------------


def test_isolation_clean_no_eval_ids(tmp_path: Path) -> None:
    """AC3-HP: eval session ids absent from all real-state fakes -> clean verdict."""
    from fno.evals.isolation import check_isolation, IsolationResult

    fake = _fake_state_paths(tmp_path)

    # Plant unrelated content in ledger - different session id, not an eval id
    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": "aaaa-other-session", "task": "real-work"})

    result = check_isolation(
        eval_session_ids={"eval-sid-0001", "eval-sid-0002"},
        real_state_paths=fake,
    )

    assert isinstance(result, IsolationResult)
    assert result.verdict == "clean"
    assert result.violations == []


def test_isolation_clean_all_files_absent(tmp_path: Path) -> None:
    """AC3-HP: all real-state files absent -> clean (skip, no error)."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    # Don't create any of the files

    result = check_isolation(
        eval_session_ids={"eval-sid-1234"},
        real_state_paths=fake,
    )

    assert result.verdict == "clean"
    assert result.violations == []


def test_isolation_clean_empty_eval_ids(tmp_path: Path) -> None:
    """AC3-HP: empty eval_session_ids set -> trivially clean."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": "anything"})

    result = check_isolation(
        eval_session_ids=set(),
        real_state_paths=fake,
    )

    assert result.verdict == "clean"
    assert result.violations == []


# ---------------------------------------------------------------------------
# AC3-ERR: violation - eval id planted in fake ledger
# ---------------------------------------------------------------------------


def test_isolation_violated_id_in_ledger(tmp_path: Path) -> None:
    """AC3-ERR: eval session id found in fake ledger -> violated verdict."""
    from fno.evals.isolation import check_isolation, Violation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-leaked-sid-0042"

    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": eval_id, "task": "leaked-task"})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"
    assert len(result.violations) >= 1

    v = result.violations[0]
    assert isinstance(v, Violation)
    assert eval_id in v.session_id
    assert fake["ledger_json"] == v.path or str(fake["ledger_json"]) in str(v.path)


def test_isolation_violated_id_in_graph(tmp_path: Path) -> None:
    """AC3-ERR: eval session id found in graph.json -> violated verdict."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-graph-sid-9999"

    fake["graph_json"].parent.mkdir(parents=True, exist_ok=True)
    graph_data = {"entries": [{"session_id": eval_id, "title": "leaked node"}]}
    fake["graph_json"].write_text(json.dumps(graph_data), encoding="utf-8")

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"
    paths = {str(v.path) for v in result.violations}
    assert str(fake["graph_json"]) in paths


def test_isolation_violated_id_in_repo_events(tmp_path: Path) -> None:
    """AC3-ERR: eval session id in fno repo events.jsonl -> violated."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-repo-events-sid"

    fake["repo_events_jsonl"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["repo_events_jsonl"], {"session_id": eval_id, "type": "session_start"})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"


def test_isolation_violated_id_in_global_events(tmp_path: Path) -> None:
    """AC3-ERR: eval session id in global ~/.fno/events.jsonl -> violated."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-global-events-sid"

    fake["global_events_jsonl"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["global_events_jsonl"], {"session_id": eval_id, "type": "loop_check"})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"


def test_isolation_violated_id_in_memory_file(tmp_path: Path) -> None:
    """AC3-ERR: eval session id in memory dir file -> violated."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-memory-sid-7777"

    mem_dir = fake["memory_dir"]
    mem_dir.mkdir(parents=True, exist_ok=True)
    (mem_dir / "some_memory.md").write_text(
        f"## Memory\nsession: {eval_id}\nsome notes here.\n",
        encoding="utf-8",
    )

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"


def test_isolation_violated_id_in_corrections_log(tmp_path: Path) -> None:
    """AC3-ERR: eval session id in corrections.log -> violated."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-corrections-sid-5555"

    fake["corrections_log"].parent.mkdir(parents=True, exist_ok=True)
    fake["corrections_log"].write_text(
        f"2026-06-05 session={eval_id} correction note\n",
        encoding="utf-8",
    )

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"


# ---------------------------------------------------------------------------
# AC3-EDGE: different concurrent session id -> still clean
# ---------------------------------------------------------------------------


def test_isolation_different_session_id_is_clean(tmp_path: Path) -> None:
    """AC3-EDGE: concurrent session's id in ledger but not eval's id -> clean."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    concurrent_id = "real-session-concurrent-abc"
    eval_id = "eval-my-specific-sid-xyz"

    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": concurrent_id, "task": "concurrent-work"})
    _write_jsonl_line(fake["ledger_json"], {"session_id": "other-real-work", "task": "another"})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "clean", (
        f"Concurrent session id {concurrent_id!r} should not trigger violation "
        f"for eval id {eval_id!r}"
    )
    assert result.violations == []


# ---------------------------------------------------------------------------
# AC3-FR: recovery guidance fields
# ---------------------------------------------------------------------------


def test_isolation_violation_has_line_number(tmp_path: Path) -> None:
    """AC3-FR: violation carries line number for JSONL files."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-lineno-sid"

    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    # Write 3 lines; eval id on line 2
    _write_jsonl_line(fake["ledger_json"], {"session_id": "unrelated-1"})
    _write_jsonl_line(fake["ledger_json"], {"session_id": eval_id, "escaped": True})
    _write_jsonl_line(fake["ledger_json"], {"session_id": "unrelated-2"})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"
    v = result.violations[0]
    assert v.line_number == 2


def test_isolation_violation_has_escape_explanation(tmp_path: Path) -> None:
    """AC3-FR: violation detail mentions known escapee for ledger."""
    from fno.evals.isolation import check_isolation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-escape-sid"

    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": eval_id})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    assert result.verdict == "violated"
    v = result.violations[0]
    # detail should reference the known escapee
    assert "register-task" in v.detail.lower() or "paths.sh" in v.detail.lower() or "bypass" in v.detail.lower()


def test_isolation_violation_has_all_required_fields(tmp_path: Path) -> None:
    """AC3-FR: Violation object has path, session_id, line_number, detail."""
    from fno.evals.isolation import check_isolation, Violation

    fake = _fake_state_paths(tmp_path)
    eval_id = "eval-fields-sid"

    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": eval_id})

    result = check_isolation(
        eval_session_ids={eval_id},
        real_state_paths=fake,
    )

    v = result.violations[0]
    assert isinstance(v.path, Path)
    assert isinstance(v.session_id, str)
    assert v.session_id == eval_id
    assert isinstance(v.line_number, int)
    assert v.line_number >= 1
    assert isinstance(v.detail, str)
    assert len(v.detail) > 0


# ---------------------------------------------------------------------------
# Session-id collection: manifest + events.jsonl
# ---------------------------------------------------------------------------


def test_collect_session_ids_from_manifest_and_events(tmp_path: Path) -> None:
    """COLLECT: session ids are gathered from target-state.md and events.jsonl."""
    from fno.evals.isolation import collect_eval_session_ids

    workdir = tmp_path / "workdir"
    workdir.mkdir()

    abilities_dir = workdir / ".fno"
    abilities_dir.mkdir()

    # Write target-state.md with a session_id
    manifest_sid = "manifest-session-aaa"
    (abilities_dir / "target-state.md").write_text(
        "---\n"
        f"session_id: {manifest_sid}\n"
        "plan_path: /path/to/plan.md\n"
        "---\n",
        encoding="utf-8",
    )

    # Write events.jsonl with two session ids
    event_sid_1 = "events-session-bbb"
    event_sid_2 = "events-session-ccc"
    _write_jsonl_line(abilities_dir / "events.jsonl", {"session_id": event_sid_1, "type": "start"})
    _write_jsonl_line(abilities_dir / "events.jsonl", {"session_id": event_sid_2, "type": "loop"})

    ids, transcript_path = collect_eval_session_ids(workdir, claude_projects_dir=tmp_path / "no-projects")

    assert manifest_sid in ids
    assert event_sid_1 in ids
    assert event_sid_2 in ids


def test_collect_session_ids_manifest_absent(tmp_path: Path) -> None:
    """COLLECT: absent manifest is tolerated; events ids still collected."""
    from fno.evals.isolation import collect_eval_session_ids

    workdir = tmp_path / "workdir"
    workdir.mkdir()
    abilities_dir = workdir / ".fno"
    abilities_dir.mkdir()

    event_sid = "events-only-sid"
    _write_jsonl_line(abilities_dir / "events.jsonl", {"session_id": event_sid})

    ids, transcript_path = collect_eval_session_ids(workdir, claude_projects_dir=tmp_path / "no-projects")

    assert event_sid in ids


def test_collect_session_ids_all_absent(tmp_path: Path) -> None:
    """COLLECT: workdir with no manifest and no events -> empty set."""
    from fno.evals.isolation import collect_eval_session_ids

    workdir = tmp_path / "empty-workdir"
    workdir.mkdir()

    ids, transcript_path = collect_eval_session_ids(workdir, claude_projects_dir=tmp_path / "no-projects")

    assert ids == set()
    assert transcript_path is None


# ---------------------------------------------------------------------------
# Transcript path fill
# ---------------------------------------------------------------------------


def test_collect_transcript_path_fill(tmp_path: Path) -> None:
    """COLLECT: transcript UUID found under ~/.claude/projects/<encoded-workdir>/*.jsonl."""
    from fno.evals.isolation import collect_eval_session_ids

    workdir = tmp_path / "workdir"
    workdir.mkdir()

    # Encode the workdir path as Claude does: non-[a-zA-Z0-9] -> '-'
    encoded = re.sub(r"[^a-zA-Z0-9]", "-", str(workdir))
    projects_dir = tmp_path / "fake-claude-projects"
    project_dir = projects_dir / encoded
    project_dir.mkdir(parents=True)

    transcript_sid = "transcript-session-uuid-1234"
    transcript_file = project_dir / f"{transcript_sid}.jsonl"
    transcript_file.write_text(
        json.dumps({"type": "session_start", "session_id": transcript_sid}) + "\n",
        encoding="utf-8",
    )

    abilities_dir = workdir / ".fno"
    abilities_dir.mkdir()
    _write_jsonl_line(abilities_dir / "events.jsonl", {"session_id": transcript_sid})

    ids, transcript_path = collect_eval_session_ids(workdir, claude_projects_dir=projects_dir)

    assert transcript_path is not None
    assert Path(transcript_path) == transcript_file
    assert transcript_sid in ids


def test_collect_transcript_path_absent_projects(tmp_path: Path) -> None:
    """COLLECT: no ~/.claude/projects/<encoded> dir -> transcript_path is None, no error."""
    from fno.evals.isolation import collect_eval_session_ids

    workdir = tmp_path / "workdir"
    workdir.mkdir()

    ids, transcript_path = collect_eval_session_ids(workdir, claude_projects_dir=tmp_path / "no-projects")

    assert transcript_path is None


# ---------------------------------------------------------------------------
# AC3-UI: isolation verdict line in runner output
# ---------------------------------------------------------------------------


def _write_fixture_for_isolation(
    base: Path,
    *,
    slug: str = "iso-ui-task",
) -> Path:
    """Write a minimal fixture for isolation runner integration tests."""
    fx = base / slug
    fx.mkdir(parents=True)
    repo = fx / "repo"
    repo.mkdir()

    (fx / "task.yaml").write_text(
        "title: Isolation UI Test\ntags: [test]\nbudget_usd: 1.0\n"
        "max_iterations: 3\ntimeout_secs: 60\n"
    )
    (fx / "plan.md").write_text("---\nstatus: ready\n---\n# Task\n## Goal\nTest.\n")
    assert_sh = fx / "assert.sh"
    assert_sh.write_text("#!/usr/bin/env bash\necho 'ok check-passes'\n")
    assert_sh.chmod(assert_sh.stat().st_mode | stat.S_IEXEC)
    (repo / "test_placeholder.py").write_text("def test_placeholder():\n    assert True\n")
    return fx


def _write_stub_loop_isolation(tmp_path: Path, *, script_name: str = "stub-loop.sh") -> Path:
    """Write a stub loop that exits 0 with DoneAdvisory."""
    import json as _json
    event = _json.dumps({
        "type": "termination",
        "data": {"reason": "DoneAdvisory"},
        "ts": "2026-06-05T00:00:00Z",
    })
    lines = [
        "#!/usr/bin/env bash",
        "set -e",
        "mkdir -p .fno",
        f"echo '{event}' >> .fno/events.jsonl",
        "exit 0",
    ]
    script = tmp_path / script_name
    script.write_text("\n".join(lines) + "\n")
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    return script


@pytest.fixture(autouse=True)
def _isolate_history_for_isolation_tests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Point evals_history() at a tmp file so tests never touch real state."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)


def test_isolation_verdict_line_in_output_clean(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """AC3-UI: 'isolation: clean' line appears in run output for a clean run."""
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture_for_isolation(golden)
    stub_loop = _write_stub_loop_isolation(tmp_path)

    # Inject empty real-state paths (all absent) -> clean
    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="ui-isolation-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
        real_state_paths=_fake_state_paths(tmp_path),
        claude_projects_dir=tmp_path / "no-projects",
    )

    captured = capsys.readouterr()
    output = captured.out + captured.err
    assert "isolation:" in output.lower(), (
        f"Expected 'isolation:' verdict line in output:\n{output}"
    )
    assert "clean" in output.lower(), (
        f"Expected 'clean' in isolation verdict:\n{output}"
    )


def test_isolation_verdict_line_in_output_violated(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """AC3-UI: 'isolation: violated' appears in run output when a leak is detected."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture_for_isolation(golden, slug="iso-violated-task")
    stub_loop = _write_stub_loop_isolation(tmp_path)

    # We need to plant an eval session id in a fake state file.
    # Use a stub loop that also writes a known session id to events.jsonl
    # Then plant that id in the fake ledger.
    known_sid = "eval-violated-ui-sid-9876"
    fake = _fake_state_paths(tmp_path)
    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": known_sid})

    # Create a stub loop that writes the known_sid into events.jsonl
    # so the runner picks it up as an eval session id
    event = json.dumps({
        "type": "termination",
        "data": {"reason": "DoneAdvisory"},
        "ts": "2026-06-05T00:00:00Z",
    })
    sid_event = json.dumps({"session_id": known_sid, "type": "session_start"})
    lines = [
        "#!/usr/bin/env bash",
        "set -e",
        "mkdir -p .fno",
        f"echo '{event}' >> .fno/events.jsonl",
        f"echo '{sid_event}' >> .fno/events.jsonl",
        "exit 0",
    ]
    contaminated_loop = tmp_path / "contaminated-loop.sh"
    contaminated_loop.write_text("\n".join(lines) + "\n")
    contaminated_loop.chmod(contaminated_loop.stat().st_mode | stat.S_IEXEC)

    rc = run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="violated-test",
        model="m",
        keep_workdir=False,
        loop_script=contaminated_loop,
        real_state_paths=fake,
        claude_projects_dir=tmp_path / "no-projects",
    )

    captured = capsys.readouterr()
    output = captured.out + captured.err
    assert "violated" in output.lower(), (
        f"Expected 'violated' in isolation output:\n{output}"
    )
    assert rc != 0, "Violation should cause nonzero exit even if assertions passed"


# ---------------------------------------------------------------------------
# Row isolation field reflects check result
# ---------------------------------------------------------------------------


def test_isolation_row_field_is_clean(tmp_path: Path) -> None:
    """VERIFY: history row isolation field is 'clean' when no violations."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture_for_isolation(golden, slug="row-clean-task")
    stub_loop = _write_stub_loop_isolation(tmp_path)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="row-clean-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
        real_state_paths=_fake_state_paths(tmp_path),
        claude_projects_dir=tmp_path / "no-projects",
    )

    rows = []
    history_path = paths_mod.evals_history()
    if history_path.exists():
        for line in history_path.read_text().splitlines():
            line = line.strip()
            if line:
                rows.append(json.loads(line))

    assert len(rows) >= 1
    assert rows[-1]["isolation"] == "clean", (
        f"Expected isolation='clean', got {rows[-1]['isolation']!r}"
    )


def test_isolation_row_field_is_violated(tmp_path: Path) -> None:
    """VERIFY: history row isolation field is 'violated' when violation found."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture_for_isolation(golden, slug="row-violated-task")

    known_sid = "eval-row-violated-sid-1111"
    fake = _fake_state_paths(tmp_path)
    fake["ledger_json"].parent.mkdir(parents=True, exist_ok=True)
    _write_jsonl_line(fake["ledger_json"], {"session_id": known_sid})

    event = json.dumps({"type": "termination", "data": {"reason": "DoneAdvisory"}, "ts": "2026-06-05T00:00:00Z"})
    sid_event = json.dumps({"session_id": known_sid, "type": "session_start"})
    lines = [
        "#!/usr/bin/env bash", "set -e", "mkdir -p .fno",
        f"echo '{event}' >> .fno/events.jsonl",
        f"echo '{sid_event}' >> .fno/events.jsonl",
        "exit 0",
    ]
    contaminated_loop = tmp_path / "contaminated2-loop.sh"
    contaminated_loop.write_text("\n".join(lines) + "\n")
    contaminated_loop.chmod(contaminated_loop.stat().st_mode | stat.S_IEXEC)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="row-violated",
        model="m",
        keep_workdir=False,
        loop_script=contaminated_loop,
        real_state_paths=fake,
        claude_projects_dir=tmp_path / "no-projects",
    )

    history_path = paths_mod.evals_history()
    rows = []
    for line in history_path.read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))

    assert rows[-1]["isolation"] == "violated"


# ---------------------------------------------------------------------------
# transcript_path row fill
# ---------------------------------------------------------------------------


def test_transcript_path_filled_in_row(tmp_path: Path) -> None:
    """VERIFY: transcript_path row field is filled when transcript exists."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture_for_isolation(golden, slug="transcript-task")

    known_sid = "eval-transcript-sid-4444"
    event = json.dumps({"type": "termination", "data": {"reason": "DoneAdvisory"}, "ts": "2026-06-05T00:00:00Z"})
    sid_event = json.dumps({"session_id": known_sid, "type": "session_start"})
    lines = [
        "#!/usr/bin/env bash", "set -e", "mkdir -p .fno",
        f"echo '{event}' >> .fno/events.jsonl",
        f"echo '{sid_event}' >> .fno/events.jsonl",
        "exit 0",
    ]
    loop_script = tmp_path / "transcript-loop.sh"
    loop_script.write_text("\n".join(lines) + "\n")
    loop_script.chmod(loop_script.stat().st_mode | stat.S_IEXEC)

    # Create a fake projects dir with a transcript matching the known_sid
    # The workdir is a temp dir created by run_tasks - we can't know its exact path
    # upfront. Instead, use a fake projects dir that captures any encoded path.
    # We'll verify transcript_path is set after the run by planting the transcript
    # in the encoded path of a workdir that run_tasks would create.
    # Since we can't predict the tempdir path, plant after run with keep_workdir=True.
    fake = _fake_state_paths(tmp_path)
    fake_projects = tmp_path / "fake-projects"
    fake_projects.mkdir()

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="transcript-test",
        model="m",
        keep_workdir=True,
        loop_script=loop_script,
        real_state_paths=fake,
        claude_projects_dir=fake_projects,
    )

    history_path = paths_mod.evals_history()
    rows = []
    for line in history_path.read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))

    row = rows[-1]
    # With no transcript dir created, transcript_path may be None
    # but the workdir_kept tells us the workdir path we can use to plant one
    # This test verifies the plumbing exists (transcript_path field is present)
    assert "transcript_path" in row


def test_transcript_path_filled_when_transcript_exists(tmp_path: Path) -> None:
    """VERIFY: transcript_path is filled with actual path when transcript file exists."""
    import fno.paths as paths_mod
    from fno.evals.isolation import collect_eval_session_ids

    # Directly test collect_eval_session_ids with a planted transcript
    workdir = tmp_path / "some-workdir-abc"
    workdir.mkdir()

    sid = "eval-transcript-exists-sid"

    abilities_dir = workdir / ".fno"
    abilities_dir.mkdir()
    _write_jsonl_line(abilities_dir / "events.jsonl", {"session_id": sid})

    # Plant transcript file
    encoded = re.sub(r"[^a-zA-Z0-9]", "-", str(workdir))
    projects_dir = tmp_path / "projects"
    proj_dir = projects_dir / encoded
    proj_dir.mkdir(parents=True)
    transcript_file = proj_dir / f"{sid}.jsonl"
    transcript_file.write_text(
        json.dumps({"session_id": sid, "type": "start"}) + "\n",
        encoding="utf-8",
    )

    ids, transcript_path = collect_eval_session_ids(workdir, claude_projects_dir=projects_dir)

    assert transcript_path == str(transcript_file)
    assert sid in ids
