from __future__ import annotations

import subprocess
import sys
import tomllib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]


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
