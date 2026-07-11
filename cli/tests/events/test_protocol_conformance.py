"""Conformance test for the a2a status-breakpoint golden fixtures (x-dbaf, US5).

The fixtures under schemas/fixtures/events-protocol-v1/ are the static artifacts
third-party adapter authors build against. This test gives them teeth: every
valid/ fixture must pass the canonical validator, every invalid/ fixture must
fail it (proving the additionalProperties:false + outcome-enum rules bite), and
the duplicate-emission fixture demonstrates the (run, task, type) dedup key.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.events import ValidationError, validate

REPO_ROOT = Path(__file__).resolve().parents[3]
FIXTURES = REPO_ROOT / "schemas" / "fixtures" / "events-protocol-v1"
VALID = sorted((FIXTURES / "valid").glob("*.json"))
INVALID = sorted((FIXTURES / "invalid").glob("*.json"))


SPEC = REPO_ROOT / "docs" / "protocol" / "a2a-status-events.md"


def test_spec_exists_and_covers_the_contract() -> None:
    assert SPEC.is_file(), f"missing spec: {SPEC}"
    text = SPEC.read_text(encoding="utf-8")
    # spec must name every kind, the schema artifact, and the dedup key so it
    # cannot silently drift from the fixtures / validator.
    for kind in ("task_started", "task_done", "blocked", "run_summary"):
        assert kind in text, f"spec omits {kind}"
    assert "events-protocol-v1.json" in text
    assert "(run, task, type)" in text
    assert "CloudEvents" in text


def test_fixtures_present() -> None:
    # one per kind + the edge cases the spec names
    names = {p.name for p in VALID}
    assert {"task_started.json", "task_done.json", "blocked.json", "run_summary.json"} <= names
    assert {"no_session_producer.json", "missing_task_id.json"} <= names
    assert INVALID, "expected at least one negative fixture"


@pytest.mark.parametrize("path", VALID, ids=lambda p: p.name)
def test_valid_fixture_conforms(path: Path) -> None:
    validate(json.loads(path.read_text(encoding="utf-8")))


@pytest.mark.parametrize("path", INVALID, ids=lambda p: p.name)
def test_invalid_fixture_rejected(path: Path) -> None:
    with pytest.raises(ValidationError):
        validate(json.loads(path.read_text(encoding="utf-8")))


def test_no_session_fixture_omits_identity() -> None:
    ev = json.loads((FIXTURES / "valid" / "no_session_producer.json").read_text())
    assert "from" not in ev and "model" not in ev  # omitted, not empty


# -- AC3-FR: duplicate emission collapses under the (run, task, type) dedup key --

def test_duplicate_emission_dedup_key() -> None:
    lines = [
        json.loads(ln)
        for ln in (FIXTURES / "valid" / "duplicate_emission.jsonl").read_text().splitlines()
        if ln.strip()
    ]
    assert len(lines) == 2
    for ev in lines:
        validate(ev)  # both are individually valid
    keys = {(ev["run"], ev.get("task"), ev["type"]) for ev in lines}
    assert len(keys) == 1  # a conforming consumer collapses them to one


# -- no leaked internal ids in the public fixtures --

def test_fixtures_use_placeholder_ids_only() -> None:
    import re

    # This repo's internal ids use the x-/ab- prefixes (e.g. x-dbaf, ab-ca822421);
    # public examples must use the prj- placeholder instead.
    leak = re.compile(r"\b(?:x|ab)-[0-9a-f]{4,}\b")
    for path in VALID + INVALID:
        text = path.read_text(encoding="utf-8")
        assert not leak.search(text), f"possible internal id leak in {path.name}"
