"""CI adapter-conformance test - the bash adapter must relay Python's verdict.

Python's ``fno.events.validate`` is the one owner of per-event validation;
``scripts/lib/events-validate.sh`` is a thin adapter that transports each
payload to ``python -m fno.events --validate-event`` and relays rc 0/1/2.
The hand-crafted corpus at ``parity_corpus.jsonl`` covers happy path,
required-field misses, forbidden-alias rejections, source/type/gate enum
violations, conditional gate invariant, mission_complete status enum, and
the 64KB data size cap.

If a record's verdicts disagree, the diagnostic names which side accepted
vs rejected. A bash rejection without a matching Python rejection means the
adapter grew a second validation brain: delete it, do not realign it.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.events import ValidationError, validate

CORPUS = Path(__file__).parent / "parity_corpus.jsonl"
REPO_ROOT = Path(__file__).resolve().parents[3]
BASH_VALIDATOR = REPO_ROOT / "scripts/lib/events-validate.sh"


def _records():
    """Yield (reason, expected_valid, event) tuples from the corpus.

    The ``data exceeds 64KB`` row carries a placeholder; the body is
    inflated to >65536 bytes here so the corpus file stays small.
    """
    for raw in CORPUS.read_text(encoding="utf-8").splitlines():
        raw = raw.strip()
        if not raw or raw.startswith("#"):
            continue
        rec = json.loads(raw)
        if rec.get("reason") == "data exceeds 64KB":
            rec["event"]["data"]["blob"] = "x" * 70_000
        yield rec


def _python_verdict(event: dict) -> tuple[bool, str]:
    try:
        validate(event)
        return True, ""
    except ValidationError as exc:
        return False, str(exc)


def _bash_verdict(event: dict, type_hint: str | None = None) -> tuple[bool, str]:
    type_str = type_hint or event.get("type", "phase_transition")
    payload = json.dumps(event, separators=(",", ":"))
    cmd = (
        f"source {BASH_VALIDATOR} && "
        f"validate_event {type_str} {json.dumps(payload)}"
    )
    proc = subprocess.run(
        ["bash", "-c", cmd],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )
    return proc.returncode == 0, proc.stderr.strip()


_RECORDS = list(_records())


def test_bash_validator_is_an_adapter_with_no_validation_brain() -> None:
    """The shell lib delegates to Python and carries no second validator.

    The retired bash implementation was jq-driven against a parsed-schema
    cache, so any reappearance of either means a second validation brain
    grew back and the two-validator drift this adapter retired can return.
    """
    script = BASH_VALIDATOR.read_text(encoding="utf-8")

    assert "from fno.events import" in script
    assert "validate(event)" in script
    assert "jq " not in script
    assert "EVENTS_SCHEMA_CACHE" not in script
    assert "required_fields=" not in script


def _adapter_verdict(event: dict | str, type_hint: str | None = None):
    type_str = type_hint or (event["type"] if isinstance(event, dict) else "reign_checkin")
    payload = event if isinstance(event, str) else json.dumps(event, separators=(",", ":"))
    cmd = f"source {BASH_VALIDATOR} && validate_event {type_str} {json.dumps(payload)}"
    return subprocess.run(["bash", "-c", cmd], capture_output=True, text=True, cwd=REPO_ROOT)


def test_adapter_shell_contract() -> None:
    valid = {
        "ts": "2026-09-10T12:00:00Z",
        "type": "reign_checkin",
        "source": "loop",
        "data": {"scope": "x-a792/fleet", "change": "merged PR 1710"},
    }

    ok = _adapter_verdict(valid)
    assert ok.returncode == 0, ok.stderr

    alias = _adapter_verdict(
        {**valid, "data": {"scope": "s", "change": "c", "crown_scope": "s"}}
    )
    assert alias.returncode == 1
    assert "forbids data field: crown_scope" in alias.stderr

    hint_mismatch = _adapter_verdict(valid, type_hint="phase_transition")
    assert hint_mismatch.returncode == 1
    assert "does not match payload type" in hint_mismatch.stderr

    garbage = _adapter_verdict("{not json")
    assert garbage.returncode == 2
    assert "not valid JSON" in garbage.stderr


def test_corpus_minimum_size() -> None:
    """The corpus must include at least the 12 hand-crafted records.

    Shrinking the corpus below the design-doc minimum is a regression -
    every category of failure must stay covered or the parity guarantee
    weakens silently.
    """
    assert len(_RECORDS) >= 12, f"corpus has only {len(_RECORDS)} records"


@pytest.mark.parametrize(
    "rec",
    _RECORDS,
    ids=[r["reason"] for r in _RECORDS],
)
def test_parity(rec: dict) -> None:
    expected_valid = rec["expect"] == "valid"
    py_ok, py_msg = _python_verdict(rec["event"])
    bash_ok, bash_msg = _bash_verdict(rec["event"])

    if py_ok != bash_ok:
        py_state = "accepted" if py_ok else f"rejected ({py_msg})"
        bash_state = "accepted" if bash_ok else f"rejected ({bash_msg})"
        pytest.fail(
            f"parity drift on {rec['reason']!r}: "
            f"python={py_state} vs bash={bash_state}"
        )

    if py_ok != expected_valid:
        verdict = "valid" if expected_valid else "invalid"
        observed = "accepted" if py_ok else f"rejected ({py_msg})"
        pytest.fail(
            f"{rec['reason']!r}: expected {verdict}, got {observed}"
        )


def test_overflow_exponent_is_rejected_on_the_raw_bash_wire() -> None:
    raw = (
        '{"ts":"2026-07-26T01:00:00Z","type":"context_snapshot","source":"hook",'
        '"data":{"session_id":"s","harness":"codex","entry_state":"startup",'
        '"context_bytes":1e999,"estimated_tokens":1.7976931348623157e308,'
        '"context_hash":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881",'
        '"source_hashes":["x"],"source_manifest":[{"source_id":"x","status":"observed",'
        '"bytes":1e999,"content_hash":"x"}],"measurement_complete":true,'
        '"measurement_errors":[]}}'
    )
    with pytest.raises(ValidationError):
        validate(json.loads(raw))
    command = (
        f"source {BASH_VALIDATOR} && payload=$(cat) && "
        'validate_event context_snapshot "$payload"'
    )
    result = subprocess.run(
        ["bash", "-c", command],
        input=raw,
        text=True,
        capture_output=True,
        cwd=REPO_ROOT,
    )

    assert result.returncode != 0


def test_verification_receipt_command_count_cap_has_python_bash_parity() -> None:
    event = {
        "ts": "2026-07-26T01:02:04Z",
        "type": "verification_receipt",
        "source": "target",
        "data": {
            "candidate_sha": "a" * 40,
            "command": ["x"] * 4097,
            "environment": {"host": "h", "platform": "p", "runner": "r"},
            "scope": ["smoke"],
            "started_at": "2026-07-26T01:00:00Z",
            "finished_at": "2026-07-26T01:02:03Z",
            "mode": "full",
            "result": "failed",
            "producer": {"kind": "preflight", "id": "h:1"},
            "steps_expected": 1,
            "steps_executed": 1,
        },
    }

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is False
    assert bash_ok is False


def test_verification_receipt_utf8_byte_cap_has_python_bash_parity() -> None:
    event = {
        "ts": "2026-07-26T01:02:04Z",
        "type": "verification_receipt",
        "source": "target",
        "data": {
            "candidate_sha": "a" * 40,
            "command": ["é" * 4096],
            "environment": {"host": "h", "platform": "p", "runner": "r"},
            "scope": ["smoke"],
            "started_at": "2026-07-26T01:00:00Z",
            "finished_at": "2026-07-26T01:02:03Z",
            "mode": "full",
            "result": "failed",
            "producer": {"kind": "preflight", "id": "h:1"},
            "steps_expected": 1,
            "steps_executed": 1,
        },
    }

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is False
    assert bash_ok is False


@pytest.mark.parametrize(
    ("detail", "expected"),
    [
        ("é" * 10_000, True),
        ("é" * 11_000, False),
        ("\x7f" * 10_000, True),
        ("\x7f" * 11_000, False),
    ],
)
def test_event_data_cap_counts_compact_ascii_json_in_python_and_bash(
    detail: str, expected: bool
) -> None:
    event = next(
        json.loads(json.dumps(rec["event"]))
        for rec in _RECORDS
        if rec["reason"] == "verification_receipt full passed exact sha"
    )
    event["data"]["detail"] = detail

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is expected
    assert bash_ok is expected


def test_event_data_lone_surrogate_rejects_in_python_and_bash() -> None:
    event = next(
        json.loads(json.dumps(rec["event"]))
        for rec in _RECORDS
        if rec["reason"] == "verification_receipt full passed exact sha"
    )
    event["data"]["detail"] = "\ud800"

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is False
    assert bash_ok is False


@pytest.mark.parametrize("field", ["ts", "started_at", "finished_at"])
@pytest.mark.parametrize(
    "timestamp",
    [
        "2023-02-29T00:00:00Z",
        "2016-12-31T23:59:60Z",
        "0000-01-01T00:00:00Z",
    ],
)
def test_verification_receipt_calendar_timestamp_parity(
    field: str, timestamp: str
) -> None:
    event = next(
        json.loads(json.dumps(rec["event"]))
        for rec in _RECORDS
        if rec["reason"] == "verification_receipt full passed exact sha"
    )
    target = event if field == "ts" else event["data"]
    target[field] = timestamp

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is False
    assert bash_ok is False


@pytest.mark.parametrize(
    "timestamp",
    [
        "0001-01-01T00:00:00Z",
        "1969-12-31T23:59:59.123456Z",
    ],
)
def test_verification_receipt_pre_epoch_timestamp_parity(timestamp: str) -> None:
    event = next(
        json.loads(json.dumps(rec["event"]))
        for rec in _RECORDS
        if rec["reason"] == "verification_receipt full passed exact sha"
    )
    event["ts"] = timestamp
    event["data"]["started_at"] = timestamp
    event["data"]["finished_at"] = timestamp

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is True
    assert bash_ok is True


def test_verification_receipt_far_future_microsecond_ordering_parity() -> None:
    event = next(
        json.loads(json.dumps(rec["event"]))
        for rec in _RECORDS
        if rec["reason"] == "verification_receipt full passed exact sha"
    )
    event["ts"] = "9999-12-31T23:59:59.000003Z"
    event["data"]["started_at"] = "9999-12-31T23:59:59.000002Z"
    event["data"]["finished_at"] = "9999-12-31T23:59:59.000001Z"

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is False
    assert bash_ok is False


@pytest.mark.parametrize(
    ("generation", "expected"),
    [
        (9_007_199_254_740_991, True),
        (9_007_199_254_740_992, False),
    ],
)
def test_verification_receipt_generation_safe_integer_parity(
    generation: int, expected: bool
) -> None:
    event = {
        "ts": "2026-07-26T01:02:04Z",
        "type": "verification_receipt",
        "source": "target",
        "data": {
            "candidate_sha": "a" * 40,
            "command": ["scripts/ci/preflight.sh"],
            "environment": {
                "host": "h",
                "platform": "p",
                "runner": "scripts/ci/preflight.sh",
            },
            "scope": ["smoke"],
            "started_at": "2026-07-26T01:00:00Z",
            "finished_at": "2026-07-26T01:02:03Z",
            "mode": "full",
            "result": "failed",
            "producer": {"kind": "preflight", "id": "h:1"},
            "generation": generation,
            "steps_expected": 1,
            "steps_executed": 1,
        },
    }

    py_ok, _ = _python_verdict(event)
    bash_ok, _ = _bash_verdict(event)

    assert py_ok is expected
    assert bash_ok is expected
