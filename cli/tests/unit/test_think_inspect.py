from __future__ import annotations

import json
import subprocess
from pathlib import Path

from typer.testing import CliRunner


def _result(argv: list[str], cwd: Path | None = None, timeout: int = 10):
    if argv[:2] == ["git", "rev-parse"]:
        value = "feature/x-4007\n" if "--abbrev-ref" in argv else "abc123def456\n"
        return subprocess.CompletedProcess(argv, 0, value, "")
    if argv[:2] == ["git", "status"]:
        return subprocess.CompletedProcess(argv, 0, " M skills/think/SKILL.md\n", "")
    if argv[:2] == ["git", "log"]:
        return subprocess.CompletedProcess(argv, 0, "abc123\tPrior change\n", "")
    if argv[:3] == ["gh", "pr", "list"]:
        assert argv[argv.index("--search") + 1] == "Lean think discovery receipt"
        payload = [{"number": 608, "title": "Lean planning", "state": "MERGED", "url": "https://example.test/608"}]
        return subprocess.CompletedProcess(argv, 0, json.dumps(payload), "")
    raise AssertionError(f"unexpected argv: {argv}")


def test_receipt_collects_grounding_without_writes(tmp_path: Path) -> None:
    from fno.think_inspect import build_receipt

    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "prisma").mkdir()
    (repo / "prisma" / "schema.prisma").write_text("model User {}\n")
    (repo / ".fno").mkdir()
    (repo / ".fno" / "codemap.md").write_text("# Map\n\n## Database Schema\n\n### public.users\n")
    (repo / "AGENTS.md").write_text(
        "# Agents\n\n## Pitfalls corpus (capped)\n\n"
        "### Guards can lie\n\nTrap.\n\n- graduates-to: lint\n- added: 2026-07-25\n\n"
        "## Repository\n\n### Not a pitfall\n"
    )
    plans = tmp_path / "plans"
    plans.mkdir()
    retro = plans / "20260725-retro-synthesis-x-abcd.md"
    retro.write_text("# Retro\n")
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    (home / ".fno" / "lesson-candidates.jsonl").write_text("{}\n{}\n")
    graph = [
        {
            "id": "x-4007",
            "slug": "lean-think",
            "title": "Lean think discovery receipt",
            "details": "deterministic discovery for think",
            "status": "in_progress",
            "domain": "code",
            "parent": "x-7a93",
        },
        {
            "id": "x-1111",
            "slug": "think-discovery",
            "title": "Think deterministic discovery",
            "details": "receipt for think discovery",
            "status": "done",
            "domain": "code",
        },
        {
            "id": "x-7a93",
            "slug": "context-evolution",
            "title": "Lean think graph evolution",
            "details": "context and graph evolution epic",
            "status": "ready",
            "domain": "code",
            "type": "epic",
        },
    ]
    before = {p.relative_to(tmp_path): p.stat().st_mtime_ns for p in tmp_path.rglob("*") if p.is_file()}

    receipt = build_receipt(
        "x-4007",
        repo=repo,
        graph_entries=graph,
        archive_entries=[],
        plans_path=plans,
        home=home,
        run=_result,
    )

    after = {p.relative_to(tmp_path): p.stat().st_mtime_ns for p in tmp_path.rglob("*") if p.is_file()}
    assert after == before
    assert receipt["version"] == 1
    assert receipt["repository"]["branch"] == "feature/x-4007"
    assert receipt["repository"]["dirty_paths"] == ["skills/think/SKILL.md"]
    assert receipt["graph"]["resolved"]["id"] == "x-4007"
    assert receipt["graph"]["parent"]["id"] == "x-7a93"
    assert [row["id"] for row in receipt["graph"]["duplicates"]] == ["x-1111"]
    assert receipt["graph"]["epic_candidates"][0]["id"] == "x-7a93"
    assert receipt["pull_requests"]["status"] == "ok"
    assert receipt["pull_requests"]["matches"][0]["number"] == 608
    assert receipt["database"] == {
        "detected": True,
        "signals": ["prisma/schema.prisma"],
        "schema_artifact": ".fno/codemap.md",
        "schema_status": "grounded",
    }
    assert receipt["pitfalls"]["entries"] == ["Guards can lie"]
    assert receipt["pitfalls"]["retro_syntheses"] == [str(retro)]
    assert receipt["pitfalls"]["lesson_candidates"] == 2
    assert receipt["complete"] is True


def test_receipt_exposes_unavailable_evidence(tmp_path: Path) -> None:
    from fno.think_inspect import build_receipt

    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "drizzle.config.ts").write_text("export default {}\n")

    def unavailable(argv: list[str], cwd: Path | None = None, timeout: int = 10):
        if argv[0] == "gh":
            raise FileNotFoundError("gh")
        return subprocess.CompletedProcess(argv, 1, "", "not a git repo")

    receipt = build_receipt(
        "new database feature",
        repo=repo,
        graph_entries=None,
        graph_error="graph unreadable",
        archive_entries=[],
        plans_path=tmp_path / "missing-plans",
        home=tmp_path,
        run=unavailable,
    )

    assert receipt["graph"]["status"] == "error"
    assert receipt["pull_requests"] == {"status": "unavailable", "matches": [], "detail": "gh not found"}
    assert receipt["database"]["schema_status"] == "missing"
    assert receipt["complete"] is False
    assert "database schema evidence is missing" in receipt["warnings"]


def test_corrupt_graph_is_not_recast_as_empty(tmp_path: Path) -> None:
    from fno.think_inspect import _load_graph

    graph = tmp_path / "graph.json"
    graph.write_text("{not json")

    entries, error = _load_graph(graph)

    assert entries is None
    assert error and "not valid JSON" in error


def test_receipt_resolves_paths_from_requested_repository(monkeypatch, tmp_path: Path) -> None:
    from fno.think_inspect import build_receipt

    repo = tmp_path / "target-repo"
    (repo / ".fno").mkdir(parents=True)
    (repo / ".fno" / "config.toml").write_text(
        'plans_dir = "target-plans"\n'
        '[paths]\n'
        'graph_json = "target-state/graph.json"\n'
    )
    graph_path = repo / "target-state" / "graph.json"
    graph_path.parent.mkdir()
    graph_path.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-target",
                        "slug": "target-feature",
                        "title": "Target feature",
                        "status": "ready",
                    }
                ]
            }
        )
    )
    plans = repo / "target-plans"
    plans.mkdir()
    retro = plans / "20260725-retro-synthesis-x-target.md"
    retro.write_text("# Retro\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")

    receipt = build_receipt(
        "x-target",
        repo=repo,
        home=tmp_path,
        run=_result_without_title_assertion,
    )

    assert receipt["graph"]["resolved"]["id"] == "x-target"
    assert receipt["pitfalls"]["retro_syntheses"] == [str(retro)]


def test_schema_artifact_older_than_schema_source_is_stale(tmp_path: Path) -> None:
    from fno.think_inspect import build_receipt

    repo = tmp_path / "repo"
    (repo / "prisma").mkdir(parents=True)
    (repo / ".fno").mkdir()
    artifact = repo / ".fno" / "codemap.md"
    artifact.write_text("## Database Schema\n")
    schema = repo / "prisma" / "schema.prisma"
    schema.write_text("model User {}\n")
    artifact.touch()
    schema.touch()
    artifact_mtime = artifact.stat().st_mtime_ns
    schema_mtime = max(schema.stat().st_mtime_ns, artifact_mtime + 1_000_000)
    import os

    os.utime(schema, ns=(schema_mtime, schema_mtime))

    receipt = build_receipt(
        "database change",
        repo=repo,
        graph_entries=[],
        archive_entries=[],
        plans_path=tmp_path / "plans",
        home=tmp_path,
        run=_result_without_title_assertion,
    )

    assert receipt["database"]["schema_status"] == "stale"
    assert "database schema evidence is stale" in receipt["warnings"]
    assert receipt["complete"] is False


def _result_without_title_assertion(argv: list[str], cwd: Path | None = None, timeout: int = 10):
    if argv[:3] == ["gh", "pr", "list"]:
        return subprocess.CompletedProcess(argv, 0, "[]", "")
    return _result(argv, cwd, timeout)


def test_exact_archived_node_is_labeled_and_keeps_pr_link(tmp_path: Path) -> None:
    from fno.think_inspect import build_receipt

    repo = tmp_path / "repo"
    repo.mkdir()
    archived = {
        "id": "x-dead",
        "slug": "old-feature",
        "title": "Old feature",
        "status": "done",
        "pr_number": 321,
        "pr_url": "https://example.test/321",
    }

    receipt = build_receipt(
        "x-dead",
        repo=repo,
        graph_entries=[],
        archive_entries=[archived],
        plans_path=tmp_path / "plans",
        home=tmp_path,
        run=_result_without_title_assertion,
    )

    assert receipt["graph"]["resolved"]["archived"] is True
    assert receipt["graph"]["resolved"]["pr_number"] == 321


def test_cli_emits_machine_readable_receipt(monkeypatch, tmp_path: Path) -> None:
    from fno.provenance.cli import think_app

    expected = {"version": 1, "complete": True}
    monkeypatch.setattr("fno.think_inspect.build_receipt", lambda seed, repo: expected)
    result = CliRunner().invoke(think_app, ["inspect", "dark mode", "--repo", str(tmp_path), "--json"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.output) == expected


def test_cli_rejects_missing_repository(monkeypatch, tmp_path: Path) -> None:
    from fno.provenance.cli import think_app

    monkeypatch.setattr(
        "fno.think_inspect.build_receipt",
        lambda seed, repo: (_ for _ in ()).throw(AssertionError("must not inspect")),
    )
    missing = tmp_path / "missing"
    result = CliRunner().invoke(think_app, ["inspect", "idea", "--repo", str(missing)])

    assert result.exit_code == 2
    assert "repository directory not found" in result.output
