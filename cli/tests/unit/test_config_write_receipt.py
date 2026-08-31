"""Tests for config-write event receipts and their history reader."""
from __future__ import annotations

import json
import os
from datetime import date

import pytest
from typer.testing import CliRunner

import fno.events as events
from fno.config_cli import app
from fno.config.writer import (
    _emit_write_receipts,
    set_config_value,
    set_config_values,
    unset_config_value,
)


def _capture_events(monkeypatch):
    captured = []
    monkeypatch.setattr(
        events,
        "append_event",
        lambda event, events_path=None: captured.append((event, events_path)),
    )
    return captured


def test_config_write_builder_returns_valid_event() -> None:
    assert hasattr(events, "config_write")
    event = events.config_write(
        key="review.self_review_required",
        scope="global",
        root_kind="operator",
        config_path="/tmp/config.toml",
        present_before=True,
        present_after=True,
        old_value=True,
        new_value=False,
        attester_session_id="session-1",
        attester_witness="process",
    )

    assert event["type"] == "config_write"
    assert event["source"] == "config"
    assert event["data"]["old_value"] is True
    assert event["data"]["new_value"] is False
    events.validate(event)


def test_fresh_set_emits_one_receipt_with_presence_and_real_path(tmp_path, monkeypatch) -> None:
    captured = []
    monkeypatch.setattr(
        events,
        "append_event",
        lambda event, events_path=None: captured.append((event, events_path)),
    )

    set_config_value(
        "config.review.self_review_required",
        "false",
        scope="project",
        repo_root=tmp_path,
    )

    assert len(captured) == 1
    event, journal = captured[0]
    assert event["data"] == {
        "key": "review.self_review_required",
        "scope": "project",
        "root_kind": "project",
        "config_path": str(tmp_path / ".fno" / "config.toml"),
        "present_before": False,
        "present_after": True,
        "new_value": False,
        "attester_session_id": "",
        "attester_witness": "env_only",
    }
    assert journal.name == "events.jsonl"


def test_builder_rejects_no_change() -> None:
    with pytest.raises(events.ValidationError, match="no-change"):
        events.config_write(
            key="review.self_review_required",
            scope="global",
            root_kind="operator",
            config_path="/tmp/config.toml",
            present_before=False,
            present_after=False,
            attester_session_id="",
            attester_witness="env_only",
        )


def test_overwrite_emits_old_and_new_values(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    set_config_value(
        "config.review.self_review_required", "false", scope="project", repo_root=tmp_path
    )
    captured.clear()

    set_config_value(
        "config.review.self_review_required", "true", scope="project", repo_root=tmp_path
    )

    assert len(captured) == 1
    data = captured[0][0]["data"]
    assert data["old_value"] is False
    assert data["new_value"] is True
    assert data["present_before"] is True
    assert data["present_after"] is True


def test_unset_emits_old_value_without_new_value(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    set_config_value(
        "config.review.self_review_required", "false", scope="project", repo_root=tmp_path
    )
    captured.clear()

    unset_config_value(
        "config.review.self_review_required", scope="project", repo_root=tmp_path
    )

    assert len(captured) == 1
    data = captured[0][0]["data"]
    assert data["old_value"] is False
    assert "new_value" not in data
    assert data["present_before"] is True
    assert data["present_after"] is False


def test_multiset_emits_changed_keys_only(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    captured.clear()

    set_config_values(
        [
            ("config.agents.a2a.auto", "false"),
            ("config.agents.a2a.turn_ceiling", "9"),
        ],
        scope="project",
        repo_root=tmp_path,
    )

    assert [event["data"]["key"] for event, _ in captured] == [
        "agents.a2a.turn_ceiling"
    ]


def test_unset_absent_key_keeps_file_byte_identical(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    captured.clear()
    config_path = tmp_path / ".fno" / "config.toml"
    before = config_path.read_bytes()

    unset_config_value(
        "config.auto_merge.enabled", scope="project", repo_root=tmp_path
    )

    assert captured == []
    assert config_path.read_bytes() == before


def test_secret_key_values_are_digest_redacted(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    _emit_write_receipts(
        {"api": {"key": "before"}},
        {"api": {"key": "after"}},
        tmp_path / "config.toml",
        "project",
    )

    assert len(captured) == 1
    data = captured[0][0]["data"]
    assert data["redacted"] is True
    assert data["old_value"].startswith("<redacted:sha256:")
    assert data["new_value"].startswith("<redacted:sha256:")
    assert "before" not in data["old_value"]
    assert "after" not in data["new_value"]


def test_append_failure_does_not_fail_config_write(tmp_path, monkeypatch, capsys) -> None:
    def fail_append(*args, **kwargs):
        raise OSError("journal unavailable")

    monkeypatch.setattr(events, "append_event", fail_append)
    set_config_value(
        "config.review.self_review_required", "false", scope="project", repo_root=tmp_path
    )

    assert (tmp_path / ".fno" / "config.toml").exists()
    assert "review.self_review_required" in capsys.readouterr().err


def test_symlinked_config_receipt_names_canonical_path(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree"
    (canonical / ".fno").mkdir(parents=True)
    (worktree / ".fno").mkdir(parents=True)
    canonical_config = canonical / ".fno" / "config.toml"
    canonical_config.write_text("[review]\nself_review_required = true\n", encoding="utf-8")
    linked_config = worktree / ".fno" / "config.toml"
    linked_config.symlink_to(canonical_config)

    set_config_value(
        "config.review.self_review_required", "false", scope="project", repo_root=worktree
    )

    assert captured[0][0]["data"]["config_path"] == os.path.realpath(canonical_config)
    assert linked_config.is_symlink()


def test_receipts_route_to_scope_journal(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    global_journal = tmp_path / "global-events.jsonl"
    project_journal = tmp_path / "project-events.jsonl"
    monkeypatch.setattr("fno.paths.global_events_json", lambda: global_journal)
    monkeypatch.setattr("fno.paths.project_events_json", lambda: project_journal)

    _emit_write_receipts({"review": {"max_rounds": 2}}, {"review": {"max_rounds": 3}}, tmp_path / "g.toml", "global")
    _emit_write_receipts({"review": {"max_rounds": 2}}, {"review": {"max_rounds": 3}}, tmp_path / "p.toml", "project")

    assert captured[0][1] == global_journal
    assert captured[1][1] == project_journal


_NOT_PROVIDED = object()


def _history_row(
    *,
    ts: str,
    key: str,
    scope: str,
    root_kind: str,
    present_before: bool,
    present_after: bool,
    old_value=_NOT_PROVIDED,
    new_value=_NOT_PROVIDED,
):
    kwargs = {
        "key": key,
        "scope": scope,
        "root_kind": root_kind,
        "config_path": f"/tmp/{scope}.toml",
        "present_before": present_before,
        "present_after": present_after,
        "attester_session_id": "session-1",
        "attester_witness": "process",
    }
    if old_value is not _NOT_PROVIDED:
        kwargs["old_value"] = old_value
    if new_value is not _NOT_PROVIDED:
        kwargs["new_value"] = new_value
    event = events.config_write(**kwargs)
    event["ts"] = ts
    return event


def test_history_reads_both_journals_newest_first_and_renders_unset(tmp_path, monkeypatch) -> None:
    global_journal = tmp_path / "global-events.jsonl"
    project_journal = tmp_path / "project-events.jsonl"
    monkeypatch.setattr("fno.paths.global_events_json", lambda: global_journal)
    monkeypatch.setattr("fno.paths.project_events_json", lambda: project_journal)
    global_journal.write_text(
        "\n".join(
            [
                json.dumps(
                    _history_row(
                        ts="2026-08-30T10:00:00Z",
                        key="review.self_review_required",
                        scope="global",
                        root_kind="operator",
                        present_before=True,
                        present_after=True,
                        old_value=True,
                        new_value=False,
                    )
                ),
                json.dumps(
                    _history_row(
                        ts="2026-08-30T11:00:00Z",
                        key="review.other",
                        scope="global",
                        root_kind="operator",
                        present_before=True,
                        present_after=False,
                        old_value=True,
                    )
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    project_journal.write_text(
        json.dumps(
            _history_row(
                ts="2026-08-30T12:00:00Z",
                key="review.self_review_required",
                scope="project",
                root_kind="project",
                present_before=False,
                present_after=True,
                new_value=False,
            )
        )
        + "\n",
        encoding="utf-8",
    )

    result = CliRunner().invoke(
        app,
        ["history", "review.self_review_required"],
    )

    assert result.exit_code == 0, result.output
    lines = result.stdout.splitlines()
    assert lines[0].startswith("2026-08-30T12:00:00Z")
    assert "(unset) -> false" in lines[0]
    assert "project/project" in lines[0]
    assert lines[1].startswith("2026-08-30T10:00:00Z")
    assert "true -> false" in lines[1]
    assert "global/operator" in lines[1]

    scoped = CliRunner().invoke(
        app,
        ["history", "review", "--scope", "project", "--json", "--limit", "1"],
    )
    assert scoped.exit_code == 0, scoped.output
    assert json.loads(scoped.stdout)["data"]["scope"] == "project"


def test_history_empty_result_names_both_journals(tmp_path, monkeypatch) -> None:
    global_journal = tmp_path / "global-events.jsonl"
    project_journal = tmp_path / "project-events.jsonl"
    monkeypatch.setattr("fno.paths.global_events_json", lambda: global_journal)
    monkeypatch.setattr("fno.paths.project_events_json", lambda: project_journal)

    result = CliRunner().invoke(
        app,
        ["history", "never.written"],
    )

    assert result.exit_code == 0, result.output
    assert str(global_journal) in result.stdout
    assert str(project_journal) in result.stdout


def test_non_json_leaf_value_is_recorded_in_serialized_form(tmp_path, monkeypatch) -> None:
    captured = _capture_events(monkeypatch)
    monkeypatch.setattr("fno.paths.global_events_json", lambda: tmp_path / "events.jsonl")

    _emit_write_receipts(
        {"deadline": "2026-01-01"},
        {"deadline": date(2026, 8, 31)},
        tmp_path / "config.toml",
        "global",
    )

    assert len(captured) == 1
    event, _journal = captured[0]
    assert event["data"]["new_value"] == "2026-08-31"
    events.validate(event)


def test_receipt_stage_failure_before_any_key_is_still_warned(
    tmp_path, monkeypatch, capsys
) -> None:
    _capture_events(monkeypatch)
    monkeypatch.setattr("fno.paths.global_events_json", lambda: tmp_path / "events.jsonl")

    def boom(*args, **kwargs):
        raise RuntimeError("flatten exploded")

    monkeypatch.setattr("fno.config._flatten_leaf_paths", boom)

    _emit_write_receipts({"a": 1}, {"a": 2}, tmp_path / "config.toml", "global")

    err = capsys.readouterr().err
    assert "config write receipt not recorded" in err
    assert "before any key was staged" in err
