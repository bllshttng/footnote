"""migrate_source_field.py rewrites legacy source: 'adopt' -> 'intake' rows.

Read-modify-write atomically; idempotent; --dry-run shows the diff
without mutating the file.
"""
from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "cli" / "scripts" / "migrate_source_field.py"


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _seed_graph(path: Path, entries: list[dict]) -> None:
    path.write_text(json.dumps({"entries": entries}, indent=2))


def _run(graph_path: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(SCRIPT), str(graph_path), *args],
        capture_output=True, text=True,
        cwd=str(REPO_ROOT),
    )


def test_rewrites_adopt_to_intake(tmp_path):
    g = tmp_path / "graph.json"
    _seed_graph(g, [
        {"id": "ab-aaaaaaaa", "source": "adopt", "title": "x"},
        {"id": "ab-bbbbbbbb", "source": "adopt", "title": "y"},
        {"id": "ab-cccccccc", "source": "intake", "title": "z"},
        {"id": "ab-dddddddd", "source": None, "title": "w"},
    ])

    r = _run(g)
    assert r.returncode == 0, r.stderr
    assert "Rewrote 2 node(s)" in r.stdout

    after = json.loads(g.read_text())
    sources = {e["id"]: e.get("source") for e in after["entries"]}
    assert sources["ab-aaaaaaaa"] == "intake"
    assert sources["ab-bbbbbbbb"] == "intake"
    assert sources["ab-cccccccc"] == "intake"  # unchanged
    assert sources["ab-dddddddd"] is None  # unchanged


def test_dry_run_does_not_write(tmp_path):
    g = tmp_path / "graph.json"
    _seed_graph(g, [{"id": "ab-aaaaaaaa", "source": "adopt", "title": "x"}])
    before = _sha256(g)

    r = _run(g, "--dry-run")
    assert r.returncode == 0, r.stderr
    assert "ab-aaaaaaaa" in r.stdout
    assert "would rewrite" in r.stdout.lower() or "[dry-run]" in r.stdout.lower()

    after = _sha256(g)
    assert before == after, "dry-run must not modify the file"


def test_idempotent_second_run(tmp_path):
    g = tmp_path / "graph.json"
    _seed_graph(g, [
        {"id": "ab-aaaaaaaa", "source": "adopt", "title": "x"},
    ])

    r1 = _run(g)
    assert r1.returncode == 0

    r2 = _run(g)
    assert r2.returncode == 0
    assert "Rewrote 0 node(s)" in r2.stdout


def test_missing_file_errors(tmp_path):
    g = tmp_path / "does-not-exist.json"
    r = _run(g)
    assert r.returncode == 1
    assert "not found" in r.stderr


def test_malformed_json_errors(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text("{not valid json")
    r = _run(g)
    assert r.returncode == 1
    assert "could not parse" in r.stderr.lower()


def test_other_fields_unchanged(tmp_path):
    g = tmp_path / "graph.json"
    rich_entry = {
        "id": "ab-rich00001",
        "source": "adopt",
        "title": "rich",
        "project": "fno",
        "cwd": "/home/user/code/fno",
        "blocked_by": ["ab-other0002"],
        "priority": "p1",
        "domain": "code",
        "plan_path": "/path/to/plan",
        "pr_number": 999,
        "completed_at": "2026-04-01T00:00:00Z",
    }
    _seed_graph(g, [rich_entry])

    r = _run(g)
    assert r.returncode == 0, r.stderr

    after = json.loads(g.read_text())
    node = after["entries"][0]
    expected = {**rich_entry, "source": "intake"}
    # Compare only the keys we seeded; the script must not strip or reorder
    # keys it doesn't own.
    for key, value in expected.items():
        assert node.get(key) == value, f"field {key} changed unexpectedly"


def test_empty_graph(tmp_path):
    """Empty entries list is valid input; reports zero work."""
    g = tmp_path / "graph.json"
    _seed_graph(g, [])
    r = _run(g)
    assert r.returncode == 0, r.stderr
    assert "Rewrote 0 node(s)" in r.stdout
