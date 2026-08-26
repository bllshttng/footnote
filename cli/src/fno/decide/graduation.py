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

REGISTERED_GRADUATION_PROBES = (
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
        path_text, separator, line_text = payload.rpartition(":")
        if not separator or not line_text.isdigit():
            path_text, line_text = payload, ""
        candidate = (root / path_text).resolve()
        try:
            candidate.relative_to(root.resolve())
        except ValueError:
            return _unknown(decision_id, f"artifact_outside_root:{artifact}")
        if not candidate.is_file():
            return _unknown(decision_id, f"artifact_missing:{artifact}")
        if line_text:
            try:
                line_count = sum(1 for _ in candidate.open(encoding="utf-8"))
            except (OSError, UnicodeError) as exc:
                return _unknown(decision_id, f"artifact_unreadable:{type(exc).__name__}")
            if int(line_text) > line_count:
                return _unknown(decision_id, f"artifact_line_missing:{artifact}")
        return _retired(decision_id, f"artifact_present:{artifact}")

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

    try:
        completed = run(["fno", "config", "get", payload], cwd=root, timeout=timeout)
    except (OSError, ValueError) as exc:
        return _unknown(decision_id, f"probe_error:{type(exc).__name__}")
    except subprocess.TimeoutExpired:
        return _unknown(decision_id, f"probe_timeout:{artifact}")
    value = str(completed.stdout or "").strip()
    if completed.returncode != 0 or not value:
        return _unknown(decision_id, f"default_unknown:{payload}")
    return _retired(decision_id, f"default_present:{payload}={value}")


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
