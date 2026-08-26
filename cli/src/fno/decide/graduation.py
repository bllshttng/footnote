"""Structured graduation declarations for durable decisions."""

from __future__ import annotations

import re
import shlex
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable

GRADUATION_KINDS = (
    "enforced",
    "guidance",
    "should-be-enforced-but-i-did-not",
)
_ARTIFACT_RE = re.compile(r"^(file|test|doc|gate|default):\S(?:.*\S)?$")
_FOLLOW_UP_RE = re.compile(r"^node:[a-z][a-z0-9]*-[0-9a-f]+$", re.IGNORECASE)

REGISTERED_GRADUATION_PROBES: tuple[dict[str, Any], ...] = (
    {
        "decision_id": "d-1ca0e711",
        "graduation": {
            "kind": "enforced",
            "artifact": (
                "test:cli/tests/integration/test_graph_cli.py::"
                "test_new_p0_requires_breaking_acknowledgment"
            ),
        },
    },
)


class InvalidGraduationError(ValueError):
    """A decision graduation declaration is missing or malformed."""


def validate_graduation(kind: str | None, reference: str | None) -> dict[str, str]:
    """Validate one local declaration without guessing a future artifact."""
    normalized_kind = (kind or "").strip().lower()
    normalized_ref = (reference or "").strip()
    if normalized_kind not in GRADUATION_KINDS:
        raise InvalidGraduationError(
            "graduation must be enforced, guidance, or "
            "should-be-enforced-but-i-did-not"
        )
    if normalized_kind == "guidance":
        if normalized_ref:
            raise InvalidGraduationError("guidance takes no graduation reference")
        return {"kind": "guidance"}
    if normalized_kind == "enforced":
        if not _ARTIFACT_RE.fullmatch(normalized_ref):
            raise InvalidGraduationError(
                "enforced graduation requires file:, test:, doc:, gate:, or default:"
            )
        return {"kind": "enforced", "artifact": normalized_ref}
    if not _FOLLOW_UP_RE.fullmatch(normalized_ref):
        raise InvalidGraduationError(
            "should-be-enforced-but-i-did-not requires node:<id>"
        )
    return {
        "kind": "should-be-enforced-but-i-did-not",
        "follow_up": normalized_ref.lower(),
    }


def graduation_or_guidance(
    kind: str | None, reference: str | None
) -> dict[str, str]:
    """Default an omitted declaration to honest guidance."""
    if kind is None and reference is None:
        return {"kind": "guidance"}
    return validate_graduation(kind, reference)


def normalize_graduation(value: dict[str, str] | None) -> dict[str, str]:
    """Validate an engine-level declaration or supply the guidance default."""
    if value is None:
        return {"kind": "guidance"}
    if not isinstance(value, dict):
        raise InvalidGraduationError("graduation must be an object")
    kind = value.get("kind")
    reference = value.get("artifact") or value.get("follow_up")
    return validate_graduation(kind, reference)


def registered_retirement(decision_id: str) -> dict[str, str] | None:
    """Return checked-in retirement evidence for one decision id."""
    wanted = decision_id.casefold()
    for row in REGISTERED_GRADUATION_PROBES:
        if str(row["decision_id"]).casefold() != wanted:
            continue
        artifact = str(row["graduation"]["artifact"])
        return {
            "artifact": artifact,
            "marker": f"test_passed:{artifact.removeprefix('test:')}",
        }
    return None


def _default_run(argv: list[str], *, cwd: Path, timeout: int):
    return subprocess.run(
        argv,
        cwd=cwd,
        timeout=timeout,
        capture_output=True,
        text=True,
        check=False,
    )


def _unknown(decision_id: str, marker: str) -> dict[str, str]:
    return {
        "decision_id": decision_id,
        "status": "unknown",
        "graduation_checked": marker,
    }


def _retired(decision_id: str, marker: str) -> dict[str, str]:
    return {
        "decision_id": decision_id,
        "status": "retired",
        "graduation_checked": marker,
        "graduation_retired": decision_id,
    }


def evaluate_graduation(
    row: dict[str, Any],
    *,
    root: Path,
    run: Callable[..., Any] = _default_run,
    timeout: int = 30,
) -> dict[str, str]:
    """Evaluate one declaration with a bounded, target-pinned probe."""
    decision_id = str(row.get("decision_id") or "unknown")
    declaration = row.get("graduation")
    if not isinstance(declaration, dict):
        return {"decision_id": decision_id, "status": "legacy"}
    kind = declaration.get("kind")
    if kind == "guidance":
        return {"decision_id": decision_id, "status": "guidance"}
    if kind == "should-be-enforced-but-i-did-not":
        return {
            "decision_id": decision_id,
            "status": "guidance",
            "follow_up": str(declaration.get("follow_up") or ""),
        }
    try:
        normalized = normalize_graduation(declaration)
    except InvalidGraduationError as exc:
        return _unknown(decision_id, f"declaration_invalid:{exc}")
    artifact = normalized["artifact"]
    prefix, payload = artifact.split(":", 1)

    if prefix in {"file", "doc"}:
        # A path that still exists proves nothing: the enforcement at that line
        # can be deleted, replaced, or shifted while the file survives. So this
        # lane takes the gate lane's contract - name the text that only the
        # declared behavior produces, and read it back at the pinned location.
        target, separator, marker = payload.partition("=>marker:")
        target = target.strip()
        marker = marker.strip()
        if not separator or not target or not marker:
            return _unknown(decision_id, f"artifact_marker_invalid:{artifact}")
        path_text, line_separator, line_text = target.rpartition(":")
        if not line_separator or not line_text.isdigit():
            path_text, line_text = target, ""
        candidate = (root / path_text).resolve()
        try:
            candidate.relative_to(root.resolve())
        except ValueError:
            return _unknown(decision_id, f"artifact_outside_root:{artifact}")
        if not candidate.is_file():
            return _unknown(decision_id, f"artifact_missing:{artifact}")
        try:
            lines = candidate.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeError) as exc:
            return _unknown(decision_id, f"artifact_unreadable:{type(exc).__name__}")
        if line_text:
            index = int(line_text) - 1
            if index < 0 or index >= len(lines):
                return _unknown(decision_id, f"artifact_line_missing:{artifact}")
            found = marker in lines[index]
        else:
            found = any(marker in line for line in lines)
        if not found:
            return _unknown(decision_id, f"marker_missing:{artifact}")
        return _retired(decision_id, f"marker_present:{artifact}")

    if prefix == "gate":
        command, separator, marker = payload.partition("=>marker:")
        if not separator or not command.strip() or not marker.strip():
            return _unknown(decision_id, f"gate_marker_invalid:{artifact}")
        try:
            argv = shlex.split(command)
            completed = run(argv, cwd=root, timeout=timeout)
        except (ValueError, OSError) as exc:
            return _unknown(decision_id, f"probe_error:{type(exc).__name__}")
        except subprocess.TimeoutExpired:
            return _unknown(decision_id, f"probe_timeout:{artifact}")
        output = f"{completed.stdout or ''}\n{completed.stderr or ''}"
        marker = marker.strip()
        if marker not in output:
            return _unknown(decision_id, f"marker_missing:{marker}")
        return _retired(decision_id, f"marker_present:{marker}")

    if prefix == "test":
        try:
            completed = run(
                [sys.executable, "-m", "pytest", "-q", payload],
                cwd=root,
                timeout=timeout,
            )
        except (OSError, ValueError) as exc:
            return _unknown(decision_id, f"probe_error:{type(exc).__name__}")
        except subprocess.TimeoutExpired:
            return _unknown(decision_id, f"probe_timeout:{artifact}")
        if completed.returncode != 0:
            return _unknown(decision_id, f"test_failed:{payload}")
        return _retired(decision_id, f"test_passed:{payload}")

    # A key that merely EXISTS is not the declared default: `False` and any
    # other contradicting value read as present. The declaration carries the
    # required value, and only that value retires the ruling.
    key, separator, expected = payload.partition("=")
    key = key.strip()
    expected = expected.strip()
    if not separator or not key or not expected:
        return _unknown(decision_id, f"default_expectation_missing:{payload}")
    try:
        completed = run(["fno", "config", "get", key], cwd=root, timeout=timeout)
    except (OSError, ValueError) as exc:
        return _unknown(decision_id, f"probe_error:{type(exc).__name__}")
    except subprocess.TimeoutExpired:
        return _unknown(decision_id, f"probe_timeout:{artifact}")
    value = str(completed.stdout or "").strip()
    if completed.returncode != 0 or not value:
        return _unknown(decision_id, f"default_unknown:{key}")
    if value != expected:
        return _unknown(decision_id, f"default_mismatch:{key}={value}")
    return _retired(decision_id, f"default_present:{key}={value}")


__all__ = [
    "GRADUATION_KINDS",
    "InvalidGraduationError",
    "REGISTERED_GRADUATION_PROBES",
    "graduation_or_guidance",
    "evaluate_graduation",
    "normalize_graduation",
    "registered_retirement",
    "validate_graduation",
]
