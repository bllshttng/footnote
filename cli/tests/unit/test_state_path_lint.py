from __future__ import annotations

from pathlib import Path

from fno.lint_cli import (
    _read_state_roots_baseline,
    _state_root_path_violations,
    _state_roots_findings,
)

REPO_ROOT = Path(__file__).resolve().parents[3]


def test_state_path_lint_catches_a_known_bad_fixture_line(tmp_path: Path):
    source = tmp_path / "cli" / "src" / "fno" / "bad.py"
    source.parent.mkdir(parents=True)
    source.write_text(
        "from pathlib import Path\n\n"
        "def bad_state_reader(root):\n"
        "    return root / '.fno' / 'graph.json'\n",
        encoding="utf-8",
    )

    violations = _state_root_path_violations(tmp_path)

    assert len(violations) == 1
    rel, filename, message = violations[0]
    assert rel == "cli/src/fno/bad.py"
    assert filename == "graph.json"
    assert "cli/src/fno/bad.py:4" in message
    assert "fno.paths.graph_json" in message


def test_state_path_lint_matches_the_committed_baseline():
    findings = _state_roots_findings(REPO_ROOT)
    baseline = _read_state_roots_baseline(REPO_ROOT)

    assert set(findings) == baseline
