from __future__ import annotations

import asyncio
import json
import os
import sys
from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app
from fno.review.orchestrator import OrchestratorResult
from fno.review.report_builder import render_artifact_markdown
from fno.retro.harvest import extract_severity
from fno.worker.review import build_review_runner


def test_zero_bot_blocking_artifact_blocks_until_durable_disposition(
    tmp_path: Path,
) -> None:
    report = tmp_path / "report.md"
    report.write_text("# Review\n\n- **P1** - fix this\n", encoding="utf-8")
    reviews_root = tmp_path / "internal"
    common = [
        "--sigma-node",
        "x-bfbb",
        "--sigma-pr",
        "42",
        "--sigma-head",
        "head-a",
        "--sigma-project",
        "fno",
        "--sigma-reviews-root",
        str(reviews_root),
        "--json",
    ]
    published = CliRunner().invoke(
        app,
        [
            "do", "review",
            "--publish-sigma",
            str(report),
            *common,
            "--sigma-current-head",
            "head-a",
            "--sigma-round",
            "round-a",
        ],
    )
    assert published.exit_code == 0, published.output
    assert json.loads(published.output)["finding_count"] == 1

    inspected = CliRunner().invoke(app, ["do", "review", "--inspect-sigma", *common])
    assert inspected.exit_code == 0, inspected.output
    artifact = json.loads(inspected.output)
    assert artifact["status"] == "accepted"
    finding_lines = [
        line for line in artifact["body"].splitlines() if extract_severity(line)
    ]
    stable_ids = [
        f"sigma:{artifact['review_round']}:{index}"
        for index, _line in enumerate(finding_lines, start=1)
    ]
    assert stable_ids == ["sigma:round-a:1"]
    severities = [extract_severity(line) for line in finding_lines]
    unresolved = list(stable_ids)
    verdict = (
        "blocked"
        if any(severity in {"critical", "high"} for severity in severities)
        and unresolved
        else "ready"
    )
    assert severities == ["high"]
    assert unresolved == ["sigma:round-a:1"]
    assert verdict == "blocked"

    disposition_comment = (
        "Fixed.\n<!-- fno-sigma-disposition "
        f"id={stable_ids[0]} head=head-a -->"
    )
    unresolved = [
        stable_id
        for stable_id in stable_ids
        if f"id={stable_id} head=head-a" not in disposition_comment
    ]
    assert unresolved == []
    verdict = (
        "blocked"
        if any(severity in {"critical", "high"} for severity in severities)
        and unresolved
        else "ready"
    )
    assert verdict == "ready"

    stale = CliRunner().invoke(
        app,
        [
            "do", "review",
            "--inspect-sigma",
            *[
                "head-b" if token == "head-a" else token
                for token in common
            ],
        ],
    )
    assert stale.exit_code == 0, stale.output
    assert json.loads(stale.output)["status"] == "rejected"


def test_configured_route_reaches_real_headless_spawn_and_report(
    tmp_path: Path, monkeypatch
) -> None:
    from fno import config as config_mod
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(
        "schema_version: 1\nconfig:\n  review:\n    agent_routes:\n"
        "      code_reviewer:\n        harness: claude\n"
        "        provider: zai\n        model: glm-5.2\n",
        encoding="utf-8",
    )
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    argv_dump = tmp_path / "claude-argv.json"
    fake_claude = bin_dir / "claude"
    fake_claude.write_text(
        f"#!{sys.executable}\n"
        "import json, os, sys\n"
        "with open(os.environ['CLAUDE_ARGV_DUMP'], 'w') as handle:\n"
        "    json.dump(sys.argv[1:], handle)\n"
        "print('[]')\n",
        encoding="utf-8",
    )
    fake_claude.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("CLAUDE_ARGV_DUMP", str(argv_dump))
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    monkeypatch.setenv("ZAI_API_KEY", "test-key")
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    route = config_mod.load_settings().review.agent_routes

    runner, _prompts, dimension = build_review_runner(
        agent_providers={},
        agent_routes=route,
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude"],
        base_prompts={"code_reviewer": "Review the diff."},
        cwd=tmp_path,
    )
    assert runner is not None
    outcome = asyncio.run(runner("code_reviewer", "Review the diff.", "diff"))
    assert outcome.ok is True
    assert outcome.provider == "claude"
    assert outcome.route_provider == "zai"
    assert outcome.model == "glm-5.2"
    assert dimension == ["code_reviewer=claude/zai/glm-5.2"]
    argv = json.loads(argv_dump.read_text(encoding="utf-8"))
    assert argv[0] == "-p"
    assert argv[argv.index("--model") + 1] == "glm-5.2"
    assert argv[argv.index("--agent") + 1] == "fno:code-reviewer"
    assert "--settings" in argv

    report = render_artifact_markdown(
        "session",
        OrchestratorResult(
            findings=[],
            workers_completed=1,
            workers_failed=0,
            suspicious=False,
            duration_seconds=outcome.duration_seconds,
            outcomes=[outcome],
        ),
        "ready-to-merge",
    )
    assert "[claude/zai/glm-5.2]" in report
    assert "fno:code-reviewer" not in report
