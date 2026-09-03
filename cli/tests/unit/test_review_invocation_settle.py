"""The settle sweep (x-82ac change 4): a lost review invocation becomes one
`lost` attestation row, idempotently, or refuses with a named reason."""
from __future__ import annotations

import json
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.review.invocation import settle_lost_invocations


def _invocation_line(invocation_id: str, ts: str, stage: str = "sent") -> str:
    return json.dumps(
        {
            "ts": ts,
            "type": "review_invocation",
            "source": "daemon",
            "data": {
                "invocation_id": invocation_id,
                "stage": stage,
                "verb": "/code-review",
                "transport": "mail",
                "receipt": "delivered (hosted)",
            },
        }
    )


def _attestation_line(invocation_id: str, head: str = "a" * 40) -> str:
    return json.dumps(
        {
            "ts": "2026-09-03T12:00:00Z",
            "type": "review_attestation",
            "source": "hook",
            "data": {
                "reviewer": "code-review",
                "head_sha": head,
                "verdict": "pass",
                "session_id": "s",
                "invocation_id": invocation_id,
            },
        }
    )


def _write_journal(tmp_path: Path, lines: list[str]) -> Path:
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True, exist_ok=True)
    events.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return events


def _git_init(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    repo.mkdir()
    def sh(*args: str) -> None:
        subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)
    sh("init", "-q", "-b", "feature/x")
    sh("config", "user.email", "t@t")
    sh("config", "user.name", "t")
    (repo / "f.txt").write_text("x\n", encoding="utf-8")
    sh("add", "-A")
    sh("commit", "-q", "-m", "c")
    return repo


NOW = datetime(2026, 9, 3, 12, 0, 0, tzinfo=timezone.utc)


def _sent_20m_ago() -> str:
    return (NOW - timedelta(minutes=20)).strftime("%Y-%m-%dT%H:%M:%SZ")


def test_lost_invocation_settles_once_and_is_idempotent(tmp_path):
    events = _write_journal(tmp_path, [_invocation_line("ri-1", _sent_20m_ago())])
    repo = _git_init(tmp_path)

    first = settle_lost_invocations(
        ttl_minutes=15, cwd=repo, events_path=events, now=NOW
    )
    assert len(first) == 1 and first[0]["settled"] is True

    text = events.read_text(encoding="utf-8")
    rows = [json.loads(line) for line in text.splitlines() if line.strip()]
    settled = [r for r in rows if r["type"] == "review_attestation"]
    assert len(settled) == 1
    data = settled[0]["data"]
    assert data["verdict"] == "fail"
    assert data["output_contract"] == "lost"
    assert data["invocation_id"] == "ri-1"

    second = settle_lost_invocations(
        ttl_minutes=15, cwd=repo, events_path=events, now=NOW
    )
    assert all(not row["settled"] for row in second), "a second run adds zero rows"


def test_answered_invocation_never_settles(tmp_path):
    events = _write_journal(
        tmp_path,
        [_invocation_line("ri-2", _sent_20m_ago()), _attestation_line("ri-2")],
    )
    repo = _git_init(tmp_path)
    rows = settle_lost_invocations(ttl_minutes=15, cwd=repo, events_path=events, now=NOW)
    assert rows == []


def test_unjoined_sentinel_settles_nothing(tmp_path):
    events = _write_journal(tmp_path, [_invocation_line("UNJOINED", _sent_20m_ago())])
    repo = _git_init(tmp_path)
    rows = settle_lost_invocations(ttl_minutes=15, cwd=repo, events_path=events, now=NOW)
    assert rows == []


def test_fresh_invocation_stays_unsettled(tmp_path):
    recent = (NOW - timedelta(minutes=5)).strftime("%Y-%m-%dT%H:%M:%SZ")
    events = _write_journal(tmp_path, [_invocation_line("ri-3", recent)])
    repo = _git_init(tmp_path)
    rows = settle_lost_invocations(ttl_minutes=15, cwd=repo, events_path=events, now=NOW)
    assert rows == []


def test_dry_run_reports_without_writing(tmp_path):
    events = _write_journal(tmp_path, [_invocation_line("ri-4", _sent_20m_ago())])
    repo = _git_init(tmp_path)
    before = events.read_text(encoding="utf-8")
    rows = settle_lost_invocations(
        ttl_minutes=15, cwd=repo, events_path=events, now=NOW, emit=False
    )
    assert len(rows) == 1
    assert events.read_text(encoding="utf-8") == before


def test_no_git_repo_refuses_with_a_named_reason(tmp_path):
    events = _write_journal(tmp_path, [_invocation_line("ri-5", _sent_20m_ago())])
    nowhere = tmp_path / "nowhere"
    nowhere.mkdir()
    rows = settle_lost_invocations(
        ttl_minutes=15, cwd=nowhere, events_path=events, now=NOW
    )
    assert len(rows) == 1
    assert rows[0]["settled"] is False
    assert "head" in rows[0]["reason"]
    assert "review_attestation" not in events.read_text(encoding="utf-8")
