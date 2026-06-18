"""Tests for Task 1.2: --emit-schema on Python events module.

Verifies that `python -m fno.events --emit-schema` prints valid JSON
to stdout describing the Branch A envelope shape, is idempotent, and has no
side effects.
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]

PYTHON_SOURCES = [
    "target", "megawalk", "megatron", "abi-loop",
    "hook", "subagent", "migration", "test", "backlog",
]


def _run_emit_schema() -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "fno.events", "--emit-schema"],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
        timeout=30,
    )


# ---------------------------------------------------------------------------
# AC2-HP: --emit-schema prints valid JSON to stdout, exit 0
# ---------------------------------------------------------------------------

def test_emit_schema_exits_zero() -> None:
    """--emit-schema must exit 0."""
    result = _run_emit_schema()
    assert result.returncode == 0, (
        f"--emit-schema exited {result.returncode}\nstdout: {result.stdout[:500]}\nstderr: {result.stderr[:500]}"
    )


def test_emit_schema_stdout_is_valid_json() -> None:
    """--emit-schema stdout must be valid JSON."""
    result = _run_emit_schema()
    assert result.returncode == 0
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        pytest.fail(f"stdout is not valid JSON: {exc}\nstdout: {result.stdout[:500]}")
    assert isinstance(data, dict)


def test_emit_schema_has_envelope_key() -> None:
    """Output must have an 'envelope' key for the Branch A schema."""
    result = _run_emit_schema()
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "envelope" in data, f"missing 'envelope' key in output: {list(data.keys())}"


def test_emit_schema_has_event_types_key() -> None:
    """Output must have an 'event_types' key listing known type names."""
    result = _run_emit_schema()
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "event_types" in data, f"missing 'event_types' key in output: {list(data.keys())}"
    assert isinstance(data["event_types"], list)


def test_emit_schema_envelope_has_required_fields() -> None:
    """Envelope schema must list required fields [ts, type, source, data]."""
    result = _run_emit_schema()
    data = json.loads(result.stdout)
    envelope = data["envelope"]
    required = set(envelope.get("required", []))
    assert {"ts", "type", "source", "data"} == required


def test_emit_schema_envelope_source_enum() -> None:
    """Envelope source must enumerate Python sources."""
    result = _run_emit_schema()
    data = json.loads(result.stdout)
    enum_vals = data["envelope"]["properties"]["source"].get("enum", [])
    for src in PYTHON_SOURCES:
        assert src in enum_vals, f"source enum missing {src!r}"


def test_emit_schema_event_types_non_empty() -> None:
    """event_types list must be non-empty."""
    result = _run_emit_schema()
    data = json.loads(result.stdout)
    assert len(data["event_types"]) > 0


def test_emit_schema_no_stderr_on_success() -> None:
    """--emit-schema must produce no stderr output on success (stdout is clean JSON)."""
    result = _run_emit_schema()
    assert result.returncode == 0
    # stdout must parse cleanly as JSON (no mixed prose)
    json.loads(result.stdout)
    # stderr must be empty on the success path
    assert result.stderr == "", (
        f"--emit-schema wrote unexpected stderr:\n{result.stderr[:500]}"
    )


# AC2-HP: Idempotent
def test_emit_schema_idempotent() -> None:
    """Two consecutive --emit-schema calls must produce identical output."""
    r1 = _run_emit_schema()
    r2 = _run_emit_schema()
    assert r1.stdout == r2.stdout, "emit-schema output is not idempotent"


# AC2-UI: diagnostics to stderr, schema to stdout
def test_emit_schema_stdout_parseable_ignoring_stderr() -> None:
    """stdout must be parseable JSON regardless of stderr content."""
    result = _run_emit_schema()
    json.loads(result.stdout)  # must not raise
