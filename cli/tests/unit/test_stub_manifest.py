"""Tests for the stub-manifest module (G3, x-24b7).

Covers the schema (validate/write/load) and the path convention. The
merge-hold lookup moved into the authorized-merge owner
(crates/fno-agents/src/merge_gates.rs); the reconcile verdict stays here.
"""
from __future__ import annotations

import json

import pytest

from fno import stub_manifest as sm


def _graph(tmp_path, entries):
    """Rows the way the store writes them: the typed api drops a row the
    model cannot parse, so seeds carry the stamped fields."""
    complete = []
    for e in entries:
        row = {"type": "feature", "priority": "p2", "status": "idea", **e}
        row.setdefault("title", e.get("id", "node"))
        row.setdefault("slug", e.get("id", "node"))
        complete.append(row)
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": complete}), encoding="utf-8")
    return p


# ---- schema ----

def test_validate_accepts_zero_stubs():
    sm.validate({"node": "x-1", "stubs": []})


def test_validate_rejects_missing_node():
    with pytest.raises(sm.StubManifestError):
        sm.validate({"stubs": []})


def test_validate_rejects_non_list_stubs():
    with pytest.raises(sm.StubManifestError):
        sm.validate({"node": "x-1", "stubs": {}})


def test_validate_rejects_stub_missing_locators():
    with pytest.raises(sm.StubManifestError):
        sm.validate({"node": "x-1", "stubs": [{"stub_id": "a", "file": ""}]})


def test_validate_rejects_explicit_null_stub_id():
    # str(None) == "None" must not sneak past the required-field check (gemini).
    with pytest.raises(sm.StubManifestError):
        sm.validate({"node": "x-1", "stubs": [{"stub_id": None, "file": "f", "kind": "fn"}]})


def test_write_then_load_roundtrip(tmp_path):
    stubs = [{"stub_id": "create", "file": "api.ts", "symbol": "createUser",
              "contract_ref": "d.md#ic", "kind": "function"}]
    path = sm.write("x-7", stubs, tmp_path, contract_version=2, contract_ref="d.md#ic")
    assert path == sm.manifest_path("x-7", tmp_path)
    loaded = sm.load(path)
    assert loaded["node"] == "x-7"
    assert loaded["contract_version"] == 2
    assert loaded["reconciled"] is False
    assert loaded["stubs"][0]["symbol"] == "createUser"


def test_load_rejects_malformed_json(tmp_path):
    p = tmp_path / ".fno" / "stub-manifest-x.json"
    p.parent.mkdir(parents=True)
    p.write_text("{not json", encoding="utf-8")
    with pytest.raises(sm.StubManifestError):
        sm.load(p)


# ---- merge-hold lookup ----

def test_verdict_manifest_missing(tmp_path):
    # AC5-FR: no manifest -> refuse (do not finalize a half-real PR).
    v = sm.reconcile_verdict("x-1", tmp_path)
    assert v["outcome"] == sm.MANIFEST_MISSING


def test_verdict_malformed_manifest_is_missing(tmp_path):
    # AC5-FR: a partial/malformed manifest refuses, never crashes.
    p = sm.manifest_path("x-1", tmp_path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("{bad", encoding="utf-8")
    assert sm.reconcile_verdict("x-1", tmp_path)["outcome"] == sm.MANIFEST_MISSING


def test_verdict_already_reconciled_is_noop(tmp_path):
    sm.write("x-1", [], tmp_path, contract_test="true", reconciled=True)
    assert sm.reconcile_verdict("x-1", tmp_path)["outcome"] == sm.ALREADY_RECONCILED


def test_verdict_no_contract_test_is_drift(tmp_path):
    # Locked Decision 5: a missing executable gate REFUSES (never guess).
    sm.write("x-1", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path)
    assert sm.reconcile_verdict("x-1", tmp_path)["outcome"] == sm.DRIFT


def test_verdict_passing_suite_authorizes(tmp_path):
    sm.write("x-1", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_test="true")
    assert sm.reconcile_verdict("x-1", tmp_path)["outcome"] == sm.AUTHORIZE


def test_verdict_failing_suite_is_drift(tmp_path):
    # AC4-ERR: the landed schema fails the contract test -> refuse auto-de-stub.
    sm.write("x-1", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_test="false")
    assert sm.reconcile_verdict("x-1", tmp_path)["outcome"] == sm.DRIFT


def test_verdict_no_run_skips_execution(tmp_path):
    # --no-run reports presence-only: a suite that WOULD fail still authorizes
    # because it is never executed.
    sm.write("x-1", [], tmp_path, contract_test="false")
    assert sm.reconcile_verdict("x-1", tmp_path, run_suite=False)["outcome"] == sm.AUTHORIZE


# ---- G4: mark_reconciled (de-stub finalize) ----

def test_mark_reconciled_flips_flag_and_preserves_fields(tmp_path):
    sm.write("x-1", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_version=3, contract_ref="d.md#ic", contract_test="pytest -q")
    sm.mark_reconciled("x-1", tmp_path)
    loaded = sm.load(sm.manifest_path("x-1", tmp_path))
    assert loaded["reconciled"] is True
    assert loaded["contract_version"] == 3
    assert loaded["contract_test"] == "pytest -q"
    assert loaded["stubs"][0]["stub_id"] == "a"


def test_mark_reconciled_missing_raises(tmp_path):
    with pytest.raises(FileNotFoundError):
        sm.mark_reconciled("x-nope", tmp_path)


def test_verdict_tolerates_non_utf8_suite_output(tmp_path):
    # gemini HIGH: a contract-test that emits non-UTF-8 bytes must not crash the
    # gate with UnicodeDecodeError (errors="replace"). Exit 0 -> authorize.
    sm.write("x-1", [], tmp_path, contract_test=r"printf '\377'")
    assert sm.reconcile_verdict("x-1", tmp_path)["outcome"] == sm.AUTHORIZE
