#!/usr/bin/env python3
"""Classify one captured UserPromptSubmit hook payload without exposing values."""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path
from typing import Any


REQUIRED_KEYS = frozenset(
    {
        "hook_event_name",
        "session_id",
        "transcript_path",
        "cwd",
        "permission_mode",
        "prompt",
    }
)
PERMISSION_MODES = frozenset(
    {
        "default",
        "acceptEdits",
        "plan",
        "dontAsk",
        "bypassPermissions",
        "yolo",
        "headless",
    }
)
PROVENANCE_KEYS = frozenset({"origin", "origin_type"})
PROVENANCE_TOKENS = ("origin", "provenance", "human")
HUMAN_ORIGINS = frozenset({"human", "user", "typed"})


def _shape(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, dict):
        return "object"
    if isinstance(value, list):
        return "array"
    if isinstance(value, str):
        return "string"
    if isinstance(value, (int, float)):
        return "number"
    return type(value).__name__


def _receipt(payload: dict[str, Any], verdict: str, reason: str) -> dict[str, Any]:
    observed_keys = sorted(payload)
    shape = {key: _shape(payload[key]) for key in observed_keys}
    shape_digest = hashlib.sha256(
        json.dumps(shape, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    return {
        "origin_provenance": verdict,
        "hook_event_name": payload.get("hook_event_name"),
        "permission_mode": payload.get("permission_mode"),
        "observed_keys": observed_keys,
        "payload_shape_sha256": shape_digest,
        "reason": reason,
    }


def classify(payload: dict[str, Any], sentinel: str) -> tuple[dict[str, Any], int]:
    if not isinstance(payload, dict):
        return _receipt({}, "unknown_schema", "payload is not an object"), 2

    unexpected_provenance = [
        key
        for key in payload
        if any(token in key.lower() for token in PROVENANCE_TOKENS)
        and key not in PROVENANCE_KEYS
    ]
    if unexpected_provenance:
        return _receipt(payload, "unknown_schema", "unrecognized provenance-shaped field"), 2

    missing = sorted(REQUIRED_KEYS - payload.keys())
    if missing:
        return _receipt(payload, "unknown_schema", "required field missing"), 2

    if payload.get("hook_event_name") != "UserPromptSubmit":
        return _receipt(payload, "unknown_schema", "unexpected hook event"), 2
    if payload.get("prompt") != sentinel:
        return _receipt(payload, "unknown_schema", "sentinel did not match"), 2
    if not isinstance(payload.get("session_id"), str) or not payload["session_id"].strip():
        return _receipt(payload, "unknown_schema", "session id is empty"), 2
    permission_mode = payload.get("permission_mode")
    if permission_mode not in PERMISSION_MODES:
        return _receipt(payload, "unknown_schema", "unknown permission mode"), 2

    for key in PROVENANCE_KEYS & payload.keys():
        value = payload[key]
        if not isinstance(value, str) or not value.strip():
            return _receipt(payload, "unknown_schema", "invalid provenance value"), 2
        if value.lower() not in HUMAN_ORIGINS and value.lower() not in {
            "agent",
            "mail",
            "task-notification",
            "webhook",
            "nonhuman",
        }:
            return _receipt(payload, "unknown_schema", "unknown provenance value"), 2

    provenance_values = [payload[key].lower() for key in PROVENANCE_KEYS & payload.keys()]
    if provenance_values:
        verdict = "exposed_human" if all(value in HUMAN_ORIGINS for value in provenance_values) else "exposed_nonhuman"
        return _receipt(payload, verdict, "recognized harness provenance field"), 0
    return _receipt(payload, "not_exposed", "recognized payload has no origin discriminator"), 0


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(
            json.dumps(_receipt({}, "unknown_schema", "expected payload path and sentinel")),
            file=sys.stdout,
        )
        return 2
    try:
        payload = json.loads(Path(argv[1]).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        print(json.dumps(_receipt({}, "unknown_schema", "payload could not be read")))
        return 2
    receipt, status = classify(payload, argv[2])
    print(json.dumps(receipt, sort_keys=True))
    return status


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
