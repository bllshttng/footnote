"""Read-only context census and compiler contracts for x-2e3c Task 1.1."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner


def _space_journal(tmp_path: Path) -> Path:
    """The event journal the spawned child writes: the repo's space, pinned
    by the conftest FNO_SPACES_DIR sandbox, keyed on the child's cwd."""
    from fno.paths import space_dir

    return space_dir(tmp_path) / "events.jsonl"


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
    # The claude census (which now expands hooks/context-run.sh groups through
    # hooks/context-hooks.json) must name every startup producer of the
    # claude-session-start group. Compact-only producers are excluded: they
    # register under matcher="compact", not startup.
    declaration = json.loads(
        (ROOT / "hooks" / "context-hooks.json").read_text(encoding="utf-8")
    )
    producers = declaration["groups"]["claude-session-start"]["producers"]
    expected_ids = [
        item["id"]
        for item in producers
        if "compact" not in (item.get("sources") or [])
    ]
    report = build_context_report(
        ROOT,
        harnesses=("claude",),
        entry_states=("startup",),
        packet_budget_bytes=100_000,
        node_count=1,
    )
    manifest = report["cells"][0]["compiled"]["source_manifest"]
    session_start_ids = {
        item["source_id"]
        for item in manifest
        if item["lifecycle"] == "session_start"
    }
    for producer_id in expected_ids:
        assert producer_id in session_start_ids


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


