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
    assert len(files) == len(list((REPO_ROOT / "agents").glob("*.md")))
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
