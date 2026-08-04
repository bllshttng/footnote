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
    assert "fno dispatch resolve" in source


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

    assert "fno dispatch resolve" in dispatch_node
    # `fno dispatch resolve` alone answers "which harness is configured", never
    # "does it have quota left" - so asserting only that string let the shell
    # dispatcher route around the quota seam while this census stayed green.
    # Pin the quota rung and the carrier that makes a cutover reach the spawn.
    assert "dispatch resolve --autonomous" in dispatch_node
    assert "--dispatch-account" in dispatch_node
    assert 'route_action" == "defer"' in dispatch_node
    assert 'DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"' in blueprint
    assert '"backlog",\n                "advance",' in active_backlog
    assert "harness_map.resolve_dispatch(**resolve_kwargs)" in advance
    assert "resolved = resolve_dispatch(" in context_think
    assert 'verb="/think"' in context_think
    assert 'trigger="autonomous"' in context_think

    # Operator spawns intentionally keep their own attended defaults, but pane
    # permission still comes from the same harness capability table.
    assert "capabilities(provider).get(\"route_on_pane\", False)" in attended_spawn


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
