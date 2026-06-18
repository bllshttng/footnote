"""Tests for the Python ``verify_child_promise`` helper at
``fno.events.verify_child_promise``.

The helper is the in-package Python parallel to the canonical
``fno-agents verify-evidence child-promise`` verb (folded out of the
deleted ``scripts/lib/verify-event-evidence.sh`` in US1, ab-58645f63).
Both share fixture events.jsonl files at
``cli/tests/fixtures/verify_child_promise/{ok,missing,nonce_mismatch,multiline}``
and produce diagnostic vocabulary that overlaps so a megawalk/CLI consumer
written in either language can interpret the other's output.

Diagnostic-vocabulary symmetry is enforced by ``test_diagnostic_symmetry``
which runs both implementations against each fixture and checks that the
Rust verb's exit code + stderr substring map to the same Python error key.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.agents.rust_runtime import resolve_binary
from fno.events.verify_child_promise import verify_child_promise


REPO_ROOT = Path(__file__).resolve().parents[3]
FIXTURES = REPO_ROOT / "cli" / "tests" / "fixtures" / "verify_child_promise"


def _fixture(name: str) -> Path:
    return FIXTURES / name / "events.jsonl"


# -- Python helper: happy path --


def test_ok_returns_true_none() -> None:
    ok, err = verify_child_promise("sX", "nX", _fixture("ok"))
    assert ok is True
    assert err is None


def test_multiline_returns_true_for_matching_session() -> None:
    """Multiple child_promise lines, helper picks the matching session_id."""
    ok, err = verify_child_promise("sX", "nX", _fixture("multiline"))
    assert ok is True
    assert err is None


# -- Python helper: error paths --


def test_missing_returns_child_promise_missing() -> None:
    ok, err = verify_child_promise("sX", "nX", _fixture("missing"))
    assert ok is False
    assert err == "child_promise_missing"


def test_nonce_mismatch_returns_child_promise_nonce_mismatch() -> None:
    ok, err = verify_child_promise("sX", "nX", _fixture("nonce_mismatch"))
    assert ok is False
    assert err == "child_promise_nonce_mismatch"


def test_nonexistent_path_returns_events_unreadable(tmp_path: Path) -> None:
    """File does not exist."""
    ok, err = verify_child_promise("sX", "nX", tmp_path / "does-not-exist.jsonl")
    assert ok is False
    assert err == "events_unreadable"


def test_directory_path_returns_events_unreadable(tmp_path: Path) -> None:
    """Path is a directory, not a file."""
    ok, err = verify_child_promise("sX", "nX", tmp_path)
    assert ok is False
    assert err == "events_unreadable"


def test_truncated_last_line_returns_missing(tmp_path: Path) -> None:
    """A truncated last line should be skipped, not raise; if no other line
    matches, the helper reports child_promise_missing."""
    p = tmp_path / "events.jsonl"
    p.write_text(
        '{"ts":"2026-05-07T09:30:42Z","type":"phase_init","source":"target",'
        '"data":{"phase":"register","nonce":"a","session_id":"sX"}}\n'
        '{"ts":"2026-05-07T09:30:43Z","type":"child_pr',  # truncated
        encoding="utf-8",
    )
    ok, err = verify_child_promise("sX", "nX", p)
    assert ok is False
    assert err == "child_promise_missing"


def test_legacy_envelope_with_timestamp_field(tmp_path: Path) -> None:
    """Legacy {timestamp,...} envelope shape is still accepted (matches bash
    helper's tolerance during the events-schema rollout window)."""
    p = tmp_path / "events.jsonl"
    p.write_text(
        '{"timestamp":"2026-05-07T09:30:42Z","type":"child_promise","source":"target",'
        '"data":{"session_id":"sLegacy","nonce":"nLegacy"}}\n',
        encoding="utf-8",
    )
    ok, err = verify_child_promise("sLegacy", "nLegacy", p)
    assert ok is True
    assert err is None


# -- Diagnostic-vocabulary symmetry with the Rust verb --


def _run_rust_verify(
    binary: Path, session_id: str, nonce: str, events_path: Path
) -> tuple[int, str]:
    """Run ``fno-agents verify-evidence child-promise`` and capture (rc, stderr)."""
    result = subprocess.run(
        [
            str(binary),
            "verify-evidence",
            "child-promise",
            session_id,
            nonce,
            str(events_path),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode, result.stderr


SYMMETRY_CASES = [
    # (label, session_id, nonce, fixture_subdir, expected_python_err, rust_rc, rust_stderr_substring)
    ("ok", "sX", "nX", "ok", None, 0, ""),
    ("missing", "sX", "nX", "missing", "child_promise_missing", 1, "child_promise missing for session sX"),
    ("mismatch", "sX", "nX", "nonce_mismatch", "child_promise_nonce_mismatch", 1, "nonce mismatch"),
    ("multiline", "sX", "nX", "multiline", None, 0, ""),
    # An event with data.nonce == "" matches the producer's loud-fail
    # path (provenance_nonce missing in target-state.md). Both impls
    # MUST classify this as a nonce mismatch, not a missing event - the
    # event was found but the nonce comparison fails. Pins the symmetry
    # so a future refactor cannot regress to collapsing an empty-nonce
    # event into the missing-event diagnostic.
    ("empty_nonce", "sX", "nX", "empty_nonce", "child_promise_nonce_mismatch", 1, "nonce mismatch"),
]


@pytest.mark.skipif(resolve_binary() is None, reason="fno-agents binary unavailable")
@pytest.mark.parametrize(
    "label,session_id,nonce,subdir,expected_py_err,rust_rc,rust_substr",
    SYMMETRY_CASES,
    ids=[c[0] for c in SYMMETRY_CASES],
)
def test_diagnostic_symmetry(
    label: str,
    session_id: str,
    nonce: str,
    subdir: str,
    expected_py_err: str | None,
    rust_rc: int,
    rust_substr: str,
) -> None:
    """For each fixture: Rust verb rc + stderr substring matches Python (ok, err) tuple.

    Vocabulary mapping:
        Python error key                 ↔ Rust verb (rc, stderr substring)
        ─────────────────────────────────────────────────────────────
        child_promise_missing            ↔ rc=1, "child_promise missing for session"
        child_promise_nonce_mismatch     ↔ rc=1, "nonce mismatch"
        events_unreadable                ↔ rc=2, "unreadable"  (covered by separate test)
    """
    binary = resolve_binary()
    assert binary is not None  # guarded by skipif
    events_path = _fixture(subdir)
    py_ok, py_err = verify_child_promise(session_id, nonce, events_path)
    rust_actual_rc, rust_stderr = _run_rust_verify(binary, session_id, nonce, events_path)

    expected_py_ok = expected_py_err is None
    assert py_ok is expected_py_ok, f"{label}: python ok={py_ok}, expected {expected_py_ok}"
    assert py_err == expected_py_err, f"{label}: python err={py_err!r}, expected {expected_py_err!r}"
    assert rust_actual_rc == rust_rc, f"{label}: rust rc={rust_actual_rc}, expected {rust_rc}"
    if rust_substr:
        assert rust_substr in rust_stderr, (
            f"{label}: rust stderr missing substring {rust_substr!r}; got {rust_stderr!r}"
        )


@pytest.mark.skipif(resolve_binary() is None, reason="fno-agents binary unavailable")
def test_diagnostic_symmetry_unreadable(tmp_path: Path) -> None:
    """Both impls report substrate failure on unreadable events files."""
    binary = resolve_binary()
    assert binary is not None  # guarded by skipif
    missing = tmp_path / "no-such-file.jsonl"
    py_ok, py_err = verify_child_promise("sX", "nX", missing)
    rust_rc, rust_stderr = _run_rust_verify(binary, "sX", "nX", missing)

    assert py_ok is False
    assert py_err == "events_unreadable"
    assert rust_rc == 2
    assert "unreadable" in rust_stderr
