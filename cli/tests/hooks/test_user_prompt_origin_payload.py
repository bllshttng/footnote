"""Positive and fail-closed checks for captured UserPromptSubmit payloads."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).parents[3]
CLASSIFIER = REPO_ROOT / "scripts" / "diagnostics" / "classify-user-prompt-payload.py"


def _payload(*, prompt: str = "sentinel-typed-7f8c", **extra: object) -> dict[str, object]:
    return {
        "hook_event_name": "UserPromptSubmit",
        "session_id": "session-typed-7f8c",
        "transcript_path": "/private/transcripts/session-typed-7f8c.jsonl",
        "cwd": "/private/worktree",
        "permission_mode": "default",
        "prompt": prompt,
        **extra,
    }


def _run(tmp_path: Path, payload: dict[str, object], sentinel: str) -> subprocess.CompletedProcess[str]:
    captured = tmp_path / "payload.json"
    captured.write_text(json.dumps(payload), encoding="utf-8")
    return subprocess.run(
        [sys.executable, str(CLASSIFIER), str(captured), sentinel],
        check=False,
        capture_output=True,
        text=True,
    )


def test_recognized_payload_emits_one_positive_not_exposed_verdict(tmp_path: Path) -> None:
    result = _run(tmp_path, _payload(), "sentinel-typed-7f8c")

    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)
    assert receipt["origin_provenance"] == "not_exposed"
    assert receipt["hook_event_name"] == "UserPromptSubmit"
    assert receipt["permission_mode"] == "default"
    assert receipt["observed_keys"] == sorted(_payload())
    assert len(receipt["payload_shape_sha256"]) == 64
    assert result.stdout.count("origin_provenance") == 1


def test_mail_payload_is_bound_to_the_same_recognized_schema(tmp_path: Path) -> None:
    result = _run(tmp_path, _payload(prompt="sentinel-mail-4a21"), "sentinel-mail-4a21")

    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)["origin_provenance"] == "not_exposed"


def test_missing_sentinel_fails_closed_as_unknown_schema(tmp_path: Path) -> None:
    result = _run(tmp_path, _payload(prompt="different-prompt"), "sentinel-typed-7f8c")

    assert result.returncode != 0
    assert json.loads(result.stdout)["origin_provenance"] == "unknown_schema"


def test_unknown_event_fails_closed_as_unknown_schema(tmp_path: Path) -> None:
    result = _run(
        tmp_path,
        _payload(hook_event_name="SessionStart"),
        "sentinel-typed-7f8c",
    )

    assert result.returncode != 0
    assert json.loads(result.stdout)["origin_provenance"] == "unknown_schema"


def test_unrecognized_provenance_field_fails_closed(tmp_path: Path) -> None:
    result = _run(
        tmp_path,
        _payload(user_origin="human"),
        "sentinel-typed-7f8c",
    )

    assert result.returncode != 0
    assert json.loads(result.stdout)["origin_provenance"] == "unknown_schema"
