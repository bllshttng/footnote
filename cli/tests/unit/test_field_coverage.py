from __future__ import annotations

import json
from pathlib import Path

import typer
import typer.main
from click.testing import CliRunner

from fno import paths
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
    paths.resolve_repo_root.cache_clear()
    try:
        return runner.invoke(app, ["field-coverage", *args])
    finally:
        paths.resolve_repo_root.cache_clear()


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
    # x-7955: substrate moved out of storage_only into the projected key set
    # (required 41 -> 42). The reign parent-edge change adds required
    # spawned_by_session on top of v26: 54 -> 55 declared, 41 -> 43 required,
    # counted from the merged tree.
    assert payload["declared_count"] == 55
    assert payload["required_count"] == 43
    assert payload["accounted_count"] == 55
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


def test_live_coverage_rediscovers_known_dead_fields(monkeypatch) -> None:
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
    assert {
        "delivery_policy",
        "forked_from_session_id",
        "predecessor_session_ids",
    } <= set(payload["persisted"]["dead_fields"])
    assert {
        "crown",
        "crown_grantor",
        "crown_level",
        "crown_scope",
        "live_status",
        "live_status_basis",
    } <= set(payload["projected"]["dead_fields"])


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
