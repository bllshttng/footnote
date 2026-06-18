#!/usr/bin/env python3
"""Tests for scripts/metrics/register-task.py.

Run: python3 tests/test_register_task.py   OR   pytest tests/test_register_task.py
"""
import importlib.util
import io
import json
import sys
import tempfile
from contextlib import redirect_stderr
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
# register-task.py moved into the fno package as fno.cost._register.
REGISTER_TASK_PATH = REPO_ROOT / "cli" / "src" / "fno" / "cost" / "_register.py"

_spec = importlib.util.spec_from_file_location("register_task", REGISTER_TASK_PATH)
register_task = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(register_task)


def _setup_state(tmp_path: Path, target_sid: str, nonce: str, transcript_uuid: str) -> Path:
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "target-state.md"
    state_file.write_text(
        "---\n"
        "status: IN_PROGRESS\n"
        f"session_id: {target_sid}\n"
        f"provenance_nonce: {nonce}\n"
        f"claude_transcript_id: {transcript_uuid}\n"
        "---\n"
    )
    return state_dir


def test_emit_ledger_transition_uses_target_session_id():
    """Regression test for ab-31391d35.

    `_emit_ledger_transition` MUST emit the target session_id (the scalar
    `entry["session_id"]`, set from `state.get("session_id")`), NOT
    `sessions[0]` which is the Claude transcript UUID passed by the stop
    hook as a CLI arg. The stop hook's `verify_provenance` greps
    events.jsonl by the target session_id read from target-state.md, so
    emitting with the transcript UUID makes the event invisible to the
    gate (no_transition_for_gate diagnostic).
    """
    target_sid = "20260420T091434Z-56177-a1b2c3"
    transcript_uuid = "11111111-2222-3333-4444-555555555555"
    nonce = "abcdef0123456789"

    with tempfile.TemporaryDirectory() as td:
        tmp_path = Path(td)
        state_dir = _setup_state(tmp_path, target_sid, nonce, transcript_uuid)

        entry = {
            "type": "execution",
            "status": "done",
            "title": "regression test for ab-31391d35",
            "root_path": str(tmp_path),
            "session_id": target_sid,
            "sessions": [transcript_uuid, target_sid],
            "pr_number": 42,
        }

        register_task._emit_ledger_transition(entry)

        events_file = state_dir / "events.jsonl"
        assert events_file.exists(), (
            f"events.jsonl was not created at {events_file}; "
            "_emit_ledger_transition silently no-op'd"
        )

        lines = [l for l in events_file.read_text().splitlines() if l.strip()]
        assert lines, (
            f"events.jsonl is empty at {events_file}; "
            "_emit_ledger_transition wrote nothing"
        )

        events = [json.loads(l) for l in lines]
        transitions = [e for e in events if e.get("type") == "phase_transition"]
        assert transitions, (
            f"No phase_transition event in events.jsonl. Lines: {lines}"
        )

        event_data = transitions[0].get("data", {})
        emitted_sid = event_data.get("session_id")

        assert emitted_sid == target_sid, (
            f"Expected emitted session_id == target_sid ({target_sid!r}), "
            f"got {emitted_sid!r}. The bug: _emit_ledger_transition reads "
            "sessions[0] (transcript UUID) instead of entry.get('session_id') "
            "(target session_id from state). Fix at "
            "scripts/metrics/register-task.py:735-736."
        )

        assert emitted_sid != transcript_uuid, (
            f"Emitted session_id is the Claude transcript UUID "
            f"({transcript_uuid!r}); the verifier in target-stop-hook.sh "
            "filters events by the target session_id, so this event would "
            "be invisible to verify_provenance for ledger_updated."
        )

        assert event_data.get("gate") == "ledger_updated"
        assert event_data.get("nonce") == nonce


def test_emit_ledger_transition_warns_when_session_id_missing():
    """Missing target session_id must surface a stderr warning and skip the emit.

    Without this guard, _emit_ledger_transition would write a phase_transition
    event with session_id="" - which verify_provenance can never match, silently
    turning the ledger_updated gate into a no-op. The warning makes the failure
    mode visible (downstream gate trip is the intended diagnostic).
    """
    nonce = "abcdef0123456789"
    transcript_uuid = "11111111-2222-3333-4444-555555555555"

    with tempfile.TemporaryDirectory() as td:
        tmp_path = Path(td)
        # state file present so nonce can be read, but no session_id
        state_dir = tmp_path / ".fno"
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "target-state.md").write_text(
            "---\n"
            "status: IN_PROGRESS\n"
            f"provenance_nonce: {nonce}\n"
            f"claude_transcript_id: {transcript_uuid}\n"
            "---\n"
        )

        entry = {
            "type": "execution",
            "status": "done",
            "title": "missing session_id case",
            "root_path": str(tmp_path),
            # session_id intentionally absent
            "sessions": [transcript_uuid],
            "pr_number": 99,
        }

        buf = io.StringIO()
        with redirect_stderr(buf):
            register_task._emit_ledger_transition(entry)
        stderr_text = buf.getvalue()

        assert "Warning" in stderr_text and "session_id" in stderr_text, (
            f"Expected stderr warning about missing session_id; got: {stderr_text!r}"
        )

        events_file = state_dir / "events.jsonl"
        assert not events_file.exists() or not events_file.read_text().strip(), (
            "events.jsonl was written despite missing session_id; emit should be skipped"
        )


def test_append_to_tasks_json_handles_bare_list_shape():
    """Regression test for ab-67063a76.

    A ledger.json written as a bare JSON list `[...]` (rather than the
    canonical `{"entries": [...]}` wrapper) parses cleanly through
    json.loads, so the `except json.JSONDecodeError` guard never fires.
    The old code then called `data.get("entries", [])` on a list and
    crashed with `'list' object has no attribute 'get'`, silently
    skipping ledger registration. append_to_tasks_json must tolerate the
    bare-list shape: treat the list as the entries, append, and rewrite
    in canonical wrapped form.
    """
    with tempfile.TemporaryDirectory() as td:
        ledger = Path(td) / "ledger.json"
        # Pre-existing legacy entry, bare-list shape (no {"entries": ...}).
        ledger.write_text(json.dumps([{"session_id": "old-1", "title": "legacy"}]))

        entry = {"session_id": "new-1", "title": "new entry"}
        register_task.append_to_tasks_json(ledger, entry)

        data = json.loads(ledger.read_text())
        assert isinstance(data, dict), "ledger should be rewritten in wrapped form"
        entries = data.get("entries", [])
        sids = [e.get("session_id") for e in entries]
        assert "old-1" in sids, "legacy bare-list entry must be preserved"
        assert "new-1" in sids, "new entry must be appended"


def test_append_to_tasks_json_recovers_from_dict_without_list_entries():
    """Hardening for ab-67063a76 (Gemini review on PR #356).

    A dict whose `entries` key is missing or non-list still cannot satisfy
    `data["entries"].append(...)`. Rather than raise (which would re-create
    the silent-skip-registration failure on the stop-hook completion path),
    append_to_tasks_json recovers like the corrupt-JSON branch: back up the
    bad file and start fresh, so the new entry still lands.
    """
    with tempfile.TemporaryDirectory() as td:
        ledger = Path(td) / "ledger.json"
        # dict present but no usable entries list.
        ledger.write_text(json.dumps({"entries": "not-a-list", "junk": 1}))

        entry = {"session_id": "new-2", "title": "after recovery"}
        register_task.append_to_tasks_json(ledger, entry)

        data = json.loads(ledger.read_text())
        assert isinstance(data, dict) and isinstance(data.get("entries"), list)
        assert [e.get("session_id") for e in data["entries"]] == ["new-2"], (
            "new entry should land after recovery from a malformed dict shape"
        )
        assert ledger.with_suffix(".json.bak").exists(), (
            "malformed prior content should be backed up, not discarded"
        )


def test_render_tasks_md_handles_bare_list_shape():
    """Regression test for ab-67063a76 (render path).

    render_tasks_md reads the same ledger.json and must not crash when it
    holds a bare list rather than the wrapped dict.
    """
    with tempfile.TemporaryDirectory() as td:
        ledger = Path(td) / "ledger.json"
        md = Path(td) / "ledger.md"
        ledger.write_text(json.dumps([{"title": "legacy", "branch": "main"}]))

        # Must not raise; should render the single legacy entry.
        register_task.render_tasks_md(ledger, md)
        assert md.exists(), "ledger.md should be rendered from bare-list ledger"
        assert "legacy" in md.read_text(), "rendered md should include the entry title"


# ─────────────────────────────────────────────────────────────────────────────
# build_entry: step-6 termination_reason / cost_json / provider_id fallback
# (ab-f8e5f214). These are the schema changes US7's per-node paper trail rests
# on; before this they had zero direct coverage (sigma-review gap 2).
# ─────────────────────────────────────────────────────────────────────────────


def test_build_entry_records_termination_reason():
    entry = register_task.build_entry(
        {"input": "x", "session_id": "sid-1"}, "tid-1", termination_reason="Budget"
    )
    assert entry["termination_reason"] == "Budget"


def test_build_entry_omits_termination_reason_when_absent():
    # Legacy byte-parity: a caller that passes no termination_reason gets an
    # entry with NO termination_reason key (old stop-hook callers unchanged).
    entry = register_task.build_entry({"input": "x", "session_id": "sid-1"}, "tid-1")
    assert "termination_reason" not in entry


def test_build_entry_cost_json_overrides_manifest():
    # Step 6's immutable manifest carries no cost; finalize passes session-cost.py
    # JSON via cost_json, which must win over any manifest total_cost.
    state = {"input": "x", "session_id": "sid", "total_cost": "9.99", "total_tokens": "1"}
    cost_json = {
        "cost_usd": 1.23,
        "tokens": {"total": 100, "cache_read": 10},
        "duration_minutes": 5.0,
        "primary_model": "claude-opus",
    }
    entry = register_task.build_entry(state, "tid", cost_json=cost_json)
    assert entry["cost_usd"] == 1.23, "cost_json wins over manifest total_cost"
    assert entry["tokens_total"] == 100
    assert entry["cache_read_tokens"] == 10
    assert entry["model"] == "claude-opus"


def test_build_entry_cost_null_when_both_absent():
    # AC7-ERR: no cost anywhere -> a thin-but-correct row with cost_usd null,
    # never a missing row.
    entry = register_task.build_entry({"input": "x", "session_id": "sid"}, "tid")
    assert entry["cost_usd"] is None
    assert entry["tokens_total"] is None


def test_build_entry_provider_id_falls_back_to_provider():
    # US7: every terminal session leaves a provider-attributed row even on a
    # standard (non-rotation) run that only has `provider`, not `provider_id`.
    entry = register_task.build_entry(
        {"input": "x", "session_id": "sid", "provider": "claude"}, "tid"
    )
    assert entry["provider_id"] == "claude"


def test_build_entry_provider_id_prefers_explicit():
    # A rotation-written provider_id wins over the provider family fallback.
    entry = register_task.build_entry(
        {"input": "x", "session_id": "sid", "provider": "claude", "provider_id": "claude-primary"},
        "tid",
    )
    assert entry["provider_id"] == "claude-primary"


def test_build_entry_no_provider_omits_provider_id():
    # Neither provider_id nor provider -> the key is omitted (legacy parity).
    entry = register_task.build_entry({"input": "x", "session_id": "sid"}, "tid")
    assert "provider_id" not in entry


# ─────────────────────────────────────────────────────────────────────────────
# build_entry: PR-number resolution from the ship handoff artifact (ab-a933adf4).
# Post-wedge the immutable manifest carries no pr_number; a worktree /target run
# used to record pr=None and never auto-link the node to its PR.
# ─────────────────────────────────────────────────────────────────────────────


def _write_ship_artifact(root: Path, sid: str, pr_number: int) -> None:
    art = root / ".fno" / "artifacts" / "handoff" / f"ship-{sid}.md"
    art.parent.mkdir(parents=True, exist_ok=True)
    art.write_text(
        f"---\nphase: ship\nsession_id: {sid}\npr_number: {pr_number}\n"
        "branch_name: feature/x\nbase_branch: main\n---\n"
    )


def test_build_entry_reads_pr_from_ship_artifact():
    # The regression that bit PR #417/#418: manifest has no pr_number, but the
    # ship artifact under the worktree root does -> build_entry must read it.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        sid = "20260606T013803Z-52762-ade1d6"
        _write_ship_artifact(root, sid, 418)
        orig_git = register_task.git_cmd

        def fake_git(*args):
            if args[:2] == ("branch", "--show-current"):
                return "feature/x"
            if args[:1] == ("remote",):
                return "git@github.com:org/repo.git"
            if args[:1] == ("rev-parse",):
                return str(root)
            return ""

        register_task.git_cmd = fake_git
        try:
            entry = register_task.build_entry({"input": "x", "session_id": sid}, "tid")
        finally:
            register_task.git_cmd = orig_git
        assert entry["pr_number"] == 418
        assert entry["pr_url"] == "https://github.com/org/repo/pull/418"


def test_build_entry_manifest_pr_still_wins():
    # A legacy manifest carrying pr_number keeps precedence over the artifact.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        sid = "sid-legacy"
        _write_ship_artifact(root, sid, 999)
        orig_git = register_task.git_cmd

        def fake_git(*args):
            if args[:1] == ("remote",):
                return "git@github.com:org/repo.git"
            if args[:1] == ("rev-parse",):
                return str(root)
            return ""

        register_task.git_cmd = fake_git
        try:
            entry = register_task.build_entry(
                {"input": "x", "session_id": sid, "pr_number": "418"}, "tid"
            )
        finally:
            register_task.git_cmd = orig_git
        assert entry["pr_number"] == 418  # manifest wins over the artifact's 999


def test_build_entry_pr_none_when_no_artifact_and_no_gh():
    # No manifest pr_number, no ship artifact, gh yields nothing -> None, never
    # an invented number (preserves the no_ship / no-PR run behavior).
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        orig_git = register_task.git_cmd
        orig_gh = register_task._pr_number_from_gh
        register_task.git_cmd = lambda *a: (str(root) if a[:1] == ("rev-parse",) else "")
        register_task._pr_number_from_gh = lambda cwd: None
        try:
            entry = register_task.build_entry({"input": "x", "session_id": "sid"}, "tid")
        finally:
            register_task.git_cmd = orig_git
            register_task._pr_number_from_gh = orig_gh
        assert entry["pr_number"] is None
        assert entry["pr_url"] is None


def test_pr_number_from_ship_artifact_absent_returns_none():
    with tempfile.TemporaryDirectory() as td:
        assert register_task._pr_number_from_ship_artifact(td, "no-such-sid") is None


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
