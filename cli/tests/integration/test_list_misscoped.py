"""list_misscoped_graph_nodes.py read-only diagnostic.

Lists graph nodes whose cwd does not match the settings.yaml workspace
path declared for their project. The script must be read-only - it must
not mutate ~/.fno/graph.json.
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "cli" / "scripts" / "list_misscoped_graph_nodes.py"


def _setup_fake_home(tmp_path: Path, settings_yaml: str, graph: dict) -> dict[str, str]:
    home = tmp_path / "home"
    home.mkdir()
    fno = home / ".fno"
    fno.mkdir()
    (fno / "settings.yaml").write_text(settings_yaml)
    (fno / "graph.json").write_text(json.dumps(graph))
    return {"HOME": str(home)}


def test_list_misscoped_outputs_markdown_table(tmp_path, monkeypatch):
    home = tmp_path / "home"
    home.mkdir()
    foo_path = str(tmp_path / "code" / "foo")
    bar_path = str(tmp_path / "code" / "bar")
    settings = (
        "work:\n"
        "  workspaces:\n"
        "    main:\n"
        "      projects:\n"
        f"        - name: foo\n          path: {foo_path}\n"
        f"        - name: bar\n          path: {bar_path}\n"
    )
    bad_node = {
        "id": "ab-bad00001",
        "project": "bar",
        "cwd": foo_path,
        "title": "x", "type": "feature",
    }
    good_node = {
        "id": "ab-good0002",
        "project": "foo",
        "cwd": foo_path,
        "title": "y", "type": "feature",
    }
    graph = {"entries": [bad_node, good_node]}
    fno = home / ".fno"
    fno.mkdir()
    (fno / "settings.yaml").write_text(settings)
    (fno / "graph.json").write_text(json.dumps(graph))
    monkeypatch.setenv("HOME", str(home))

    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        capture_output=True, text=True,
        env={**__import__("os").environ, "HOME": str(home)},
        cwd=str(tmp_path),
    )
    assert result.returncode == 0, result.stderr
    assert "ab-bad00001" in result.stdout
    assert "ab-good0002" not in result.stdout

    g_after = json.loads((fno / "graph.json").read_text())
    assert g_after == graph


def test_list_misscoped_no_candidates(tmp_path, monkeypatch):
    """When no node is misscoped, the script reports cleanly with exit 0."""
    home = tmp_path / "home"
    home.mkdir()
    foo_path = str(tmp_path / "code" / "foo")
    settings = (
        "work:\n"
        "  workspaces:\n"
        "    main:\n"
        "      projects:\n"
        f"        - name: foo\n          path: {foo_path}\n"
    )
    graph = {"entries": [
        {
            "id": "ab-clean0001", "project": "foo",
            "cwd": foo_path, "title": "x", "type": "feature",
        }
    ]}
    fno = home / ".fno"
    fno.mkdir()
    (fno / "settings.yaml").write_text(settings)
    (fno / "graph.json").write_text(json.dumps(graph))
    monkeypatch.setenv("HOME", str(home))

    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        capture_output=True, text=True,
        env={**__import__("os").environ, "HOME": str(home)},
        cwd=str(tmp_path),
    )
    assert result.returncode == 0, result.stderr
    assert "No misscoped nodes" in result.stdout


def test_list_misscoped_supports_legacy_flat_projects_schema(tmp_path, monkeypatch):
    """The diagnostic must walk legacy `work.projects.<name>` schema too."""
    home = tmp_path / "home"
    home.mkdir()
    foo_path = str(tmp_path / "code" / "foo")
    bar_path = str(tmp_path / "code" / "bar")
    settings = (
        "work:\n"
        "  projects:\n"
        f"    foo:\n      path: {foo_path}\n"
        f"    bar:\n      path: {bar_path}\n"
    )
    bad_node = {
        "id": "ab-legbad001",
        "project": "bar",
        "cwd": foo_path,
        "title": "x", "type": "feature",
    }
    graph = {"entries": [bad_node]}
    fno = home / ".fno"
    fno.mkdir()
    (fno / "settings.yaml").write_text(settings)
    (fno / "graph.json").write_text(json.dumps(graph))
    monkeypatch.setenv("HOME", str(home))

    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        capture_output=True, text=True,
        env={**__import__("os").environ, "HOME": str(home)},
        cwd=str(tmp_path),
    )
    assert result.returncode == 0, result.stderr
    assert "ab-legbad001" in result.stdout
    # The expected_project should resolve from the legacy schema
    assert "foo" in result.stdout
