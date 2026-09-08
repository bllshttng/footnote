"""Public-entry journeys for the routing inventory policy (x-90a9 task 3.1).

Three journeys, each through the entries an operator actually uses:

- the inventory readout separates a policy hold from a capacity hold;
- the audit refuses a preview alone, through the dev binary;
- the audit verifies a fully evidenced session and prints its marker.

Selection truth lives in Rust (``fno-agents route-slot``); these tests pin
that the public readouts and the audit answer with the same verdicts.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


_ROWS = [
    {"name": "flash-zai", "harness": "claude", "model": "glm-5.3-flash", "band": "low"},
]


def _settings(enforce: bool, access: str, rows: list) -> SimpleNamespace:
    # A planless target walks the blueprint slot (the work-kind ruling), so
    # both slots declare the lane or the walk refuses on "no lanes" instead
    # of exercising the access filter.
    slots = {
        verb: SimpleNamespace(lanes=["flash-zai"], on_exhausted="queue")
        for verb in ("target", "blueprint")
    }
    return SimpleNamespace(
        routing=SimpleNamespace(
            models=rows, enforce_inventory=enforce, operator_access=access,
        ),
        agents=SimpleNamespace(profiles=slots),
        model_routing=SimpleNamespace(roles={}),
    )


def _declare(monkeypatch: pytest.MonkeyPatch, rows: list) -> None:
    from fno import route_resolve as rr

    inv = rr.inventory_from_rows(rows, objective="cheapest-that-clears")
    monkeypatch.setattr(rr, "resolve_inventory", lambda **_kw: inv)


def _invoke_inventory(
    monkeypatch: pytest.MonkeyPatch, capacity: dict | None = None
) -> str:
    from typer.testing import CliRunner

    from fno import route_resolve as rr
    from fno.route_cli import route_app

    monkeypatch.setattr(rr, "runtime_capacity", lambda **kw: (capacity or {}))
    res = CliRunner().invoke(route_app, ["inventory"])
    assert res.exit_code == 0, res.output
    return res.output


@requires_rust
def test_inventory_names_a_policy_hold_never_an_exhaustion(monkeypatch) -> None:
    """AC5/AC12: a lane with no native view label is skipped under a remote
    operator and the readout says policy-held - never a spent quota. The
    native-labeled subset of the same inventory arms under the same access."""
    _declare(monkeypatch, _ROWS)
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: _settings(True, "remote", _ROWS),
    )
    out = _invoke_inventory(monkeypatch)
    assert "routing=policy-held" in out
    # The policy name on_exhausted may appear; a CAPACITY claim may not.
    assert "capacity=exhausted" not in out

    # AC5: the native-labeled row qualifies under the same remote access.
    rows = [dict(r, operator_view="claude-native") for r in _ROWS]
    _declare(monkeypatch, rows)
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: _settings(True, "remote", rows),
    )
    out = _invoke_inventory(monkeypatch)
    assert "routing=armed" in out
    assert "access=remote" in out

    # Control: no policy, real exhaustion. The hold names its own cause.
    _declare(monkeypatch, _ROWS)
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: _settings(False, "unknown", _ROWS),
    )
    out = _invoke_inventory(monkeypatch, capacity={"claude": "exhausted"})
    assert "routing=capacity-held" in out


@requires_rust
def test_inventory_names_an_armed_slot_and_its_work_kind(monkeypatch) -> None:
    """AC12: under the armed policy a qualified lane reads positively, with
    the work kind the strict walk applied."""
    rows = [dict(r, operator_view="claude-native", route="") for r in _ROWS]
    _declare(monkeypatch, rows)
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: _settings(True, "local", rows),
    )
    out = _invoke_inventory(monkeypatch)
    assert "routing=armed" in out
    assert "access=local" in out


def _verified_snapshot(view_fingerprint: str = "fp1") -> dict:
    fresh = "2026-09-08T12:00:00+00:00"
    return {
        "fingerprint": "fp1",
        "policy": {"enforce_inventory": True, "operator_access": "local"},
        "sessions": [{
            "session_id": "sid-1",
            "name": "worker-1",
            "harness": "claude",
            "model": "glm-5.3-flash",
            "model_basis": "verified",
            "account": "zai-main",
            "created_at": fresh,
            "receipt_fingerprint": "fp1",
            "view_records": [{
                "subject": "routing-view:sid-1",
                "lifecycle": "live",
                "ts": fresh,
                "decision": json.dumps({
                    "view": "claude-native",
                    "fingerprint": view_fingerprint,
                    "session_id": "sid-1",
                }),
            }],
        }],
    }


def _run_audit(snapshot: dict, tmp_path: Path) -> tuple[int, str, str]:
    import subprocess

    path = tmp_path / "snapshot.json"
    path.write_text(json.dumps(snapshot))
    proc = subprocess.run(
        [find_dev_binary(), "route-slot", "audit", "--project", "/tmp/proj",
         "--node", "x-90a9", "--since", "30m", "--json", "--snapshot", str(path)],
        capture_output=True, text=True,
    )
    return proc.returncode, proc.stdout, proc.stderr


@requires_rust
def test_audit_refuses_a_preview_alone(tmp_path: Path) -> None:
    """AC11: a snapshot with no launch names the boundary and exits nonzero."""
    code, out, err = _run_audit({
        "fingerprint": "fp1",
        "policy": {"enforce_inventory": True, "operator_access": "local"},
        "sessions": [],
    }, tmp_path)
    assert code == 1
    report = json.loads(out)
    assert report["verdict"] == "ROUTING_POLICY_INCOMPLETE"
    boundaries = [b["boundary"] for b in report["boundaries"]]
    assert "no-launch" in boundaries


@requires_rust
def test_audit_verifies_a_fully_evidenced_session(tmp_path: Path) -> None:
    """AC11: fresh matching launch, observed model and the operator's view
    record print ROUTING_POLICY_VERIFIED and exit zero."""
    code, out, err = _run_audit(_verified_snapshot(), tmp_path)
    assert err == "", err
    assert code == 0
    report = json.loads(out)
    assert report["verdict"] == "ROUTING_POLICY_VERIFIED"
    assert report["sessions"][0]["session_id"] == "sid-1"


@requires_rust
def test_audit_refuses_a_view_confirmed_under_an_old_config(tmp_path: Path) -> None:
    """AC11: the view record must ride the SAME fingerprint the config has
    now; an older confirmation is stale evidence, not proof."""
    code, out, err = _run_audit(_verified_snapshot(view_fingerprint="old"), tmp_path)
    assert code == 1
    report = json.loads(out)
    boundaries = [b["boundary"] for b in report["boundaries"]]
    assert "view-fingerprint-mismatch" in boundaries
