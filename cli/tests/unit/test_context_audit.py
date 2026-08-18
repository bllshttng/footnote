"""Read-only context census and compiler contracts for x-2e3c Task 1.1."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import context_observation
from fno.cli import app
from fno.context_audit import (
    ContextSource,
    MeasurementKind,
    SUPPORTED_HARNESSES,
    active_pitfall_headings,
    build_context_report,
    compile_context,
    measure_file_source,
)
from fno.setup.managed_block import render_block


ROOT = Path(__file__).resolve().parents[3]
pytestmark = pytest.mark.xdist_group("context-audit-processes")


def _source(
    source_id: str,
    content: bytes,
    *,
    layer: str = "progressive",
    harness: str = "claude",
    ordinal: int = 0,
) -> ContextSource:
    return ContextSource(
        source_id=source_id,
        harness=harness,
        entry_state="startup",
        lifecycle="session_start",
        layer=layer,
        provenance=f"fixture/{source_id}.md",
        carrier=f"fixture/{source_id}.sh",
        reachability_condition="fixture",
        content=content,
        ordinal=ordinal,
    )


def test_census_fingerprints_sources_and_deduplicates_content() -> None:
    shared = b"Never hand-edit managed state.\n"
    sources = [
        _source("kernel", b"Footnote safety kernel.\n", layer="kernel"),
        _source("guard-a", shared, ordinal=1),
        _source("guard-b", shared, ordinal=2),
    ]
    report = compile_context(
        sources,
        packet_budget_bytes=4_096,
        node_count=1,
    )

    records = report["source_manifest"]
    content_by_id = {source.source_id: source.content for source in sources}
    for record in records:
        assert record["harness"] == "claude"
        assert record["entry_state"] == "startup"
        assert record["bytes"] == len(content_by_id[record["source_id"]])
        assert record["estimated_tokens"] == (record["bytes"] + 3) // 4
        assert len(record["content_hash"]) == 64
        assert record["provenance"].startswith("fixture/")

    assert report["duplicates"] == [
        {
            "content_hash": hashlib.sha256(shared).hexdigest(),
            "source_ids": ["guard-a", "guard-b"],
            "occurrences": 2,
        }
    ]
    assert report["kernel"]["source_ids"] == ["kernel"]
    assert [item["source_id"] for item in report["progressive_sources"]] == ["guard-a"]


def test_conflicting_versions_retain_provenance() -> None:
    report = compile_context(
        [
            _source("worktree-rule", b"Use a worktree.\n", ordinal=1),
            ContextSource(
                source_id="worktree-rule",
                harness="codex",
                entry_state="startup",
                lifecycle="session_start",
                layer="progressive",
                provenance="fixture/codex.md",
                carrier="fixture/codex.sh",
                reachability_condition="fixture",
                content=b"Work in the canonical checkout.\n",
                ordinal=2,
            ),
        ],
        packet_budget_bytes=4_096,
        node_count=1,
    )

    assert report["conflicts"] == [
        {
            "source_id": "worktree-rule",
            "content_hashes": sorted(
                [
                    hashlib.sha256(b"Use a worktree.\n").hexdigest(),
                    hashlib.sha256(b"Work in the canonical checkout.\n").hexdigest(),
                ]
            ),
            "provenance": ["fixture/worktree-rule.md", "fixture/codex.md"],
        }
    ]


def test_packet_budget_emits_explicit_omission_and_continuation() -> None:
    report = compile_context(
        [
            _source("kernel", b"1234", layer="kernel"),
            _source("first", b"5678", ordinal=1),
            _source("second", b"90", ordinal=2),
        ],
        packet_budget_bytes=8,
        node_count=1,
    )

    assert report["packet"]["bytes"] == 8
    assert report["packet"]["within_budget"] is True
    assert [item["source_id"] for item in report["progressive_sources"]] == ["first"]
    assert report["omitted_sources"] == [
        {
            "source_id": "second",
            "provenance": "fixture/second.md",
            "reason": "packet_budget",
            "bytes": 2,
        }
    ]
    assert report["continuation"] == {
        "after_source_id": "first",
        "remaining_source_ids": ["second"],
        "remaining_bytes": 2,
    }


def test_carrier_template_bytes_never_enter_delivered_packet_totals() -> None:
    carrier = ContextSource(
        source_id="dynamic-hook",
        harness="claude",
        entry_state="startup",
        lifecycle="session_start",
        layer="progressive",
        provenance="hooks/dynamic.sh",
        carrier="hooks/dynamic.sh",
        reachability_condition="fixture",
        content=b"implementation bytes that may emit nothing",
        ordinal=1,
        packet_eligible=False,
        measurement=MeasurementKind.CARRIER_TEMPLATE_BYTES,
    )

    report = compile_context(
        [_source("kernel", b"kernel", layer="kernel"), carrier],
        packet_budget_bytes=100,
        node_count=1,
    )

    assert report["packet"]["bytes"] == len(b"kernel")
    assert report["source_manifest"][1]["measurement"] == "carrier_template_bytes"
    assert report["source_manifest"][1]["packet_eligible"] is False
    assert report["source_manifest"][1]["bytes"] == 0
    assert report["source_manifest"][1]["content_hash"] is None
    assert report["source_manifest"][1]["carrier_bytes"] == len(carrier.content)
    assert report["source_manifest"][1]["carrier_hash"] == hashlib.sha256(
        carrier.content
    ).hexdigest()
    assert report["duplicates"] == []


def test_oversized_kernel_never_allows_progressive_content_without_it() -> None:
    report = compile_context(
        [
            _source("kernel", b"kernel-too-large", layer="kernel"),
            _source("small-progressive", b"x", ordinal=1),
        ],
        packet_budget_bytes=4,
        node_count=1,
    )

    assert report["packet"]["bytes"] == 0
    assert report["kernel"]["source_ids"] == []
    assert report["progressive_sources"] == []
    assert report["continuation"]["remaining_source_ids"] == [
        "kernel",
        "small-progressive",
    ]


def test_unreadable_source_is_reported_not_dropped(tmp_path: Path) -> None:
    missing = tmp_path / "missing.md"
    source = measure_file_source(
        source_id="missing",
        path=missing,
        harness="gemini",
        entry_state="startup",
        lifecycle="session_start",
        layer="progressive",
        carrier="hooks/session-start.sh",
        reachability_condition="fixture",
        ordinal=0,
        repo_root=tmp_path,
    )

    report = compile_context([source], packet_budget_bytes=100, node_count=1)
    assert report["source_manifest"][0]["status"] == "unreadable"
    assert report["source_manifest"][0]["content_hash"] is None
    assert report["omitted_sources"][0]["reason"].startswith("unreadable:")


def test_one_node_path_never_compiles_or_dispatches_a_graph() -> None:
    report = compile_context(
        [_source("kernel", b"kernel", layer="kernel")],
        packet_budget_bytes=100,
        node_count=1,
    )

    assert report["execution"] == {
        "mode": "existing_single_loop",
        "graph_compiled": False,
        "dispatch_changed": False,
    }


def test_required_pitfalls_reachable_for_every_harness() -> None:
    """Pass^3 eval target: every active capped-corpus entry reaches each kernel."""
    expected = active_pitfall_headings(ROOT)
    assert expected, "the capped AGENTS.md pitfalls corpus must not be empty"

    report = build_context_report(
        ROOT,
        harnesses=SUPPORTED_HARNESSES,
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )
    by_harness = {cell["harness"]: cell for cell in report["cells"]}

    assert set(by_harness) == set(SUPPORTED_HARNESSES)
    for harness in SUPPORTED_HARNESSES:
        kernel = by_harness[harness]["compiled"]["kernel"]
        assert kernel["anchors"]["pitfalls"] == expected
        assert "kdc-delivery-sentinel-1932" in kernel["anchors"]["sentinels"]


def test_repository_instruction_stubs_compile_to_the_exact_same_hash() -> None:
    report = build_context_report(
        ROOT,
        harnesses=SUPPORTED_HARNESSES,
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )

    hashes = {
        next(
            item
            for item in cell["compiled"]["source_manifest"]
            if item["source_id"] == "project-instructions"
        )["content_hash"]
        for cell in report["cells"]
    }
    assert len(hashes) == 1


def test_context_doctor_surface_is_machine_readable() -> None:
    result = CliRunner().invoke(
        app,
        [
            "doctor",
            "--context-audit",
            "--context-harness",
            "claude",
            "--context-entry",
            "startup",
            "--context-budget",
            "100000",
            "--source",
            str(ROOT),
            "--json",
        ],
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["schema_version"] == 1
    assert payload["matrix"] == {
        "harnesses": ["claude"],
        "entry_states": ["startup"],
    }
    assert payload["cells"][0]["compiled"]["execution"]["graph_compiled"] is False


def test_malformed_hook_group_is_recorded_instead_of_crashing(tmp_path: Path) -> None:
    (tmp_path / "AGENTS.md").write_text("# fixture\n", encoding="utf-8")
    hooks = tmp_path / "hooks"
    hooks.mkdir()
    (hooks / "hooks.json").write_text(
        json.dumps({"hooks": {"SessionStart": ["not-an-object"]}}),
        encoding="utf-8",
    )

    report = build_context_report(
        tmp_path,
        harnesses=("claude",),
        entry_states=("startup",),
        packet_budget_bytes=100,
        node_count=1,
    )

    manifest = report["cells"][0]["compiled"]["source_manifest"]
    failed = next(item for item in manifest if item["source_id"] == "hook-manifest")
    assert failed["status"] == "unreadable"
    assert "hooks/hooks.json" in failed["error"]


def test_external_host_counts_only_footnote_managed_instructions(
    tmp_path: Path,
) -> None:
    managed = render_block()
    (tmp_path / "AGENTS.md").write_text(
        "# User-owned rules\n\nNever count these bytes.\n\n" + managed + "\n",
        encoding="utf-8",
    )
    rules = tmp_path / ".claude" / "rules"
    rules.mkdir(parents=True)
    (rules / "user.md").write_text("User-owned progressive rule.\n", encoding="utf-8")
    plugin = tmp_path / ".claude-plugin"
    plugin.mkdir()
    (plugin / "plugin.json").write_text(
        json.dumps({"name": "unrelated-plugin"}),
        encoding="utf-8",
    )

    report = build_context_report(
        tmp_path,
        plugin_root=ROOT,
        harnesses=("claude", "codex"),
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )

    by_harness = {cell["harness"]: cell for cell in report["cells"]}
    claude_manifest = by_harness["claude"]["compiled"]["source_manifest"]
    assert not any(
        item["source_id"] in {"project-instructions", "managed-footnote-block"}
        for item in claude_manifest
    )
    assert not any(
        item["source_id"].startswith("claude-rule:") for item in claude_manifest
    )

    codex_manifest = by_harness["codex"]["compiled"]["source_manifest"]
    managed_source = next(
        item for item in codex_manifest if item["source_id"] == "managed-footnote-block"
    )
    assert managed_source["bytes"] == len(managed.encode())
    for manifest in (claude_manifest, codex_manifest):
        assert not any(item["source_id"] == "project-instructions" for item in manifest)
        assert not any(item["source_id"].startswith("claude-rule:") for item in manifest)
        assert not any(item["source_id"] == "hook-manifest" for item in manifest)


@pytest.mark.parametrize(
    "content",
    [
        "<!-- fno:begin v=1 -->\nunterminated\n",
        "<!-- fno:end -->\n<!-- fno:begin v=1 -->\n",
        render_block() + "\n" + render_block(),
    ],
)
def test_external_malformed_managed_block_is_reported_unreadable(
    tmp_path: Path,
    content: str,
) -> None:
    (tmp_path / "AGENTS.md").write_text(content, encoding="utf-8")

    report = build_context_report(
        tmp_path,
        plugin_root=ROOT,
        harnesses=("codex",),
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )

    manifest = report["cells"][0]["compiled"]["source_manifest"]
    managed = next(
        item for item in manifest if item["source_id"] == "managed-footnote-block"
    )
    assert managed["status"] == "unreadable"
    assert managed["error"] == "malformed Footnote managed block fences"


def test_external_claude_mixed_prose_import_counts_only_managed_block(
    tmp_path: Path,
) -> None:
    managed = render_block()
    (tmp_path / "AGENTS.md").write_text(
        "# User-owned rules\n\n" + managed + "\n",
        encoding="utf-8",
    )
    (tmp_path / "CLAUDE.md").write_text(
        "# Claude-owned prose\n\n@AGENTS.md\n\nMore user prose.\n",
        encoding="utf-8",
    )

    report = build_context_report(
        tmp_path,
        plugin_root=ROOT,
        harnesses=("claude",),
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )

    manifest = report["cells"][0]["compiled"]["source_manifest"]
    native = next(
        item for item in manifest if item["source_id"] == "managed-footnote-block"
    )
    assert native["bytes"] == len(managed.encode())
    assert not any(item["source_id"] == "project-instructions" for item in manifest)


def test_codex_treats_agents_import_syntax_as_literal_instruction_text(
    tmp_path: Path,
) -> None:
    carrier = "@rules.md\n"
    (tmp_path / "AGENTS.md").write_text(carrier, encoding="utf-8")
    (tmp_path / "rules.md").write_text(render_block(), encoding="utf-8")
    plugin = tmp_path / ".claude-plugin"
    plugin.mkdir()
    (plugin / "plugin.json").write_text(
        json.dumps({"name": "fno"}),
        encoding="utf-8",
    )

    report = build_context_report(
        tmp_path,
        plugin_root=ROOT,
        harnesses=("codex",),
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )

    manifest = report["cells"][0]["compiled"]["source_manifest"]
    native = next(
        item for item in manifest if item["source_id"] == "project-instructions"
    )
    assert native["bytes"] == len(carrier.encode())
    assert native["content_hash"] == hashlib.sha256(carrier.encode()).hexdigest()
    assert not any(item["source_id"] == "managed-footnote-block" for item in manifest)


def test_unresolvable_registered_hook_command_is_recorded(tmp_path: Path) -> None:
    (tmp_path / "AGENTS.md").write_text("# fixture\n", encoding="utf-8")
    hooks = tmp_path / "hooks"
    hooks.mkdir()
    (hooks / "hooks.json").write_text(
        json.dumps(
            {
                "hooks": {
                    "SessionStart": [
                        {"hooks": [{"command": "python3 /outside/plugin-hook.py"}]}
                    ]
                }
            }
        ),
        encoding="utf-8",
    )

    report = build_context_report(
        tmp_path,
        harnesses=("claude",),
        entry_states=("startup",),
        packet_budget_bytes=100,
        node_count=1,
    )

    manifest = report["cells"][0]["compiled"]["source_manifest"]
    failed = next(item for item in manifest if item["source_id"].startswith("unresolved-hook"))
    assert failed["status"] == "unreadable"
    assert failed["carrier"] == "python3 /outside/plugin-hook.py"


def test_static_postcompact_inventory_distinguishes_registration_from_delivery() -> None:
    report = build_context_report(
        ROOT,
        harnesses=SUPPORTED_HARNESSES,
        entry_states=("post_compact",),
        packet_budget_bytes=100_000,
        node_count=1,
    )
    by_harness = {cell["harness"]: cell for cell in report["cells"]}

    # The reinject is registered and delivering post-compact on both lanes, but
    # via different carriers: PostCompact on Codex, SessionStart(source=compact)
    # on Claude (PostCompact on Claude is stderr-only and cannot inject). The
    # lifecycle records which carrier each harness registered it under.
    expected_lifecycle = {"claude": "session_start", "codex": "post_compact"}
    for harness in ("claude", "codex"):
        manifest = by_harness[harness]["compiled"]["source_manifest"]
        for source_id in (
            "target-postcompact-reinject",
            "king-postcompact-reinject",
        ):
            source = next(
                item for item in manifest if item["source_id"] == source_id
            )
            assert source["status"] == "registered"
            assert source["lifecycle"] == expected_lifecycle[harness]
            assert source["measurement"] == "carrier_template_bytes"
            assert source["bytes"] == 0
            assert source["content_hash"] is None
            assert source["carrier_bytes"] > 0
    gemini = next(
        item
        for item in by_harness["gemini"]["compiled"]["source_manifest"]
        if item["source_id"] == "target-postcompact-reinject"
    )
    assert gemini["status"] == "omitted"
    assert gemini["error"] == "no_post_compact_registration"
    # Gemini registers no post-compact hook, so EVERY reinject is omitted there
    # - one row each, never one row standing in for the rest. The expected set
    # is derived from the hooks dir (as the audit does), so a third reinject
    # hook cannot be silently missing from the assertion.
    gemini_omitted = [
        item
        for item in by_harness["gemini"]["compiled"]["source_manifest"]
        if item["error"] == "no_post_compact_registration"
    ]
    expected_reinjects = sorted(
        path.name[: -len(".sh")] for path in (ROOT / "hooks").glob("*-postcompact-reinject.sh")
    )
    assert [item["source_id"] for item in gemini_omitted] == expected_reinjects


def test_every_claude_sessionstart_recorder_declares_the_exact_same_inventory() -> None:
    manifest = json.loads((ROOT / "hooks" / "hooks.json").read_text(encoding="utf-8"))
    # Startup recorders live in the matcher="" SessionStart group and share one
    # inventory. A compact-only recorder (the reinject) declares its own, so it
    # is excluded from the startup consensus.
    commands = [
        item["command"]
        for group in manifest["hooks"]["SessionStart"]
        if group.get("matcher", "") == ""
        for item in group["hooks"]
        if "context-observe-hook.sh" in item.get("command", "")
    ]
    source_ids = [
        command.split("--source-id ", 1)[1].split(" ", 1)[0] for command in commands
    ]
    inventories = [
        command.split("--expected ", 1)[1].split(" -- ", 1)[0]
        for command in commands
    ]

    assert len(commands) == 14
    assert len(set(inventories)) == 1
    assert inventories[0].split(",") == source_ids


def test_runtime_observer_emits_exact_session_bound_context_snapshot(
    tmp_path: Path,
) -> None:
    hook_input = tmp_path / "input.json"
    output = tmp_path / "output.json"
    hook_input.write_text(
        json.dumps({"session_id": "codex-session", "source": "startup"}),
        encoding="utf-8",
    )
    delivered = "exact delivered directive\n"
    output.write_text(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": delivered,
                }
            }
        ),
        encoding="utf-8",
    )
    env = {
        **os.environ,
        "FNO_CONTEXT_OBSERVATION_DIR": str(tmp_path / "scratch"),
        "FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS": "30",
        "FNO_PLATFORM": "codex",
    }
    env.pop("FNO_REPO_ROOT", None)
    result = subprocess.run(
        [
            sys.executable,
            str(ROOT / "cli" / "src" / "fno" / "context_observation.py"),
            "direct",
            "--source-id",
            "session-start-combined",
            "--expected",
            "session-start-combined",
            "--carrier",
            "hooks/session-start.sh",
            "--input-file",
            str(hook_input),
            "--output-file",
            str(output),
        ],
        cwd=tmp_path,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    event = json.loads((tmp_path / ".fno" / "events.jsonl").read_text())
    assert event["type"] == "context_snapshot"
    assert event["data"]["session_id"] == "codex-session"
    assert event["data"]["context_bytes"] == len(delivered.encode())
    assert event["data"]["source_manifest"][0]["carrier"] == "hooks/session-start.sh"
    assert event["data"]["measurement_complete"] is True


def test_runtime_snapshot_joins_only_footnote_owned_native_context(
    tmp_path: Path,
) -> None:
    managed = render_block()
    delivered = "hook directive"
    (tmp_path / "AGENTS.md").write_text(
        "# User-owned rules\n\nNever measure this.\n\n" + managed + "\n",
        encoding="utf-8",
    )
    hook_input = tmp_path / "input.json"
    output = tmp_path / "output.json"
    hook_input.write_text(
        json.dumps({"session_id": "joined-session", "source": "startup"}),
        encoding="utf-8",
    )
    output.write_text(json.dumps({"systemMessage": delivered}), encoding="utf-8")
    env = {
        **os.environ,
        "FNO_CONTEXT_OBSERVATION_DIR": str(tmp_path / "scratch"),
        "FNO_PLATFORM": "codex",
        "FNO_REPO_ROOT": str(tmp_path),
    }
    result = subprocess.run(
        [
            sys.executable,
            str(ROOT / "cli" / "src" / "fno" / "context_observation.py"),
            "direct",
            "--source-id",
            "session-start-combined",
            "--expected",
            "session-start-combined",
            "--carrier",
            "hooks/session-start.sh",
            "--input-file",
            str(hook_input),
            "--output-file",
            str(output),
        ],
        cwd=tmp_path,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    event = json.loads((tmp_path / ".fno" / "events.jsonl").read_text())
    manifest = event["data"]["source_manifest"]
    assert [item["source_id"] for item in manifest] == [
        "managed-footnote-block",
        "session-start-combined",
    ]
    expected_bytes = len(managed.encode()) + len(delivered.encode())
    assert event["data"]["context_bytes"] == expected_bytes

    from fno.scoreboard.fold import build_context_outcome_trace

    trace = build_context_outcome_trace(
        {"session_id": "joined-session", "commit_sha": "head"},
        None,
        [event],
    )
    assert trace["context"]["bytes"] == expected_bytes


def test_runtime_observer_upgrades_one_incomplete_snapshot_to_one_complete(
    tmp_path: Path,
) -> None:
    hook_input = tmp_path / "input.json"
    hook_input.write_text(
        json.dumps({"session_id": "late-session", "source": "startup"}),
        encoding="utf-8",
    )
    env = {
        **os.environ,
        "FNO_CONTEXT_OBSERVATION_DIR": str(tmp_path / "scratch"),
        "FNO_PLATFORM": "codex",
        "FNO_REPO_ROOT": str(tmp_path),
    }
    observer = ROOT / "cli" / "src" / "fno" / "context_observation.py"
    for source_id in ("first", "second"):
        output = tmp_path / f"{source_id}.json"
        output.write_text(
            json.dumps({"systemMessage": source_id}),
            encoding="utf-8",
        )
        result = subprocess.run(
            [
                sys.executable,
                str(observer),
                "direct",
                "--source-id",
                source_id,
                "--expected",
                "first,second",
                "--carrier",
                f"hooks/{source_id}.sh",
                "--input-file",
                str(hook_input),
                "--output-file",
                str(output),
            ],
            cwd=tmp_path,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=60,
        )
        assert result.returncode == 0, result.stderr

    events = [
        json.loads(line)
        for line in (tmp_path / ".fno" / "events.jsonl").read_text().splitlines()
    ]
    assert len(events) == 2
    assert events[0]["data"]["measurement_complete"] is False
    assert events[1]["data"]["measurement_complete"] is True


def test_runtime_observer_can_treat_valid_json_as_delivered_directive(
    tmp_path: Path,
) -> None:
    hook_input = tmp_path / "input.json"
    output = tmp_path / "output.json"
    hook_input.write_text(
        json.dumps({"session_id": "gemini-session", "source": "startup"}),
        encoding="utf-8",
    )
    delivered = '{"directive":"keep this exact JSON"}'
    output.write_text(delivered, encoding="utf-8")
    env = {
        **os.environ,
        "FNO_CONTEXT_OBSERVATION_DIR": str(tmp_path / "scratch"),
        "FNO_PLATFORM": "gemini",
        "FNO_REPO_ROOT": str(tmp_path),
    }

    result = subprocess.run(
        [
            sys.executable,
            str(ROOT / "cli" / "src" / "fno" / "context_observation.py"),
            "direct",
            "--source-id",
            "session-start-combined",
            "--expected",
            "session-start-combined",
            "--carrier",
            "hooks/session-start.sh",
            "--input-file",
            str(hook_input),
            "--output-file",
            str(output),
            "--output-is-directive",
        ],
        cwd=tmp_path,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    event = json.loads((tmp_path / ".fno" / "events.jsonl").read_text())
    assert event["data"]["context_bytes"] == len(delivered.encode())
    assert event["data"]["source_manifest"][0]["content_hash"] == hashlib.sha256(
        delivered.encode()
    ).hexdigest()


def test_postcompact_hook_wrapper_preserves_output_and_emits_snapshot(
    tmp_path: Path,
) -> None:
    fixture = tmp_path / "fixture-hook.sh"
    fixture.write_text(
        "#!/usr/bin/env bash\n"
        "printf '%s\\n' '{\"systemMessage\":\"post compact directive\"}'\n",
        encoding="utf-8",
    )
    fixture.chmod(0o755)
    hook_input = json.dumps(
        {"session_id": "claude-session", "hook_event_name": "PostCompact"}
    )
    env = {
        **os.environ,
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "FNO_CONTEXT_OBSERVATION_DIR": str(tmp_path / "scratch"),
        "FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS": "30",
        "FNO_PLATFORM": "codex",
        "FNO_REPO_ROOT": str(tmp_path),
    }
    result = subprocess.run(
        [
            str(ROOT / "hooks" / "context-observe-hook.sh"),
            "--source-id",
            "target-postcompact-reinject",
            "--expected",
            "target-postcompact-reinject",
            "--entry",
            "post_compact",
            "--",
            str(fixture),
        ],
        cwd=tmp_path,
        env=env,
        input=hook_input,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == '{"systemMessage":"post compact directive"}\n'
    event = json.loads((tmp_path / ".fno" / "events.jsonl").read_text())
    assert event["data"]["entry_state"] == "post_compact"
    assert event["data"]["context_bytes"] == len(b"post compact directive")
    assert event["data"]["measurement_complete"] is True


@pytest.mark.parametrize(
    ("platform", "hook_input"),
    [
        pytest.param("claude", '{"source":"compact"}', id="claude-compact"),
        pytest.param("claude", "", id="claude-empty-input"),
        pytest.param("codex", "{}", id="codex"),
    ],
)
def test_postcompact_producer_uses_each_harness_wire_schema(
    tmp_path: Path,
    platform: str,
    hook_input: str,
) -> None:
    plugin = tmp_path / "plugin"
    guard = plugin / "scripts" / "lib" / "target-guard.sh"
    guard.parent.mkdir(parents=True)
    guard.write_text(
        "target_is_active() { return 0; }\n"
        "target_state_field() {\n"
        "  sed -n \"s/^$1: *//p\" \"$2\" | head -1 | tr -d '\\\"'\n"
        "}\n",
        encoding="utf-8",
    )
    # The hook sources its carrier from the plugin's lib dir; a fake plugin that
    # ships the guard but not the carrier would silence the hook (by design).
    shutil.copy(
        ROOT / "scripts" / "lib" / "postcompact-carrier.sh",
        plugin / "scripts" / "lib" / "postcompact-carrier.sh",
    )
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir()
    state.write_text(
        "session_id: wire-session\n"
        'input: "Keep the target oriented"\n'
        "plan_path: null\n"
        "graph_node_id: x-2e3c\n",
        encoding="utf-8",
    )
    # Claude reinjects via SessionStart(source=compact); Codex via PostCompact.
    # Empty input on Claude must retain the Claude carrier because selection is
    # harness-keyed rather than inferred from the event payload.
    hook_env = {
        **os.environ,
        "FNO_PLATFORM": platform,
    }
    if platform == "codex":
        hook_env["PLUGIN_ROOT"] = str(plugin)
        hook_env["CLAUDE_PLUGIN_ROOT"] = str(tmp_path / "foreign-claude-plugin")
    else:
        hook_env["CLAUDE_PLUGIN_ROOT"] = str(plugin)
    result = subprocess.run(
        [str(ROOT / "hooks" / "target-postcompact-reinject.sh")],
        cwd=tmp_path,
        env=hook_env,
        input=hook_input,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    if platform == "codex":
        assert set(payload) == {"systemMessage"}
        assert "Keep the target oriented" in payload["systemMessage"]
    else:
        assert set(payload) == {"hookSpecificOutput"}
        assert payload["hookSpecificOutput"]["hookEventName"] == "SessionStart"
        assert (
            "Keep the target oriented"
            in payload["hookSpecificOutput"]["additionalContext"]
        )


def test_observer_fallbacks_always_preserve_original_output_input_and_exit(
    tmp_path: Path,
) -> None:
    fixture = tmp_path / "original.sh"
    fixture.write_text(
        "#!/usr/bin/env bash\n"
        "/bin/cat\n"
        "printf '%s' '::original'\n"
        "exit 7\n",
        encoding="utf-8",
    )
    fixture.chmod(0o755)
    wrapper = ROOT / "hooks" / "context-observe-hook.sh"
    missing_plugin = tmp_path / "missing-plugin"
    copied_wrapper = missing_plugin / "hooks" / wrapper.name
    copied_wrapper.parent.mkdir(parents=True)
    copied_wrapper.write_text(wrapper.read_text(encoding="utf-8"), encoding="utf-8")
    copied_wrapper.chmod(0o755)
    input_text = "exact stdin\n"
    full_args = [
        "--source-id",
        "fixture",
        "--expected",
        "fixture",
        "--",
        str(fixture),
    ]
    cases = [
        ("missing-expected", wrapper, ["--source-id", "fixture", "--", str(fixture)], {}),
        ("missing-helper", copied_wrapper, full_args, {}),
        (
            "missing-temp",
            wrapper,
            full_args,
            {"TMPDIR": str(tmp_path / "does-not-exist")},
        ),
        (
            "missing-uv",
            wrapper,
            full_args,
            {"PATH": "/bin:/usr/bin"},
        ),
    ]

    for name, selected_wrapper, args, overrides in cases:
        result = subprocess.run(
            [str(selected_wrapper), *args],
            cwd=tmp_path,
            env={**os.environ, **overrides},
            input=input_text,
            text=True,
            capture_output=True,
            check=False,
        )
        assert result.returncode == 7, name
        assert result.stdout == input_text + "::original", name


def _await_marker(path: Path, timeout: float = 15.0) -> bool:
    """Wait for a TERM-handler side effect, bounded.

    The hook returns as soon as it has SENT the signal. The observer's bash trap
    runs `touch` after that, on its own scheduling slice, so an instant assert
    races the handler and flakes under parallel load. This asserts the same
    positive marker and only widens the window it is allowed to arrive in.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            return True
        time.sleep(0.05)
    return path.is_file()


def test_nonreturning_observer_is_killed_without_changing_hook_result(
    tmp_path: Path,
) -> None:
    fixture = tmp_path / "original.sh"
    fixture.write_text(
        "#!/usr/bin/env bash\n"
        "/bin/cat\n"
        "printf '%s' '::original'\n"
        "exit 7\n",
        encoding="utf-8",
    )
    fixture.chmod(0o755)
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    term_marker = tmp_path / "observer-terminated"
    fake_uv = fake_bin / "uv"
    fake_uv.write_text(
        "#!/usr/bin/env bash\n"
        "trap 'touch \"$FNO_OBSERVER_TERM_MARKER\"' TERM\n"
        "sleep 30\n",
        encoding="utf-8",
    )
    fake_uv.chmod(0o755)
    result = subprocess.run(
        [
            str(ROOT / "hooks" / "context-observe-hook.sh"),
            "--source-id",
            "fixture",
            "--expected",
            "fixture",
            "--",
            str(fixture),
        ],
        cwd=tmp_path,
        env={
            **os.environ,
            "PATH": f"{fake_bin}:{os.environ['PATH']}",
            "FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS": "2",
            "FNO_PLATFORM": "codex",
            "FNO_REPO_ROOT": str(tmp_path),
            "FNO_OBSERVER_TERM_MARKER": str(term_marker),
        },
        input='{"session_id":"hung-observer","source":"startup"}',
        text=True,
        capture_output=True,
        check=False,
        timeout=60,
    )

    assert _await_marker(term_marker)
    assert result.returncode == 7
    assert result.stdout == '{"session_id":"hung-observer","source":"startup"}::original'


def test_run_bounded_uses_the_configured_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    observed: dict[str, object] = {}

    class FakeProcess:
        pid = 123

        def wait(self, *, timeout: float) -> int:
            observed["timeout"] = timeout
            return 0

    def fake_popen(command: list[str], *, start_new_session: bool) -> FakeProcess:
        observed["command"] = command
        observed["start_new_session"] = start_new_session
        return FakeProcess()

    monkeypatch.setattr(context_observation.subprocess, "Popen", fake_popen)

    assert (
        context_observation._run_bounded(
            ["--timeout", "0.2", "--", "observer", "--flag"]
        )
        == 0
    )
    assert observed == {
        "command": ["observer", "--flag"],
        "start_new_session": True,
        "timeout": 0.2,
    }


def test_context_observer_threads_the_timeout_override(tmp_path: Path) -> None:
    fixture = tmp_path / "original.sh"
    fixture.write_text(
        "#!/usr/bin/env bash\n"
        "/bin/cat\n"
        "printf '%s' '::original'\n"
        "exit 7\n",
        encoding="utf-8",
    )
    fixture.chmod(0o755)
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    calls = tmp_path / "python-calls"
    fake_python = fake_bin / "python3"
    fake_python.write_text(
        "#!/usr/bin/env bash\n"
        "printf '<%s>' \"$@\" >>\"$FNO_PYTHON_CALLS\"\n"
        "printf '\\n' >>\"$FNO_PYTHON_CALLS\"\n",
        encoding="utf-8",
    )
    fake_python.chmod(0o755)
    fake_uv = fake_bin / "uv"
    fake_uv.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
    fake_uv.chmod(0o755)

    result = subprocess.run(
        [
            str(ROOT / "hooks" / "context-observe-hook.sh"),
            "--source-id",
            "fixture",
            "--expected",
            "fixture",
            "--",
            str(fixture),
        ],
        cwd=tmp_path,
        env={
            **os.environ,
            "PATH": f"{fake_bin}:{os.environ['PATH']}",
            "FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS": "0.2",
            "FNO_PYTHON_CALLS": str(calls),
        },
        input="exact stdin",
        text=True,
        capture_output=True,
        check=False,
        timeout=60,
    )

    assert result.returncode == 7
    assert result.stdout == "exact stdin::original"
    assert "<run-bounded><--timeout><0.2><--><uv>" in calls.read_text(
        encoding="utf-8"
    )


def test_session_start_wire_survives_nonreturning_observer(tmp_path: Path) -> None:
    fast_bin = tmp_path / "fast-bin"
    hung_bin = tmp_path / "hung-bin"
    term_marker = tmp_path / "observer-terminated"
    calls = tmp_path / "python-calls"
    fast_bin.mkdir()
    hung_bin.mkdir()
    for bin_dir, uv_body in (
        (fast_bin, "exit 1"),
        (
            hung_bin,
            "trap 'touch \"$FNO_OBSERVER_TERM_MARKER\"' TERM\nsleep 30",
        ),
    ):
        uv = bin_dir / "uv"
        uv.write_text(f"#!/usr/bin/env bash\n{uv_body}\n", encoding="utf-8")
        uv.chmod(0o755)
        fno = bin_dir / "fno"
        fno.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
        fno.chmod(0o755)
        python = bin_dir / "python3"
        python.write_text(
            "#!/usr/bin/env bash\n"
            "printf '<%s>' \"$@\" >>\"$FNO_PYTHON_CALLS\"\n"
            "printf '\\n' >>\"$FNO_PYTHON_CALLS\"\n"
            "exec \"$FNO_REAL_PYTHON\" \"$@\"\n",
            encoding="utf-8",
        )
        python.chmod(0o755)

    def invoke(bin_dir: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(ROOT / "hooks" / "session-start.sh")],
            cwd=tmp_path,
            env={
                **os.environ,
                "HOME": str(tmp_path / "home"),
                "FNO_HOME": str(tmp_path / "fno-home"),
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
                "FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS": "2",
                "FNO_PLATFORM": "gemini",
                "FNO_REPO_ROOT": str(tmp_path),
                "FNO_OBSERVER_TERM_MARKER": str(term_marker),
                "FNO_PYTHON_CALLS": str(calls),
                "FNO_REAL_PYTHON": sys.executable,
            },
            input='{"session_id":"hung-session-start","source":"startup"}',
            text=True,
            capture_output=True,
            check=False,
            timeout=60,
        )

    baseline = invoke(fast_bin)
    hung = invoke(hung_bin)

    assert baseline.returncode == hung.returncode == 0
    assert _await_marker(term_marker)
    assert "<run-bounded><--timeout><2><--><uv>" in calls.read_text(
        encoding="utf-8"
    )
    assert json.loads(hung.stdout) == json.loads(baseline.stdout)
    assert (
        hung.stdout
        and json.loads(hung.stdout)["hookSpecificOutput"]["additionalContext"]
    )


def test_parallel_recorders_collect_once_after_reversed_completion(
    tmp_path: Path,
) -> None:
    wrapper = ROOT / "hooks" / "context-observe-hook.sh"
    expected = "slow,medium,fast"
    hook_input = json.dumps(
        {
            "session_id": "parallel-session",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "transcript_path": str(tmp_path / "transcript.jsonl"),
        }
    )
    (tmp_path / "transcript.jsonl").write_text("{}\n", encoding="utf-8")
    env = {
        **os.environ,
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "FNO_CONTEXT_OBSERVATION_DIR": str(tmp_path / "scratch"),
        "FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS": "30",
        "FNO_PLATFORM": "claude",
        "FNO_REPO_ROOT": str(tmp_path),
    }
    fixtures = {}
    directives = {"slow": "duplicate", "medium": "duplicate", "fast": "fast"}
    for source_id, delay in (("slow", "0.30"), ("medium", "0.15"), ("fast", "0.00")):
        fixture = tmp_path / f"{source_id}.sh"
        fixture.write_text(
            "#!/usr/bin/env bash\n"
            f"sleep {delay}\n"
            f"printf '%s\\n' '{{\"additionalContext\":\"{directives[source_id]}\"}}'\n",
            encoding="utf-8",
        )
        fixture.chmod(0o755)
        fixtures[source_id] = fixture

    def invoke(source_id: str) -> subprocess.CompletedProcess[str]:
        extra = ["--final"] if source_id == "fast" else []
        return subprocess.run(
            [
                str(wrapper),
                "--source-id",
                source_id,
                "--expected",
                expected,
                *extra,
                "--",
                str(fixtures[source_id]),
            ],
            cwd=tmp_path,
            env=env,
            input=hook_input,
            text=True,
            capture_output=True,
            check=False,
            timeout=60,
        )

    with ThreadPoolExecutor(max_workers=3) as pool:
        results = list(pool.map(invoke, ("fast", "medium", "slow")))

    assert [result.returncode for result in results] == [0, 0, 0]
    assert [json.loads(result.stdout)["additionalContext"] for result in results] == [
        "fast",
        "duplicate",
        "duplicate",
    ]
    events = [
        json.loads(line)
        for line in (tmp_path / ".fno" / "events.jsonl").read_text().splitlines()
    ]
    assert len(events) == 1
    snapshot = events[0]["data"]
    assert snapshot["measurement_complete"] is True
    assert [item["source_id"] for item in snapshot["source_manifest"]] == [
        "slow",
        "medium",
        "fast",
    ]
    assert snapshot["context_bytes"] == len(b"duplicateduplicatefast")
    assert snapshot["source_hashes"][0] == snapshot["source_hashes"][1]


def test_helper_runs_under_a_bare_python_with_no_pythonpath(tmp_path: Path) -> None:
    """The helper is executed BY PATH by context-observe-hook.sh.

    Running a file by path puts its own directory on sys.path, not the package
    root, so ``from fno.harness_identity import ...`` raised ModuleNotFoundError
    under any interpreter without the package installed or on PYTHONPATH. The
    hook ends every helper call with ``>/dev/null 2>&1 || true``, so the record
    step no-opped in silence, collect then found no directory to lock, and no
    snapshot was ever emitted.

    The suite hid this because ``fno test`` exports an ABSOLUTE worktree
    PYTHONPATH that the helper inherited. A relative one does not survive the
    hook's ``cwd`` change, and a real session may have none at all - so this
    strips it rather than trusting the runner's.

    Stripping PYTHONPATH is not enough on its own. Whichever interpreter runs
    this, an editable install of fno sits in its site-packages, so the import
    succeeds with or without the fix and the test proves nothing. Picking
    ``python3`` off PATH only moves the problem: on a box with the venv
    activated - CI included - PATH resolves to that same interpreter, and
    comparing it to ``sys.executable`` by path string does not notice, because
    ``bin/python3`` is a symlink to ``bin/python``.

    ``-S`` is the deterministic cure: no site-packages, so the editable install
    is gone and the only thing that can make ``fno`` importable is the helper's
    own sys.path insert. Same outcome on every machine.
    """
    helper = Path(context_observation.__file__)
    hook_input = tmp_path / "input.json"
    hook_input.write_text(json.dumps({"session_id": "bare-python"}), encoding="utf-8")
    output = tmp_path / "output"
    output.write_text('{"additionalContext":"x"}\n', encoding="utf-8")

    env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}
    env["FNO_CONTEXT_OBSERVATION_DIR"] = str(tmp_path / "scratch")
    env["FNO_REPO_ROOT"] = str(tmp_path)
    # _write_record no-ops on an unrecognized harness, so without this the test
    # would pass on an early return rather than on a successful import.
    env["FNO_PLATFORM"] = "claude"

    # Positive control: under -S with no PYTHONPATH, fno MUST be unimportable.
    # If it is importable anyway the run below cannot fail, so say so instead of
    # reporting a pass that was never at risk.
    probe = subprocess.run([sys.executable, "-S", "-c", "import fno"],
                           env=env, capture_output=True, text=True, timeout=60)
    assert probe.returncode != 0, (
        "fno is importable under -S with no PYTHONPATH, so this test cannot "
        "detect the missing sys.path insert it exists to catch"
    )

    result = subprocess.run(
        [sys.executable, "-S", str(helper), "record",
         "--source-id", "solo", "--expected", "solo", "--carrier", "true",
         "--input-file", str(hook_input), "--output-file", str(output),
         "--hook-rc", "0"],
        cwd=tmp_path, env=env, capture_output=True, text=True, timeout=60,
    )

    assert result.returncode == 0, f"helper failed without PYTHONPATH:\n{result.stderr}"
    assert "ModuleNotFoundError" not in result.stderr
    assert list((tmp_path / "scratch").rglob("*.json")), "record wrote nothing"
