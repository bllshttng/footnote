#!/usr/bin/env python3
"""Closure probe core for the agent field population contract.

Reads the `fno-py doctor lint field-coverage --live --json` report from
stdin (or a path argument) and prints the positive marker only when every
contract field is either populated or measured under its declared
population mode, and absent from both dead-field lists. Fails closed on
malformed JSON, an unmeasured registry, contract errors, a schema entry
that is missing or drifted, or any missing classification. A conditional
or transient contract is never proof the instrument ran. Dead fields
outside the contract are the evaluator's own finding; they do not block
this marker.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

MARKER = "agent-field-population-contract: verified"
ANCHORS = ("name", "created_at", "harness", "status")
# field -> the mode/surface the probe itself promises; the schema must agree
FIELDS = {
    "forked_from_session_id": ("conditional", "persisted_and_projected"),
    "predecessor_session_ids": ("conditional", "persisted_and_projected"),
    "live_status": ("conditional", "projected"),
    "live_status_basis": ("conditional", "projected"),
    "delivery_policy": ("transient", "persisted_and_projected"),
}
READINGS = ("persisted", "projected")


def _fail(reason: str) -> int:
    print(f"agent-field-population-contract: FAIL: {reason}", file=sys.stderr)
    return 1


def _covered(surface: str, reading: str) -> bool:
    return surface == "persisted_and_projected" or surface == reading


def _schema_expectations(repo_root: Path) -> dict[str, tuple[str, str]] | None:
    """Read mode/surface per contract field from the schema.

    Returns None when the schema or any entry is missing or malformed, so
    the probe fails closed instead of trusting a partial contract.
    """
    schema_path = repo_root / "schemas" / "agents-list-row.json"
    try:
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    block = schema.get("population_contract") if isinstance(schema, dict) else None
    if not isinstance(block, dict):
        return None
    expectations: dict[str, tuple[str, str]] = {}
    for field, declared in FIELDS.items():
        entry = block.get(field)
        if not isinstance(entry, dict):
            return None
        mode = entry.get("mode")
        surface = entry.get("surface")
        if not isinstance(mode, str) or not isinstance(surface, str):
            return None
        if (mode, surface) != declared:
            return None
        expectations[field] = (mode, surface)
    return expectations


def check(
    payload: object, expectations: dict[str, tuple[str, str]] | None
) -> int:
    if not isinstance(payload, dict) or payload.get("status") == "unmeasured":
        return _fail("report is UNMEASURED; the conditional contract is not proof the instrument ran")
    anchors = payload.get("anchors")
    if not isinstance(anchors, dict):
        return _fail("report has no anchors block")
    for name in ANCHORS:
        reading = anchors.get(name)
        if not isinstance(reading, dict):
            return _fail(f"anchor {name} missing")
        if not reading.get("total") or reading.get("set") != reading.get("total"):
            return _fail(f"anchor {name} is not fully set")
    errors = payload.get("contract_errors")
    if errors:
        return _fail(f"contract errors: {errors}")
    for reading in READINGS:
        section = payload.get(reading)
        if not isinstance(section, dict):
            return _fail(f"report has no {reading} reading")
    if not expectations:
        return _fail("schema population_contract is missing, malformed, or drifted")
    for field, declared in FIELDS.items():
        mode, surface = expectations[field]
        for reading in READINGS:
            if not _covered(surface, reading):
                continue
            section = payload[reading]
            if field in section.get("dead_fields", []):
                return _fail(f"{field} is in the {reading} dead_fields")
            if section.get("counts", {}).get(field):
                continue  # populated: the outcome the writers exist for
            reports = {
                **section.get("conditional_zero", {}),
                **section.get("transient_zero", {}),
            }
            report = reports.get(field)
            if not isinstance(report, dict):
                return _fail(f"{field}: not classified in the {reading} reading")
            if report.get("mode") != mode:
                fail_msg = (
                    f"{field}: classified {report.get('mode')!r}, "
                    f"schema declares {mode!r}"
                )
                return _fail(fail_msg)
            if not report.get("writer") or not report.get("test"):
                return _fail(f"{field}: classification lacks writer or test evidence")
    print(MARKER)
    return 0


def main(argv: list[str]) -> int:
    if len(argv) > 1:
        try:
            data = Path(argv[1]).read_bytes()
        except OSError as exc:
            return _fail(f"unreadable report: {exc}")
    else:
        data = sys.stdin.buffer.read()
    try:
        payload = json.loads(data)
    except json.JSONDecodeError as exc:
        return _fail(f"malformed JSON: {exc}")
    repo_root = Path(__file__).resolve().parents[2]
    return check(payload, _schema_expectations(repo_root))


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
