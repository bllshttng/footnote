"""Tests for `fno agents king history` - the crown-scope reign readback.

Covers explicit and caller-derived scope resolution, cross-scope
exclusion, newest-first ordering, full evidence preservation, legacy-row
diagnostics, positive zero-match receipts, and the corrupt-journal /
missing-crown refusals.
"""
from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from fno.king.history import (
    FORBIDDEN_ALIASES,
    HistoryUnreadable,
    canonicalize_scope,
    read_history,
    render,
    resolve_scope,
)

SCOPE = "x-a792/fleet"


def _checkin(ts: str, data: dict) -> dict:
    return {"ts": ts, "type": "reign_checkin", "source": "loop", "data": data}


def _canonical(ts: str, scope: str = SCOPE, change: str = "did a thing", **extra) -> dict:
    return _checkin(ts, {"scope": scope, "change": change, **extra})


def _journal(tmp_path, rows: list[dict]):
    path = tmp_path / "events.jsonl"
    path.write_text(
        "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8"
    )
    return path


def test_canonical_rows_for_one_scope_newest_first_evidence_intact(tmp_path) -> None:
    path = _journal(
        tmp_path,
        [
            _canonical("2026-09-10T08:00:00Z", change="first check-in"),
            {
                "ts": "2026-09-10T09:00:00Z",
                "type": "phase_transition",
                "source": "loop",
                "data": {"phase": "review"},
            },
            _canonical("2026-09-10T09:30:00Z", scope="other/epic", change="not my crown"),
            _canonical(
                "2026-09-10T12:00:00Z",
                change="merged PR 1710",
                open_prs_fleet=3,
                mine=1,
                blockers="none",
            ),
        ],
    )

    result = read_history(path, SCOPE)

    assert result["matched"] == 2
    assert result["scanned"] == 4
    assert [e["ts"] for e in result["events"]] == [
        "2026-09-10T12:00:00Z",
        "2026-09-10T08:00:00Z",
    ]
    newest = result["events"][0]
    assert newest["data"]["open_prs_fleet"] == 3
    assert newest["data"]["mine"] == 1
    assert newest["data"]["blockers"] == "none"
    assert newest["data"]["change"] == "merged PR 1710"


def test_alias_rows_are_legacy_evidence_never_history(tmp_path) -> None:
    path = _journal(
        tmp_path,
        [
            _checkin(
                "2026-09-10T08:00:00Z",
                {"crown_scope": SCOPE, "change": "pre-contract row"},
            ),
            _checkin(
                "2026-09-10T08:30:00Z",
                {"scope": SCOPE, "result": "no change"},
            ),
            _checkin("2026-09-10T09:00:00Z", {"change": "no scope named at all"}),
        ],
    )

    result = read_history(path, SCOPE)

    assert result["matched"] == 0
    assert result["rejected"] == 3
    legacy = result["rejected_legacy"]
    assert len(legacy) == 2
    assert legacy[0]["line"] == 1
    assert legacy[0]["forbidden"] == ["crown_scope"]
    assert legacy[0]["missing"] == ["scope"]
    assert legacy[1]["line"] == 2
    assert legacy[1]["forbidden"] == ["result"]
    # The unattributable row counts as rejected evidence but names no crown,
    # so it cannot be laid at this scope's door.
    assert all(entry["line"] != 3 for entry in legacy)


def test_missing_change_with_matching_scope_is_rejected_evidence(tmp_path) -> None:
    path = _journal(
        tmp_path,
        [_checkin("2026-09-10T08:00:00Z", {"scope": SCOPE, "open_prs_fleet": 2})],
    )

    result = read_history(path, SCOPE)

    assert result["matched"] == 0
    assert result["rejected"] == 1
    assert result["rejected_legacy"][0]["missing"] == ["change"]
    assert result["rejected_legacy"][0]["forbidden"] == []


def test_missing_journal_reads_as_positive_zero(tmp_path) -> None:
    result = read_history(tmp_path / "absent.jsonl", SCOPE)

    assert result["scanned"] == 0
    assert result["matched"] == 0
    assert result["events"] == []
    assert result["events_path"].endswith("absent.jsonl")


def test_corrupt_line_refuses_with_line_number(tmp_path) -> None:
    path = tmp_path / "events.jsonl"
    path.write_text(
        json.dumps(_canonical("2026-09-10T08:00:00Z"))
        + "\n"
        + "{not json\n",
        encoding="utf-8",
    )

    with pytest.raises(HistoryUnreadable, match=":2: corrupt"):
        read_history(path, SCOPE)


def test_non_object_line_refuses(tmp_path) -> None:
    path = tmp_path / "events.jsonl"
    path.write_text("[1, 2]\n", encoding="utf-8")

    with pytest.raises(HistoryUnreadable, match="not a JSON object"):
        read_history(path, SCOPE)


def test_explicit_scope_is_canonicalized(tmp_path) -> None:
    assert canonicalize_scope("x-a792") == "x-a792"


def _patch_caller(monkeypatch, row) -> None:
    from fno.agents import crown

    monkeypatch.setattr(crown, "calling_agent_row", lambda: row)


def test_caller_crown_resolves_when_no_scope_given(monkeypatch) -> None:
    _patch_caller(monkeypatch, SimpleNamespace(crown_scope=SCOPE))

    assert resolve_scope("") == SCOPE


def test_unresolvable_caller_crown_refuses(monkeypatch) -> None:
    from fno.agents.crown import AGENT_UNREGISTERED, REGISTRY_UNREADABLE

    for bad in (REGISTRY_UNREADABLE, AGENT_UNREGISTERED, SimpleNamespace(crown_scope="")):
        _patch_caller(monkeypatch, bad)
        with pytest.raises(HistoryUnreadable, match="--scope"):
            resolve_scope("")


def test_explicit_scope_beats_the_caller(monkeypatch) -> None:
    _patch_caller(monkeypatch, SimpleNamespace(crown_scope="other/epic"))

    assert resolve_scope(SCOPE) == SCOPE


def test_render_prints_change_and_evidence_without_summarizing(tmp_path) -> None:
    path = _journal(
        tmp_path,
        [
            _canonical(
                "2026-09-10T12:00:00Z",
                change="merged PR 1710",
                open_prs_fleet=3,
            ),
        ],
    )
    result = read_history(path, SCOPE)
    text = render(result)

    assert "change: merged PR 1710" in text
    assert '"open_prs_fleet": 3' in text
    assert "0 canonical" not in text
    for alias in FORBIDDEN_ALIASES:
        assert alias not in text


def test_command_wiring_end_to_end(tmp_path, monkeypatch) -> None:
    """`fno agents king history` resolves through the agents king app."""
    from typer.testing import CliRunner

    from fno.king.cli import agents_king_app

    path = _journal(
        tmp_path,
        [
            _canonical("2026-09-10T08:00:00Z", change="older"),
            _canonical("2026-09-10T12:00:00Z", change="newer"),
            _canonical("2026-09-10T09:00:00Z", scope="other/epic", change="elsewhere"),
        ],
    )
    monkeypatch.setenv("FNO_EVENTS_PATH", str(path))
    result = CliRunner().invoke(
        agents_king_app, ["history", "--scope", SCOPE, "--json"]
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["scope"] == SCOPE
    assert payload["matched"] == 2
    assert [e["data"]["change"] for e in payload["events"]] == ["newer", "older"]
