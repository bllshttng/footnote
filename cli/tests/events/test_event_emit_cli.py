"""Tests for `fno event emit` CLI - canonical envelope path.

Covers AC1-HP, AC1-ERR, AC1-UI, AC1-EDGE, AC1-FR from the
events + test hygiene cleanup spec (ab-a1118224).

Routes through fno.events.__init__._build() + append_event() so
the emitted envelope is {ts, type, source, data} (canonical) rather
than the legacy {type, campaign_id, session_id, nonce, ts, payload}
envelope written by events/log.py:emit_event().
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.events.cli import cli as event_cli


@pytest.fixture
def runner() -> CliRunner:
    # Click 8.2+ separates stderr by default; mix_stderr kwarg was removed.
    return CliRunner()


def _write_state(tmp_path: Path, session_id: str = "ses-test-001") -> Path:
    """Write a minimal target-state.md so source auto-detection sees 'target'."""
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(
        f"---\nsession_id: {session_id}\nstatus: IN_PROGRESS\n"
        f"provenance_nonce: aaaa1111bbbb2222\n---\n"
    )
    return state


def _events_path(tmp_path: Path) -> Path:
    return tmp_path / ".fno" / "events.jsonl"


# ---------------------------------------------------------------------------
# AC1-HP: Canonical envelope with auto-detected source=target
# ---------------------------------------------------------------------------


def test_ac1_hp_canonical_envelope_target_source(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-HP: state file present -> source auto-detects to 'target'; envelope is canonical."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "phase_transition",
            "--data", json.dumps({
                "gate_bearing": True,
                "gate": "ledger_updated",
                "phase": "register",
                "nonce": "deadbeef" * 4,
                "session_id": "ses-test-001",
            }),
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code == 0, f"stderr={result.stderr!r} stdout={result.stdout!r}"
    assert events.exists()
    lines = events.read_text().splitlines()
    assert len(lines) == 1
    event = json.loads(lines[0])

    # Canonical envelope keys (NOT legacy {type, campaign_id, session_id, nonce, ts, payload})
    assert set(event.keys()) == {"ts", "type", "source", "data"}
    assert event["type"] == "phase_transition"
    assert event["source"] == "target"  # auto-detected from state file
    assert event["data"]["gate"] == "ledger_updated"
    assert event["data"]["phase"] == "register"
    assert event["data"]["nonce"] == "deadbeef" * 4
    assert event["data"]["session_id"] == "ses-test-001"


def test_ac1_hp_explicit_source_overrides_autodetect(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-HP: --source overrides auto-detection."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", json.dumps({"session_id": "ses-x", "nonce": "n" * 16}),
            "--source", "megawalk",
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code == 0
    event = json.loads(events.read_text().splitlines()[0])
    assert event["source"] == "megawalk"


def test_ac1_hp_no_state_file_defaults_to_test(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-HP: missing state file -> default source is 'test' (no auto-attribution)."""
    events = _events_path(tmp_path)
    nonexistent_state = tmp_path / ".fno" / "target-state.md"
    # parent dir is created by events.jsonl write, but state file itself must not exist
    events.parent.mkdir(parents=True, exist_ok=True)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", json.dumps({"session_id": "ses-y", "nonce": "n" * 16}),
            "--state", str(nonexistent_state),
            "--events", str(events),
        ],
    )

    assert result.exit_code == 0
    event = json.loads(events.read_text().splitlines()[0])
    assert event["source"] == "test"


# ---------------------------------------------------------------------------
# AC1-ERR: Unknown event type rejected; no line appended
# ---------------------------------------------------------------------------


def test_ac1_err_unknown_event_type_rejected(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-ERR: unknown --type is non-zero exit; events.jsonl unchanged."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "not_a_real_event",
            "--data", "{}",
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code != 0
    assert "unknown event type" in result.stderr.lower() or "not_a_real_event" in result.stderr
    assert not events.exists() or events.read_text() == ""


# ---------------------------------------------------------------------------
# AC1-UI: Deprecated --payload alias emits stderr warning
# ---------------------------------------------------------------------------


def test_ac1_ui_payload_alias_emits_deprecation_warning(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-UI: --payload still works but stderr warns and recommends --data."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--payload", json.dumps({"session_id": "ses-z", "nonce": "n" * 16}),
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code == 0
    # Deprecation warning surfaces on stderr.
    assert "--payload" in result.stderr
    assert "--data" in result.stderr
    # Event is still emitted with canonical envelope.
    event = json.loads(events.read_text().splitlines()[0])
    assert set(event.keys()) == {"ts", "type", "source", "data"}
    assert event["data"]["session_id"] == "ses-z"


def test_ac1_ui_data_and_payload_both_passed_errors(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-UI: passing BOTH --data and --payload is an error (ambiguous)."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", "{}",
            "--payload", "{}",
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code != 0
    assert "data" in result.stderr.lower() and "payload" in result.stderr.lower()


# ---------------------------------------------------------------------------
# AC1-EDGE: minimum-required payload for child_promise
# ---------------------------------------------------------------------------


def test_ac1_edge_child_promise_minimum_data(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-EDGE: child_promise validates with the minimum {session_id, nonce}."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", json.dumps({"session_id": "ses-abc", "nonce": "f" * 32}),
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code == 0, f"stderr={result.stderr!r}"
    event = json.loads(events.read_text().splitlines()[0])
    assert event["type"] == "child_promise"
    assert event["data"] == {"session_id": "ses-abc", "nonce": "f" * 32}


# ---------------------------------------------------------------------------
# PR #270 Codex P2: non-JSON success path must print a non-empty token
# ---------------------------------------------------------------------------


def test_pr270_codex_non_json_success_prints_nonce_when_present(
    runner: CliRunner, tmp_path: Path
) -> None:
    """Codex P2 (PR #270): shell callers using `$(fno event emit ...)` must not
    receive an empty string on success. When the event type carries a nonce
    (phase_transition, child_promise), the nonce is the token (matches the
    legacy ``emit_event`` contract that shell scripts may depend on)."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)
    nonce = "deadbeef" * 4

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", json.dumps({"session_id": "ses-x", "nonce": nonce}),
            "--state", str(state),
            "--events", str(events),
        ],
    )
    assert result.exit_code == 0
    # stdout carries the nonce, not an empty string.
    assert result.output.strip() == nonce


def test_pr270_gemini_default_paths_anchor_to_repo_root(
    runner: CliRunner, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Gemini MEDIUM (PR #270): when --events / --state are not passed, the
    defaults must anchor to the repo root, not to the current working
    directory. Otherwise running `fno event emit` from a subdirectory writes
    events to a per-subdir ``.fno/events.jsonl`` instead of the
    repository's central log."""
    # Stage a fake repo root with a state file at <root>/.fno/.
    repo_root = tmp_path / "repo"
    state = repo_root / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(
        "---\nsession_id: ses-anchored\nstatus: IN_PROGRESS\n---\n"
    )

    # Pin resolve_repo_root() to our staged root so the CLI uses the
    # anchored defaults rather than the test runner's actual CWD.
    monkeypatch.setattr(
        "fno.paths.resolve_repo_root",
        lambda: repo_root,
    )

    # Invoke from a subdirectory; do NOT pass --state or --events.
    subdir = repo_root / "deep" / "subdir"
    subdir.mkdir(parents=True, exist_ok=True)
    monkeypatch.chdir(subdir)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", json.dumps({"session_id": "ses-anchored", "nonce": "a" * 32}),
        ],
    )
    assert result.exit_code == 0, f"stderr={result.stderr!r} stdout={result.output!r}"

    # The event must land in <repo_root>/.fno/events.jsonl, NOT in the
    # subdir's own .fno/.
    anchored_events = repo_root / ".fno" / "events.jsonl"
    subdir_events = subdir / ".fno" / "events.jsonl"
    assert anchored_events.exists(), (
        f"event should land in repo root .fno, not subdir; "
        f"anchored={anchored_events!r} subdir={subdir_events!r}"
    )
    assert not subdir_events.exists(), (
        f"event leaked to subdir .fno; defaults are not anchored"
    )
    # And source auto-detection found the repo-root state file -> "target".
    event = json.loads(anchored_events.read_text().splitlines()[0])
    assert event["source"] == "target"


def test_pr270_codex_non_json_success_prints_ts_when_no_nonce(
    runner: CliRunner, tmp_path: Path
) -> None:
    """Codex P2 (PR #270): events without a nonce in data (e.g. mission_started)
    fall back to the canonical timestamp as the success token so shell callers
    still receive a non-empty value."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "mission_started",
            "--data", json.dumps({"mission_id": "mi-abc"}),
            "--source", "megatron",
            "--state", str(state),
            "--events", str(events),
        ],
    )
    assert result.exit_code == 0
    # mission_started has no nonce in data; success token falls back to ts.
    out = result.output.strip()
    assert out, "non-JSON success path must not print an empty string"
    # The token is the event's ts, which is also written to events.jsonl.
    event = json.loads(events.read_text().splitlines()[0])
    assert out == event["ts"]


# ---------------------------------------------------------------------------
# AC1-FR: Schema validation failure surfaces (no event written)
# ---------------------------------------------------------------------------


def test_ac1_fr_missing_required_data_field(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-FR: phase_transition data missing nonce/session_id -> non-zero exit, no append."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "phase_transition",
            # Missing nonce + session_id (required fields)
            "--data", json.dumps({"gate_bearing": False, "phase": "register"}),
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code != 0
    assert ("nonce" in result.stderr.lower() or "session_id" in result.stderr.lower()
            or "required" in result.stderr.lower())
    assert not events.exists() or events.read_text() == ""


def test_ac1_fr_invalid_json_in_data_rejected(runner: CliRunner, tmp_path: Path) -> None:
    """AC1-FR: malformed JSON --data -> non-zero exit, no append."""
    state = _write_state(tmp_path)
    events = _events_path(tmp_path)

    result = runner.invoke(
        event_cli,
        [
            "emit",
            "--type", "child_promise",
            "--data", "{not-json",
            "--state", str(state),
            "--events", str(events),
        ],
    )

    assert result.exit_code != 0
    assert "json" in result.stderr.lower() or "invalid" in result.stderr.lower()
    assert not events.exists() or events.read_text() == ""
