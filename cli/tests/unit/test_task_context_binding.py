"""Task-context binding gates (x-59b0).

Pins the Python doors around the native verdicts: the init gate refuses a
declared required binding that fails native revalidation (before any claim),
the resume receipt carries a binding compatibly (legacy = explicitly unbound,
legacy digests unchanged), and corruption refuses by name. The verdict logic
itself is the Rust module's; the journey test drives the binary end to end.
"""

import hashlib
import json
from pathlib import Path

import pytest

from fno.rust_binary import find_dev_binary
from fno.target_context_gate import TaskContextGateRefused, gate_declared_task_context

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


def _source_bytes(body: str) -> dict:
    return {
        "path": "PLAN.md",
        "content_revision": "b48ba4b8c",
        "content_digest": hashlib.sha256(body.encode()).hexdigest(),
        "byte_size": len(body.encode()),
    }


def _binding_request(tmp_path: Path, body: str = "plan bytes\n") -> dict:
    src = tmp_path / "PLAN.md"
    src.write_text(body, encoding="utf-8")
    return {
        "version": 1,
        "node": "x-59b0",
        "attempt": "20260912T055500Z-test99-abc123",
        "harness": "claude",
        "session": "sess-worker",
        "worktree": str(tmp_path),
        "plan_path": "PLAN.md",
        "plan_digest": hashlib.sha256(body.encode()).hexdigest(),
        "required_constraints": ["Do not widen scope beyond the plan"],
        "required_sources": [_source_bytes(body)],
        "source_bytes": len(body.encode()),
        "payload_bytes": 512,
        "stage": "prepared",
    }


def _write_bound_binding(tmp_path: Path, binding_file: Path) -> dict:
    from fno.rust_binary import verb_call

    request = _binding_request(tmp_path)
    answer = verb_call("task-context-prepare", {"binding": request})
    assert answer["ok"] is True, answer
    bound = dict(request)
    bound["binding_digest"] = answer["binding_digest"]
    binding_file.write_text(json.dumps(bound), encoding="utf-8")
    return bound


# ── the init gate ──────────────────────────────────────────────────────────


def test_no_declared_binding_is_no_gate(tmp_path):
    # Ordinary behavior: an undeclared binding never gates init.
    assert gate_declared_task_context("x-59b0", str(tmp_path), env={}) is None


def test_declared_but_unreadable_binding_refuses_by_name(tmp_path):
    missing = tmp_path / "gone.json"
    with pytest.raises(TaskContextGateRefused) as exc:
        gate_declared_task_context(
            "x-59b0", str(tmp_path), env={"FNO_TASK_CONTEXT_FILE": str(missing)}
        )
    assert exc.value.reason == "context_binding_unreadable"


@requires_rust
def test_gate_ok_when_required_sources_unchanged(tmp_path):
    binding_file = tmp_path / "task-context-x-59b0.json"
    _write_bound_binding(tmp_path, binding_file)
    answer = gate_declared_task_context(
        "x-59b0", str(tmp_path), env={"FNO_TASK_CONTEXT_FILE": str(binding_file)}
    )
    assert answer is not None and answer["ok"] is True
    assert answer["checked_sources"] == 1


@requires_rust
def test_gate_refuses_when_required_source_changed(tmp_path):
    binding_file = tmp_path / "task-context-x-59b0.json"
    _write_bound_binding(tmp_path, binding_file)
    (tmp_path / "PLAN.md").write_text("CHANGED bytes\n", encoding="utf-8")
    with pytest.raises(TaskContextGateRefused) as exc:
        gate_declared_task_context(
            "x-59b0", str(tmp_path), env={"FNO_TASK_CONTEXT_FILE": str(binding_file)}
        )
    assert exc.value.reason == "context_stale_source"


@requires_rust
def test_gate_refuses_wrong_node(tmp_path):
    binding_file = tmp_path / "task-context-x-59b0.json"
    _write_bound_binding(tmp_path, binding_file)
    with pytest.raises(TaskContextGateRefused) as exc:
        gate_declared_task_context(
            "x-other", str(tmp_path), env={"FNO_TASK_CONTEXT_FILE": str(binding_file)}
        )
    assert exc.value.reason == "context_wrong_node"


@requires_rust
def test_gate_ignores_implementation_head_moves(tmp_path):
    # AC3-EDGE: an unrelated implementation change (a code file) must not stale
    # unchanged required sources - only declared sources are checked.
    binding_file = tmp_path / "task-context-x-59b0.json"
    _write_bound_binding(tmp_path, binding_file)
    code = tmp_path / "impl.py"
    code.write_text("def moved(): ...\n", encoding="utf-8")
    answer = gate_declared_task_context(
        "x-59b0", str(tmp_path), env={"FNO_TASK_CONTEXT_FILE": str(binding_file)}
    )
    assert answer is not None and answer["ok"] is True
    assert code.exists()


# ── receipt transport compatibility ────────────────────────────────────────


def _minimal_receipt(**kwargs):
    from fno.resume.receipt import build_receipt

    defaults = dict(
        node="x-59b0",
        session="sess-worker",
        phase="do",
        generation=1,
        repo="footnote",
        worktree="/wt/x-59b0",
        branch="feature/x-59b0",
        head="b48ba4b8cfff",
        next_verb="/fno:target",
        next_target="x-59b0",
        written_at="2026-09-12T00:00:00Z",
    )
    defaults.update(kwargs)
    return build_receipt(**defaults)


def test_legacy_receipt_is_explicitly_unbound():
    from fno.resume.receipt import _receipt_from_dict

    receipt = _minimal_receipt()
    loaded = _receipt_from_dict(receipt.to_dict())
    assert loaded.task_context is None
    assert loaded.content_sha == receipt.content_sha


def test_legacy_content_sha_survives_the_optional_field():
    # A v1 receipt written before task_context existed must still validate:
    # the canonical payload ignores the absent (None) optional field.
    from fno.resume.receipt import _canonical_payload

    receipt = _minimal_receipt()
    legacy_dict = receipt.to_dict()
    legacy_dict.pop("task_context")
    legacy_dict.pop("content_sha")
    legacy_payload = json.dumps(legacy_dict, sort_keys=True, separators=(",", ":"))
    assert hashlib.sha256(legacy_payload.encode()).hexdigest() == receipt.content_sha
    assert _canonical_payload(receipt.to_dict()) == legacy_payload


def test_bound_receipt_round_trips():
    from fno.resume.receipt import _receipt_from_dict

    binding = {
        "node": "x-59b0",
        "attempt": "attempt-1",
        "binding_digest": "d" * 64,
        "stage": "prepared",
    }
    receipt = _minimal_receipt(task_context=binding)
    loaded = _receipt_from_dict(receipt.to_dict())
    assert loaded.task_context == binding
    assert loaded.content_sha == receipt.content_sha


def test_non_object_task_context_is_malformed():
    from fno.resume.receipt import MalformedReceiptError, _receipt_from_dict

    receipt = _minimal_receipt()
    d = receipt.to_dict()
    d["task_context"] = "not an object"
    with pytest.raises(MalformedReceiptError):
        _receipt_from_dict(d)
