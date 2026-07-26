"""Unit tests for fno.test_cmd smoke discovery.

discover_shell_harnesses() walks the owned shell-harness trees and returns the
standalone harnesses, so a new tests/hooks/test_x.sh runs with zero registry
edits (the two-test-trees lesson: a green subset is not proof everything ran).
It excludes libraries (scripts/lib/), the orchestrated cli/tests/smoke/
subtree, files without a shell shebang, and source-only helpers.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.test_cmd import changed_snapshot, discover_shell_harnesses, select_changed


def _write(path: Path, body: str, *, executable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    if executable:
        path.chmod(0o755)


def test_discovers_new_harness_and_excludes_non_harnesses(tmp_path: Path) -> None:
    root = tmp_path

    # AC4-EDGE: a brand-new harness dropped into an owned tree is discovered
    # with no registry edit (the consolidation's core win).
    _write(root / "tests/zzz-probe.sh", "#!/usr/bin/env bash\necho probe\n", executable=True)
    _write(root / "tests/hooks/test_new_probe.sh",
           "#!/usr/bin/env bash\necho hi\n", executable=True)
    # A harness in scripts/tests/ and one under skills/*/tests/.
    _write(root / "scripts/tests/test_one.sh",
           "#!/usr/bin/env bash\necho one\n", executable=True)
    _write(root / "skills/agent/tests/test_one.sh",
           "#!/usr/bin/env bash\necho skill\n", executable=True)

    # A source-only helper: a sibling sources it -> helper, not a harness.
    _write(root / "tests/hooks/test_helper.sh",
           "#!/usr/bin/env bash\n# sourced by a sibling, never standalone\n",
           executable=True)
    _write(root / "tests/hooks/test_uses_helper.sh",
           "#!/usr/bin/env bash\nsource ./test_helper.sh\necho ok\n", executable=True)
    # A helper sourced via the conventional variable form ($SCRIPT_DIR/...):
    # the resolver falls back to the basename under the sourcer's dir.
    _write(root / "tests/hooks/test_var_helper.sh",
           "#!/usr/bin/env bash\n# sourced via $SCRIPT_DIR\n", executable=True)
    _write(root / "tests/hooks/test_uses_var_helper.sh",
           '#!/usr/bin/env bash\nSCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"\n'
           'source "$SCRIPT_DIR/test_var_helper.sh"\necho ok\n', executable=True)

    # Excluded classes.
    _write(root / "scripts/lib/libthing.sh",       # library, out of owned scope
           "#!/usr/bin/env bash\necho lib\n", executable=True)
    _write(root / "cli/tests/smoke/test_smoke_one.sh",  # orchestrated subtree
           "#!/usr/bin/env bash\necho smoke\n", executable=True)
    _write(root / "tests/hooks/not_shell.sh",       # no shell shebang
           "#!/usr/bin/env python3\nprint('no')\n", executable=True)

    found = set(discover_shell_harnesses(root))

    assert "tests/zzz-probe.sh" in found                  # AC4-EDGE
    assert "tests/hooks/test_new_probe.sh" in found
    assert "scripts/tests/test_one.sh" in found
    assert "skills/agent/tests/test_one.sh" in found
    assert "tests/hooks/test_uses_helper.sh" in found     # real harness that sources a helper
    assert "tests/hooks/test_helper.sh" not in found      # source-only helper excluded
    assert "tests/hooks/test_uses_var_helper.sh" in found  # real harness, variable source ref
    assert "tests/hooks/test_var_helper.sh" not in found   # $SCRIPT_DIR-sourced helper excluded
    assert "scripts/lib/libthing.sh" not in found         # library excluded
    assert "cli/tests/smoke/test_smoke_one.sh" not in found  # orchestrated subtree excluded
    assert "tests/hooks/not_shell.sh" not in found        # non-shell shebang excluded


# --- changed-surface selection (fno test smoke --changed) -------------------


def _repo(root: Path) -> None:
    """Minimal shape select_changed() reads: a cli/tests tree + owned harnesses."""
    _write(root / "cli/src/fno/widget.py", "x = 1\n")
    _write(root / "cli/tests/unit/test_widget.py", "def test_x(): pass\n")
    _write(root / "cli/tests/unit/test_test_cmd.py", "def test_y(): pass\n")
    _write(root / "tests/lib/helper.sh", "#!/usr/bin/env bash\n:\n", executable=True)
    for name in ("test_a", "test_b"):
        _write(root / f"tests/lib/{name}.sh",
               '#!/usr/bin/env bash\nsource "$(dirname "$0")/helper.sh"\n:\n',
               executable=True)


def test_python_source_selects_conventional_test_and_names_the_rule(tmp_path: Path) -> None:
    """AC2: the receipt names the selected test AND the rule that selected it."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/widget.py"])
    assert [(s["rule"], s["target"]) for s in sel] == [
        ("python-source-stem", "cli/tests/unit/test_widget.py")
    ]
    assert unmapped == []


def test_changed_test_file_selects_itself(tmp_path: Path) -> None:
    """AC1: a changed test file is its own selection (no inference needed)."""
    _repo(tmp_path)
    sel, _ = select_changed(tmp_path, ["cli/tests/unit/test_widget.py"])
    assert sel == [{"rule": "test-file-self", "path": "cli/tests/unit/test_widget.py",
                    "kind": "pytest", "target": "cli/tests/unit/test_widget.py"}]


def test_shell_helper_selects_both_owning_harnesses_once(tmp_path: Path) -> None:
    """AC3: reverse source-reference rule, each owner exactly once."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["tests/lib/helper.sh"])
    assert sorted(s["target"] for s in sel) == ["tests/lib/test_a.sh", "tests/lib/test_b.sh"]
    assert {s["rule"] for s in sel} == {"shell-helper-reverse"}
    assert unmapped == []


def test_unknown_path_is_unmapped_and_selects_nothing(tmp_path: Path) -> None:
    """AC4: an unknown path is visible as unmapped, never silently green."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["docs/some-note.md"])
    assert sel == []
    assert unmapped == ["docs/some-note.md"]


def test_python_source_without_a_conventional_test_is_unmapped(tmp_path: Path) -> None:
    """AC4: best-effort inference that finds nothing admits it."""
    _repo(tmp_path)
    _write(tmp_path / "cli/src/fno/lonely.py", "x = 1\n")
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/lonely.py"])
    assert sel == []
    assert unmapped == ["cli/src/fno/lonely.py"]


def test_infra_change_selects_the_selector_contract_tests(tmp_path: Path) -> None:
    """Rule 6: a change to the runner itself falls back to its contract tests."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/test_cmd.py"])
    assert [s["rule"] for s in sel] == ["infra-broad"]
    assert sel[0]["target"] == "cli/tests/unit/test_test_cmd.py"
    assert unmapped == []


def test_duplicate_selection_is_emitted_once(tmp_path: Path) -> None:
    """Two changed paths that map to one test select it once, not twice."""
    _repo(tmp_path)
    sel, _ = select_changed(
        tmp_path, ["cli/src/fno/widget.py", "cli/tests/unit/test_widget.py"])
    assert len(sel) == 1


def _git_repo(root: Path) -> None:
    for args in (("init", "-q"), ("config", "user.email", "t@t.t"),
                 ("config", "user.name", "t")):
        subprocess.run(["git", "-C", str(root), *args], check=True)


def test_snapshot_diffs_explicit_base_and_head(tmp_path: Path) -> None:
    """The CI path diffs the given revisions, not a mutable remote ref."""
    _git_repo(tmp_path)
    _write(tmp_path / "a.txt", "one\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "base"], check=True)
    base = subprocess.run(["git", "-C", str(tmp_path), "rev-parse", "HEAD"],
                          capture_output=True, text=True, check=True).stdout.strip()
    _write(tmp_path / "b.txt", "two\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "head"], check=True)

    paths, reason = changed_snapshot(tmp_path, base, "HEAD")
    assert reason == ""
    assert paths == ["b.txt"]


def test_snapshot_fails_closed_on_an_unresolvable_base(tmp_path: Path) -> None:
    """AC5: a missing base is an explicit unevaluated result, not an empty diff."""
    _git_repo(tmp_path)
    _write(tmp_path / "a.txt", "one\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "base"], check=True)

    paths, reason = changed_snapshot(tmp_path, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "HEAD")
    assert paths == []
    assert "does not resolve" in reason


def test_snapshot_refuses_a_half_specified_range(tmp_path: Path) -> None:
    """AC5: --base without --head cannot silently fall back to local mode."""
    _git_repo(tmp_path)
    paths, reason = changed_snapshot(tmp_path, "HEAD", "")
    assert paths == []
    assert "both --base and --head" in reason


def test_python_source_selects_the_suffixed_test_family(tmp_path: Path) -> None:
    """The repo's real convention: claims.py is covered by test_claims_*.py."""
    _repo(tmp_path)
    _write(tmp_path / "cli/src/fno/claims.py", "x = 1\n")
    _write(tmp_path / "cli/tests/unit/test_claims_core.py", "def test_a(): pass\n")
    _write(tmp_path / "cli/tests/unit/test_claims_io.py", "def test_b(): pass\n")
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/claims.py"])
    assert sorted(s["target"] for s in sel) == [
        "cli/tests/unit/test_claims_core.py", "cli/tests/unit/test_claims_io.py"]
    assert unmapped == []
