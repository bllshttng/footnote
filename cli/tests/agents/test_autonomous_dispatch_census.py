"""Regression census for autonomous dispatch routing ownership."""
from __future__ import annotations

import re
import shutil
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]


def _read_with_positive_control(relative: str, marker: str) -> str:
    """Read one owned path only after an anchored ripgrep control finds it."""
    source = (ROOT / relative).read_text(encoding="utf-8")
    rg = shutil.which("rg")
    if rg is None:
        assert marker in source, f"positive control {marker!r} missing from {relative}"
        return source
    found = subprocess.run(
        [rg, "-n", "--glob", "!/target/**", marker, relative],
        cwd=ROOT,
        text=True,
        capture_output=True,
    )
    assert found.returncode == 0, (
        f"positive control {marker!r} missing from {relative}: {found.stderr}"
    )
    return source


def test_positive_control_falls_back_without_ripgrep(monkeypatch) -> None:
    monkeypatch.setattr(shutil, "which", lambda name: None)
    source = _read_with_positive_control(
        "skills/target/scripts/dispatch-node.sh",
        "dispatch-node.sh",
    )
    assert "fno agents dispatch resolve" in source


def test_all_autonomous_entry_points_reach_an_owned_routing_seam() -> None:
    dispatch_node = _read_with_positive_control(
        "skills/target/scripts/dispatch-node.sh",
        "dispatch-node.sh",
    )
    blueprint = _read_with_positive_control(
        "skills/blueprint/scripts/autolaunch-on-ready.sh",
        "autolaunch-on-ready.sh",
    )
    active_backlog = _read_with_positive_control(
        "crates/fno-agents/src/active_backlog.rs",
        "dispatch_mission",
    )
    advance = _read_with_positive_control(
        "cli/src/fno/backlog/advance.py",
        "def _spawn_worker",
    )
    context_think = _read_with_positive_control(
        "cli/src/fno/provenance/spawn_think.py",
        "def _spawn_think_worker",
    )
    attended_spawn = _read_with_positive_control(
        "cli/src/fno/agents/cli.py",
        "def cmd_spawn",
    )

    assert "fno agents dispatch resolve" in dispatch_node
    # `fno agents dispatch resolve` alone answers "which harness is configured", never
    # "does it have quota left" - so asserting only that string let the shell
    # dispatcher route around the quota seam while this census stayed green.
    # Pin the quota rung and the carrier that makes a cutover reach the spawn.
    assert "dispatch resolve --autonomous" in dispatch_node
    assert "--dispatch-account" in dispatch_node
    assert 'route_action" == "defer"' in dispatch_node
    # The cutover spawn MUST pin the Python runtime. `spawn` auto-routes to the
    # Rust client, which does not know --dispatch-account and rejects the whole
    # launch ("fno-agents: unknown flag"), so the flag alone is not enough - the
    # earlier version of this census asserted only that the flag appeared.
    assert "spawn_runtime=(env FNO_AGENTS_RUNTIME=python)" in dispatch_node
    pin_at = dispatch_node.index("spawn_runtime=(env FNO_AGENTS_RUNTIME=python)")
    flag_at = dispatch_node.index("--dispatch-account")
    assert flag_at < pin_at, "the runtime pin must be set alongside the flag"
    for spawn in re.findall(r"spawn_out=\"\$\((.*?) fno agents spawn", dispatch_node):
        assert "spawn_runtime" in spawn, f"spawn site not runtime-pinnable: {spawn}"
    assert 'DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"' in blueprint
    assert '"backlog",\n                "advance",' in active_backlog
    assert "harness_map.resolve_dispatch(**resolve_kwargs)" in advance
    assert "resolved = resolve_dispatch(" in context_think
    assert 'verb="/think"' in context_think
    assert 'trigger="autonomous"' in context_think

    # Operator spawns intentionally keep their own attended defaults, but pane
    # permission still comes from the same harness capability table. The reader
    # is the posture one (x-f579): an undeclared harness answers
    # route_on_pane=False instead of raising, so the seam stays a refusal.
    assert (
        "capabilities_or_undeclared(harness).get(\"route_on_pane\", False)"
        in attended_spawn
    )


def _substrate_warning_block(dispatch_node: str) -> str:
    """The `Loud, once` fallback block, extracted so its guard and payload
    are asserted together rather than as loose source substrings."""
    match = re.search(
        r"# Loud, once:.*?\nif \[\[.*?\nfi\n", dispatch_node, re.DOTALL
    )
    assert match is not None, "substrate warning block not found in dispatch-node.sh"
    return match.group(0)


def test_substrate_warning_fires_only_on_the_one_shot_lane() -> None:
    """A thread-resolving dispatch prints nothing and emits no
    dispatch_substrate_fallback row (AC1/AC9); the guard reads the resolved
    substrate's meaning, never a spelling that can drift a release."""
    dispatch_node = _read_with_positive_control(
        "skills/target/scripts/dispatch-node.sh",
        "dispatch-node.sh",
    )
    block = _substrate_warning_block(dispatch_node)
    assert '[[ "$DISPATCH_SUBSTRATE" == "headless" ]]' in block
    assert '"bg"' not in block
    assert "has no bg substrate" not in block


def test_substrate_warning_and_event_name_the_resolved_value() -> None:
    """The message names the resolved substrate instead of asserting one; the
    event's `from` is the value actually resolved from, `to` the one-shot lane
    (AC2)."""
    dispatch_node = _read_with_positive_control(
        "skills/target/scripts/dispatch-node.sh",
        "dispatch-node.sh",
    )
    block = _substrate_warning_block(dispatch_node)
    assert "$DISPATCH_SUBSTRATE" in block
    assert '"from\\":\\"$DISPATCH_SUBSTRATE\\"' in block
    assert '"to\\":\\"headless\\"' in block


def test_resolver_failure_refusal_carries_no_substrate_assertions() -> None:
    """The resolver-failure refusal names the config key and no dead substrate
    spelling or per-harness mapping (AC3): the resolver itself reports what
    each harness supports."""
    dispatch_node = _read_with_positive_control(
        "skills/target/scripts/dispatch-node.sh",
        "dispatch-node.sh",
    )
    refusal = next(
        line
        for line in dispatch_node.splitlines()
        if "no autonomous substrate resolved" in line
    )
    assert "config.dispatch.harness" in refusal
    assert "bg" not in refusal
    assert "=headless" not in refusal


def test_dispatch_harness_registry_entry_carries_its_migration() -> None:
    """The registry description of the deprecated key must teach the migration
    itself, matching the sibling `dispatch.auto_merge` entry's shape (AC8)."""
    registry = _read_with_positive_control(
        "cli/src/fno/config/registry.py",
        '"dispatch.auto_merge"',
    )
    entry = next(
        line for line in registry.splitlines() if '"dispatch.harness"' in line
    )
    assert "DEPRECATED" in entry
    assert "fno config set agents.profiles.target.provider" in entry


def test_context_think_legacy_substrate_is_compatibility_only() -> None:
    config = _read_with_positive_control(
        "cli/src/fno/config/__init__.py",
        "class ThinkSpawnBlock",
    )
    registry = _read_with_positive_control(
        "cli/src/fno/config/registry.py",
        "think_spawn.enabled",
    )
    guide = _read_with_positive_control(
        "docs/configuration-guide.md",
        "think_spawn.enabled",
    )

    block = re.search(
        r"class ThinkSpawnBlock\(BaseModel\):(.*?)\nclass ",
        config,
        re.DOTALL,
    )
    assert block is not None
    assert "substrate: Optional[str] = None" in block.group(1)
    assert '"think_spawn.substrate"' in registry
    assert "deprecated compatibility fallback" in registry.lower()
    assert "deprecated compatibility fallback" in guide.lower()
