"""Tests for the stub-manifest module (G3, x-24b7).

Covers the schema (validate/write/load), the path convention, and the
merge-hold lookup (`unreconciled_manifest_for_pr`) that backs the
`fno pr merge` draft-held guard. The graph is a tmp JSON passed explicitly so
no global state is touched.
"""
from __future__ import annotations

import json

import pytest

from fno import stub_manifest as sm


def _graph(tmp_path, entries):
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}), encoding="utf-8")
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

def test_contract_node_with_unreconciled_manifest_is_held(tmp_path):
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42, "dep": "contract"}])
    sm.write("x-9", [{"stub_id": "a", "file": "f.ts", "kind": "function"}], tmp_path)
    held = sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp)
    assert held is not None
    assert held["_node"] == "x-9"


def test_reconciled_manifest_is_not_held(tmp_path):
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42, "dep": "contract"}])
    sm.write("x-9", [], tmp_path, reconciled=True)
    assert sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp) is None


def test_hard_node_is_never_held_even_with_a_manifest_file(tmp_path):
    # AC6-EDGE: the default path is unchanged. A hard node has no `dep`; even a
    # stray manifest file must not hold its merge.
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42}])
    sm.write("x-9", [{"stub_id": "a", "file": "f.ts", "kind": "function"}], tmp_path)
    assert sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp) is None


def test_contract_node_without_manifest_is_not_held(tmp_path):
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42, "dep": "contract"}])
    assert sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp) is None


def test_unknown_pr_is_not_held(tmp_path):
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42, "dep": "contract"}])
    assert sm.unreconciled_manifest_for_pr(999, tmp_path, graph_path=gp) is None


def test_malformed_manifest_holds_conservatively(tmp_path):
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42, "dep": "contract"}])
    p = sm.manifest_path("x-9", tmp_path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("{bad", encoding="utf-8")
    held = sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp)
    assert held is not None and held.get("_malformed") is True


def test_unreadable_manifest_holds_not_bypasses(tmp_path):
    # gemini high: an unreadable (bad-encoding) manifest must HOLD, not raise an
    # exception that the merge guard swallows to allow the merge.
    gp = _graph(tmp_path, [{"id": "x-9", "pr_number": 42, "dep": "contract"}])
    p = sm.manifest_path("x-9", tmp_path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(b"\xff\xfe not utf-8")
    held = sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp)
    assert held is not None and held.get("_malformed") is True


def test_pr_recorded_in_additional_prs_is_found(tmp_path):
    # codex P2: a contract dependent whose PR sits in additional_prs must still
    # be matched (ints and /pull/<n> URLs).
    gp = _graph(tmp_path, [{
        "id": "x-9", "pr_number": 7, "dep": "contract",
        "additional_prs": [42, "https://github.com/o/r/pull/99"],
    }])
    sm.write("x-9", [{"stub_id": "a", "file": "f.ts", "kind": "function"}], tmp_path)
    assert sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp) is not None
    assert sm.unreconciled_manifest_for_pr(99, tmp_path, graph_path=gp) is not None
