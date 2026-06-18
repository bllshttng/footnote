"""CI parity test - Python and bash validators must agree on every record.

The hand-crafted corpus at ``parity_corpus.jsonl`` covers happy path,
required-field misses, source/type/gate enum violations, conditional gate
invariant, mission_complete status enum, and the 64KB data size cap.

If the test fails because Python and bash give different verdicts on the
same record, the diagnostic names which side accepted vs rejected and the
failure messages each produced. Fixing the test means re-aligning whichever
validator drifted; do not paper over a real disagreement.
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
