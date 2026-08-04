from __future__ import annotations

import subprocess
from pathlib import Path

import yaml

from fno.approvals.models import classify_effect
from fno.plugins.verify import verify_pack

REPO_ROOT = Path(__file__).resolve().parents[3]
PACK = REPO_ROOT / "plugins" / "growth-studio" / "plugin.yaml"
PACK_DIR = REPO_ROOT / "plugins" / "growth-studio"

# The bounded tool allowlist a packaged agent may hold. No ceiling grants a
# network, shell, or delegation tool.
ALLOWED_TOOLS = {"Read", "Write", "Edit", "Glob", "Grep"}
FORBIDDEN_TOOLS = {"Bash", "Task", "WebSearch", "WebFetch"}
AGENT_GLOBS = (
    PACK_DIR / "agents",
    REPO_ROOT / "agents",
)


def _agent_files() -> list[Path]:
    files: list[Path] = []
    for base in AGENT_GLOBS:
        if base.is_dir():
            files.extend(base.glob("growth-*.md"))
    return files


def _frontmatter_tools(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---"):
        return []
    lines = text.splitlines()
    end = next((i for i in range(1, len(lines)) if lines[i].strip() == "---"), None)
    if end is None:
        return []
    block = yaml.safe_load("\n".join(lines[1:end])) or {}
    tools = block.get("tools", [])
    return [str(t) for t in tools]


# AC14-INV: the resolution core is byte-identical to main, and the function-
# agnostic gate still passes.


def test_resolver_core_byte_identical_to_main() -> None:
    result = subprocess.run(
        ["git", "diff", "--exit-code", "origin/main", "--",
         "cli/src/fno/roles/registry.py", "cli/src/fno/roles/resolver.py"],
        cwd=REPO_ROOT,
        capture_output=True,
    )
    assert result.returncode == 0, "resolver core differs from main:\n" + result.stdout.decode()


def test_plugins_function_agnostic_gate_passes() -> None:
    result = subprocess.run(
        ["bash", str(REPO_ROOT / "scripts" / "ci" / "check-plugins-function-agnostic.sh")],
        cwd=REPO_ROOT,
        capture_output=True,
    )
    assert result.returncode == 0, result.stderr.decode()


# AC13-SEC: no packaged agent holds a network, shell, or delegation tool, and a
# declared publication effect still requires approval. The tool-list check is
# scoped to each agent's `tools:` field (not its disallowedTools refusal list).


def test_every_packaged_agent_tool_list_is_bounded() -> None:
    agents = _agent_files()
    assert agents, "expected bundled + pack agent files to exist"
    all_tools: set[str] = set()
    for agent in agents:
        tools = set(_frontmatter_tools(agent))
        # Positive control: the parse found a real allowed tool, so an empty
        # forbidden-set is a real result, not a missed search.
        assert tools & ALLOWED_TOOLS, f"no allowed tools parsed from {agent.name}"
        offenders = tools & FORBIDDEN_TOOLS
        assert not offenders, f"{agent.name} tools include forbidden {offenders}"
        offenders_mcp = {t for t in tools if t.startswith("mcp__")}
        assert not offenders_mcp, f"{agent.name} tools include MCP {offenders_mcp}"
        all_tools |= tools
    # Positive control at the sweep level: at least one allowed tool is present
    # across the agent set, proving the search reached the files.
    assert all_tools & ALLOWED_TOOLS


def test_publication_effect_still_requires_approval() -> None:
    assert classify_effect("external.publication").value == "require_approval"


# The faucet surface verifies clean: agents, skill, and runnable evaluators all
# declared and checked.


def test_faucet_pack_verifies_all_checked_and_passed() -> None:
    report = verify_pack(PACK, installed={})
    assert report.ok, [c.name for c in report.conditions if not c.ok]
    names = {c.name for c in report.conditions}
    for agent in ("growth-marketer", "growth-comms", "growth-designer", "growth-social"):
        assert f"source:agent:{agent}" in names
        assert f"agent-role-binding:{agent}" in names
        assert f"agent-tools-bounded:{agent}" in names
    assert "source:skill:growth-launch" in names
    for evaluator in ("factual-review", "brand-review", "accessibility-review"):
        assert f"evaluator-runnable:{evaluator}" in names


# AC8 re-confirmed at the faucet level: the evaluators run and a cited draft
# passes while an uncited one fails.


def test_evaluators_run_over_a_sample_draft(tmp_path: Path) -> None:
    truth = PACK_DIR / "assets" / "product-truth.md"
    good = tmp_path / "good.md"
    good.write_text(
        "# Plan\n\nA cited plan.\n\n## Claims\n\n"
        "- Five phases in one graph [Delivery pipeline].\n",
        encoding="utf-8",
    )
    bad = tmp_path / "bad.md"
    bad.write_text("# Plan\n\n## Claims\n\n- An uncited claim.\n", encoding="utf-8")

    def run(script: str, draft: Path, *args: str) -> int:
        return subprocess.run(
            ["bash", str(PACK_DIR / "evaluators" / script), str(draft), *map(str, args)],
            capture_output=True,
        ).returncode

    assert run("factual-check.sh", good, truth) == 0
    assert run("factual-check.sh", bad, truth) != 0
    assert run("brand-check.sh", good) == 0
