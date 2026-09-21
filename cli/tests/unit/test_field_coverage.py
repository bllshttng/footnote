from __future__ import annotations

import json
import sys
from pathlib import Path

import typer
import typer.main
from click.testing import CliRunner

from fno.agents.registry import AgentEntry
from fno.lint_cli import lint


def _live_lint_command():
    sub = typer.Typer(add_completion=False)
    sub.command(name="lint")(lint)
    return typer.main.get_command(sub)


app = _live_lint_command()
runner = CliRunner()


def _invoke(monkeypatch, repo: Path, *args: str):
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    return runner.invoke(app, ["field-coverage", *args])


def _write_source_fixture(repo: Path, *, required: list[str], extra: list[str]) -> None:
    registry = repo / "cli" / "src" / "fno" / "agents" / "registry.py"
    registry.parent.mkdir(parents=True)
    fields = "\n".join(f"    {name}: str | None = None" for name in required + extra)
    registry.write_text(
        "from dataclasses import dataclass\n\n@dataclass\nclass AgentEntry:\n" + fields + "\n",
        encoding="utf-8",
    )
    schema = repo / "schemas" / "agents-list-row.json"
    schema.parent.mkdir(parents=True)
    schema.write_text(
        json.dumps(
            {
                "required": required,
                "projection_omissions": [],
                "removed": {},
                "stored": {},
                "derived": {},
                "rust_only": {"keys": []},
                "python_only": {"keys": []},
                "storage_only": [],
                "known_gaps": {},
            }
        ),
        encoding="utf-8",
    )


def test_source_coverage_accounts_current_agent_entry(monkeypatch) -> None:
    repo = Path(__file__).resolve().parents[3]

    result = _invoke(monkeypatch, repo, "--json")

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)["source"]
    # v25 added the three spawn-stamped route-identity fields
    # (route_provider_id, model_name, account_record_id): 45 -> 48 declared,
    # all accounted as storage_only in schemas/agents-list-row.json. v26
    # added the served facts (liveness, liveness_measured_at, harness_title):
    # 48 -> 54 declared, accounted as rust_only (the Rust row projects them).
    # x-7955: substrate moved out of storage_only into the projected key set.
    # The reign parent-edge change adds required spawned_by_session on top:
    # 41 -> 43 required, declared unchanged at 54 (it was already a declared
    # v26 leaf). v27 added launch_account_source, storage_only: 54 -> 55.
    # v28 added adopted_by_session (the adoption voucher, x-5283),
    # storage_only: 55 -> 56. v29 added resolved_sandbox and
    # granted_writable_roots (the codex thread lane's resolved posture vs
    # the sandbox_posture request), storage_only: 56 -> 58. v30 added
    # git_grant (the effective Git common-dir receipt), storage_only:
    # 58 -> 59. last_activity_basis joined the required list (the age's
    # instrument word): 43 -> 44. v32 added stop (fno's own stop record),
    # storage_only: 59 -> 60. v33 added lineage_reason (why no parent
    # session could be named), storage_only: 60 -> 61, and spawn_id +
    # spawn_provenance (the spawn door's attempt id and structured birth
    # record), storage_only: 61 -> 63. v34 added lineage_kind (the served
    # CHILD/PEER spawn-edge word), storage_only: 63 -> 64, and it joined
    # the required list: 44 -> 45. v35 added the codex thread posture
    # record (requested_permission_mode, turn_policy_source),
    # storage_only: 64 -> 66. v36 added node_reason (why the row works no
    # node when the spawn NAMED one), storage_only: 66 -> 67, and it joined
    # the required list: 45 -> 46.
    assert payload["declared_count"] == 67
    assert payload["required_count"] == 46
    assert payload["accounted_count"] == 67
    assert payload["known_gaps"] == {}


def test_source_coverage_rediscovers_node_projection_omission_and_choices(
    monkeypatch, tmp_path: Path
) -> None:
    required = [f"field_{index}" for index in range(40)]
    _write_source_fixture(tmp_path, required=required, extra=["node"])
    schema_path = tmp_path / "schemas" / "agents-list-row.json"
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    schema["stored"] = {"node": "Descriptive metadata is not a disposition."}
    schema_path.write_text(json.dumps(schema), encoding="utf-8")

    result = _invoke(monkeypatch, tmp_path)

    assert result.exit_code == 1
    assert "unaccounted field: node" in result.output
    assert "required and project it" in result.output
    assert "storage_only" in result.output
    assert "known_gaps" in result.output


def test_source_coverage_rejects_known_gap_without_owner(
    monkeypatch, tmp_path: Path
) -> None:
    required = [f"field_{index}" for index in range(40)]
    _write_source_fixture(tmp_path, required=required, extra=["node"])
    schema_path = tmp_path / "schemas" / "agents-list-row.json"
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    schema["known_gaps"] = {"node": ""}
    schema_path.write_text(json.dumps(schema), encoding="utf-8")

    result = _invoke(monkeypatch, tmp_path)

    assert result.exit_code == 1
    assert "unaccounted field: node" in result.output


def test_source_coverage_refuses_empty_accounting(monkeypatch, tmp_path: Path) -> None:
    _write_source_fixture(
        tmp_path,
        required=[],
        extra=[f"field_{index}" for index in range(40)],
    )

    result = _invoke(monkeypatch, tmp_path, "--live")

    assert result.exit_code == 2
    assert "UNMEASURED" in result.output
    assert "clean" not in result.output.lower()


def test_live_coverage_classifies_contract_zeroes_and_keeps_real_dead(
    monkeypatch,
) -> None:
    import fno.agents.registry as registry

    entry = AgentEntry(
        name="positive-control",
        cwd="/tmp/project",
        log_path="/tmp/session.log",
        harness="codex",
    )
    monkeypatch.setattr(registry, "load_registry", lambda: [entry])
    repo = Path(__file__).resolve().parents[3]

    result = _invoke(monkeypatch, repo, "--live", "--json")

    assert result.exit_code == 1, result.output
    payload = json.loads(result.output)
    assert {"name", "created_at", "harness", "status"} <= set(
        payload["anchors"]
    )
    assert payload["contract_errors"] == []
    contracted = {
        "delivery_policy",
        "forked_from_session_id",
        "predecessor_session_ids",
        "live_status",
        "live_status_basis",
    }
    persisted = payload["persisted"]
    projected = payload["projected"]
    # contract-covered zeroes are measured, never dead-field findings
    assert not contracted & set(persisted["dead_fields"])
    assert not contracted & set(projected["dead_fields"])
    # lineage fields are conditional and persisted_and_projected
    assert {"forked_from_session_id", "predecessor_session_ids"} <= set(
        persisted["conditional_zero"]
    )
    assert {"forked_from_session_id", "predecessor_session_ids"} <= set(
        projected["conditional_zero"]
    )
    forked = persisted["conditional_zero"]["forked_from_session_id"]
    assert forked["count"] == 0
    assert forked["writer"]
    assert forked["test"] == "cli/tests/agents/test_session_lineage.py"
    # delivery_policy is transient on both readings
    assert persisted["transient_zero"]["delivery_policy"]["mode"] == "transient"
    assert projected["transient_zero"]["delivery_policy"]["mode"] == "transient"
    # liveness fields are projected-only enrichment
    assert {"live_status", "live_status_basis"} <= set(
        projected["conditional_zero"]
    )
    # crown fields have no population contract: still dead findings
    assert {"crown", "crown_grantor", "crown_level", "crown_scope"} <= set(
        projected["dead_fields"]
    )


def test_live_coverage_fixture_zero_without_contract_stays_dead(
    monkeypatch, tmp_path: Path
) -> None:
    import fno.agents.registry as registry

    entry = AgentEntry(
        name="positive-control",
        cwd="/tmp/project",
        log_path="/tmp/session.log",
        harness="codex",
    )
    monkeypatch.setattr(registry, "load_registry", lambda: [entry])
    _write_source_fixture(
        tmp_path,
        required=[f"field_{index}" for index in range(40)],
        extra=[],
    )
    schema_path = tmp_path / "schemas" / "agents-list-row.json"
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    schema["population_contract"] = {
        "pid": {
            "mode": "conditional",
            "surface": "persisted_and_projected",
            "writer": "the reconcile sweep",
            "test": "cli/tests/unit/test_field_coverage.py",
        }
    }
    schema_path.write_text(json.dumps(schema), encoding="utf-8")

    result = _invoke(monkeypatch, tmp_path, "--live", "--json")

    assert result.exit_code == 1, result.output
    payload = json.loads(result.output)
    # covered zero moves out of dead_fields
    assert "pid" not in payload["persisted"]["dead_fields"]
    assert "pid" in payload["persisted"]["conditional_zero"]
    # uncovered zeroes stay findings on both readings
    assert "stop" in payload["persisted"]["dead_fields"]
    assert "crown" in payload["projected"]["dead_fields"]


def test_live_coverage_malformed_contract_metadata_is_a_finding(
    monkeypatch, tmp_path: Path
) -> None:
    import fno.agents.registry as registry

    entry = AgentEntry(
        name="positive-control",
        cwd="/tmp/project",
        log_path="/tmp/session.log",
        harness="codex",
    )
    monkeypatch.setattr(registry, "load_registry", lambda: [entry])
    _write_source_fixture(
        tmp_path,
        required=[f"field_{index}" for index in range(40)],
        extra=[],
    )
    schema_path = tmp_path / "schemas" / "agents-list-row.json"
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    schema["population_contract"] = {
        "crown": {"mode": "sometimes", "surface": "projected"},
    }
    schema_path.write_text(json.dumps(schema), encoding="utf-8")

    result = _invoke(monkeypatch, tmp_path, "--live", "--json")

    assert result.exit_code == 1, result.output
    payload = json.loads(result.output)
    assert payload["contract_errors"], payload
    # the malformed entry covers nothing
    assert "crown" in payload["projected"]["dead_fields"]


def test_live_coverage_refuses_empty_registry(monkeypatch) -> None:
    import fno.agents.registry as registry

    monkeypatch.setattr(registry, "load_registry", lambda: [])
    repo = Path(__file__).resolve().parents[3]

    result = _invoke(monkeypatch, repo, "--live")

    assert result.exit_code == 2
    assert "UNMEASURED" in result.output
    assert "zero persisted rows" in result.output


def test_live_coverage_refuses_unreadable_registry(monkeypatch) -> None:
    import fno.agents.registry as registry

    def unreadable():
        raise registry.RegistryVersionError("registry schema is unreadable")

    monkeypatch.setattr(registry, "load_registry", unreadable)
    repo = Path(__file__).resolve().parents[3]

    result = _invoke(monkeypatch, repo, "--live")

    assert result.exit_code == 2
    assert "UNMEASURED" in result.output
    assert "registry schema is unreadable" in result.output


# --- closure probe: check-agent-field-population-contract ---


def _repo() -> Path:
    return Path(__file__).resolve().parents[3]


def _load_checker():
    import importlib.util

    path = _repo() / "scripts" / "ci" / "check-agent-field-population-contract.py"
    spec = importlib.util.spec_from_file_location("closure_probe", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _closure_report() -> dict:
    forked = {
        "mode": "conditional",
        "surface": "persisted_and_projected",
        "writer": "record_session_observation branch classification",
        "test": "cli/tests/agents/test_session_lineage.py",
    }
    delivery = {
        "mode": "transient",
        "surface": "persisted_and_projected",
        "writer": "fno agents mail hold while a busy-mode hold is active",
        "test": "cli/tests/unit/test_mail_hold.py",
    }
    live = {
        "mode": "conditional",
        "surface": "projected",
        "writer": "read.py Claude supervisor enrichment",
        "test": "cli/tests/agents/test_read.py",
    }
    anchor = {"set": 7, "total": 7}
    return {
        "status": "ok",
        "anchors": {name: dict(anchor) for name in
                    ("name", "created_at", "harness", "status")},
        "contract_errors": [],
        "persisted": {
            "total": 7,
            "counts": {"forked_from_session_id": 0,
                       "predecessor_session_ids": 0,
                       "delivery_policy": 0, "name": 7},
            "dead_fields": [],
            "conditional_zero": {
                "forked_from_session_id": dict(forked),
                "predecessor_session_ids": dict(forked),
            },
            "transient_zero": {"delivery_policy": dict(delivery)},
        },
        "projected": {
            "total": 7,
            "counts": {"forked_from_session_id": 0, "live_status": 0,
                       "live_status_basis": 0, "delivery_policy": 0, "name": 7},
            "dead_fields": [],
            "conditional_zero": {
                "forked_from_session_id": dict(forked),
                "predecessor_session_ids": dict(forked),
                "live_status": dict(live),
                "live_status_basis": dict(live),
            },
            "transient_zero": {"delivery_policy": dict(delivery)},
        },
    }


def test_closure_probe_prints_marker_on_complete_report() -> None:
    import subprocess

    result = subprocess.run(
        [sys.executable,
         str(_repo() / "scripts" / "ci"
             / "check-agent-field-population-contract.py")],
        input=json.dumps(_closure_report()).encode(),
        capture_output=True,
    )

    assert result.returncode == 0, result.stderr
    assert b"agent-field-population-contract: verified" in result.stdout


def test_closure_probe_fails_on_unmeasured_report() -> None:
    checker = _load_checker()
    payload = _closure_report()
    payload["status"] = "unmeasured"

    code = checker.check(payload, checker._schema_expectations(_repo()))

    assert code != 0


def test_closure_probe_fails_on_dead_contract_field() -> None:
    checker = _load_checker()
    payload = _closure_report()
    payload["persisted"]["dead_fields"] = ["forked_from_session_id"]

    assert checker.check(payload, checker.FIELDS) != 0


def test_closure_probe_fails_on_missing_classification() -> None:
    checker = _load_checker()
    payload = _closure_report()
    del payload["persisted"]["transient_zero"]["delivery_policy"]

    assert checker.check(payload, checker.FIELDS) != 0


def test_closure_probe_fails_on_malformed_json() -> None:
    import subprocess

    result = subprocess.run(
        [sys.executable,
         str(_repo() / "scripts" / "ci"
             / "check-agent-field-population-contract.py")],
        input=b"not json",
        capture_output=True,
    )

    assert result.returncode != 0
    assert b"agent-field-population-contract: verified" not in result.stdout


def test_closure_probe_refuses_schema_missing_entry(tmp_path: Path) -> None:
    checker = _load_checker()
    schema_dir = tmp_path / "schemas"
    schema_dir.mkdir()
    schema = {"population_contract": {}}
    (schema_dir / "agents-list-row.json").write_text(
        json.dumps(schema), encoding="utf-8"
    )

    assert checker._schema_expectations(tmp_path) is None
    assert checker.check(_closure_report(), None) != 0


def test_closure_probe_schema_agrees_with_declared_fields() -> None:
    checker = _load_checker()

    expectations = checker._schema_expectations(_repo())

    assert expectations == checker.FIELDS
