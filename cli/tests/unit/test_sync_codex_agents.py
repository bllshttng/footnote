from __future__ import annotations

import importlib.util
import subprocess
import sys
import tomllib
from pathlib import Path
from types import ModuleType

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
GENERATOR_PATH = REPO_ROOT / "scripts" / "sync-codex-agents.py"


@pytest.fixture(scope="module")
def sync_codex_agents() -> ModuleType:
    spec = importlib.util.spec_from_file_location("sync_codex_agents", GENERATOR_PATH)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def generate_fixture(
    tmp_path: Path, sync_codex_agents: ModuleType, frontmatter: str, body: str
) -> tuple[str, dict[str, object]]:
    source = tmp_path / "fixture.md"
    source.write_text(f"---\n{frontmatter.rstrip()}\n---\n\n{body}", encoding="utf-8")
    generated = sync_codex_agents.generated_toml(source)
    return generated, tomllib.loads(generated)


def test_codex_agents_are_generated_and_parse() -> None:
    res = subprocess.run(
        [sys.executable, "scripts/sync-codex-agents.py", "--check"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
    )
    assert res.returncode == 0, res.stderr

    files = sorted((REPO_ROOT / ".codex" / "agents").glob("*.toml"))
    names = {path.stem for path in files}

    def _has_pack(path: Path) -> bool:
        text = path.read_text(encoding="utf-8")
        block = text.split("---\n", 1)[1].split("\n---", 1)[0] if text.startswith("---\n") else ""
        return any(line.strip().startswith("pack:") for line in block.splitlines())

    agent_mds = sorted((REPO_ROOT / "agents").glob("*.md"))
    non_pack = [path for path in agent_mds if not _has_pack(path)]
    # Every non-pack agent is registered on codex; pack-contributed agents are a
    # Claude-plugin-skill surface (their bounded-tool allowlist is enforced by
    # the Claude harness, not expressible in codex's coarse sandbox), so they
    # are deliberately NOT registered on the codex harness.
    assert names == {path.stem for path in non_pack}
    assert "growth-marketer" not in names
    for path in files:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        assert data["name"] == path.stem
        assert data["description"]
        assert len(data["description"]) <= 360
        assert "<example>" not in data["description"]
        assert "\\n" not in data["description"]
        assert data["developer_instructions"]
        assert data["sandbox_mode"] in {"read-only", "workspace-write"}


def test_claude_only_fields_degrade_to_codex_context(
    tmp_path: Path, sync_codex_agents: ModuleType
) -> None:
    generated, data = generate_fixture(
        tmp_path,
        sync_codex_agents,
        """\
name: fixture
description: Claude-specific fixture
model: sonnet
tools: [Read, Grep]
skills: [fno:think, fno:review]
disallowedTools: [Write, Bash]""",
        'Inspect the input, including a TOML-sensitive """ marker and \\ path.',
    )

    assert "model =" not in generated
    assert data["sandbox_mode"] == "read-only"
    assert data["developer_instructions"] == (
        'Inspect the input, including a TOML-sensitive """ marker and \\ path.\n\n'
        "## Source Skills\n\n"
        "- fno:think\n"
        "- fno:review\n\n"
        "## Disallowed Source Tools\n\n"
        "- Write\n"
        "- Bash\n"
    )


def test_codex_model_and_write_capable_tools_are_preserved(
    tmp_path: Path, sync_codex_agents: ModuleType
) -> None:
    _generated, data = generate_fixture(
        tmp_path,
        sync_codex_agents,
        """\
name: codex-worker
description: Codex fixture
model: gpt-5.1-codex
tools: [Read, Bash]""",
        "Implement and verify the change.",
    )

    assert data["model"] == "gpt-5.1-codex"
    assert data["sandbox_mode"] == "workspace-write"


def test_explicit_codex_fields_override_inferred_defaults(
    tmp_path: Path, sync_codex_agents: ModuleType
) -> None:
    _generated, data = generate_fixture(
        tmp_path,
        sync_codex_agents,
        """\
name: custom-worker
description: Explicit Codex fields
tools: [Read]
sandbox_mode: workspace-write
nickname_candidates: [custom, worker]""",
        "Follow the explicit Codex configuration.",
    )

    assert data["sandbox_mode"] == "workspace-write"
    assert data["nickname_candidates"] == ["custom", "worker"]


def test_chomped_block_scalar_is_read_as_its_body(
    tmp_path: Path, sync_codex_agents: ModuleType
) -> None:
    """`|-` is a valid header, and reading it as a value is the whole bug.

    The parser matched the bare markers `|` and `>` only, so `description: |-`
    parsed as the literal string "|-" with the indented body appended as loose
    continuation lines. That put an example block into a codex description and
    broke the contract forbidding it. The repaired agent files use `|-`,
    because that is what makes their round trip byte-exact.
    """
    _generated, data = generate_fixture(
        tmp_path,
        sync_codex_agents,
        "name: fixture\ndescription: |-\n  A short pointer sentence.\nmodel: haiku",
        "Body.\n",
    )

    assert data["description"] == "A short pointer sentence."


def test_folded_scalar_keeps_paragraph_breaks(
    tmp_path: Path, sync_codex_agents: ModuleType
) -> None:
    """YAML folds a line break to a space and a BLANK line to a newline.

    Joining every line with " " deletes each paragraph break and leaves a
    doubled space where one was. Asserted against PyYAML on the PARSER, not on
    the generated toml: codex_description collapses all whitespace downstream,
    so a test at that layer passes either way and proves nothing.
    """
    import pathlib

    import yaml

    raw = (
        "name: fixture\n"
        "description: >\n"
        "  first line\n"
        "  same paragraph\n"
        "\n"
        "  second paragraph\n"
        "model: haiku"
    )

    parsed = sync_codex_agents.parse_agent_frontmatter(raw, pathlib.Path("fixture.md"))

    assert parsed["description"] == "first line same paragraph\nsecond paragraph"
    assert "  " not in parsed["description"], (
        "a dropped blank line leaves a doubled space"
    )
    # The reference reader, modulo the trailing newline clip chomping keeps.
    assert parsed["description"] == yaml.safe_load(raw)["description"].rstrip("\n")
