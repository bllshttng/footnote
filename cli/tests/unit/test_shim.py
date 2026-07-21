"""Tests for the scripts/roadmap-tasks.py compatibility shim (#24).

The shim has two distinct error paths:
- cli/src directory missing (covered by FOLLOW-UPS #11).
- cli/src present but fno.graph package unimportable.

#24 covers the second branch. Both must surface rc=3 with their own
diagnostic, and a fix to one path must not regress the other.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path


SHIM_PATH = Path(__file__).resolve().parents[3] / "scripts" / "roadmap-tasks.py"


def test_shim_import_error_exits_3(tmp_path: Path) -> None:
    """ImportError branch: cli/src exists but fno.graph is not importable.

    Reproduce: copy the shim into a tmpdir layout where parents[1] has a
    cli/src/ directory. Plant a STUB fno package whose __init__.py
    raises ImportError; because the shim does sys.path.insert(0, cli/src)
    this stub wins precedence over any fno installed in the test
    runner's venv. The shim's `from fno.graph.cli import cli as
    app` then trips the ImportError branch and exits 3, reporting the
    underlying import failure.

    Without this stub the test runner's venv (which has fno
    installed) would satisfy the import and the ImportError branch would
    never fire.
    """
    fake_repo = tmp_path / "fake-repo"
    scripts_dir = fake_repo / "scripts"
    cli_src = fake_repo / "cli" / "src"
    scripts_dir.mkdir(parents=True)
    cli_src.mkdir(parents=True)
    fake_shim = scripts_dir / "roadmap-tasks.py"
    shutil.copy2(SHIM_PATH, fake_shim)

    # Stub fno package: __init__.py raises ImportError on import.
    # The shim's sys.path.insert(0, cli/src) puts this ahead of the
    # runner's venv site-packages, so this stub wins the import race.
    abilities_pkg = cli_src / "fno"
    abilities_pkg.mkdir()
    (abilities_pkg / "__init__.py").write_text(
        'raise ImportError("forced for #24 ImportError-branch test")\n'
    )

    result = subprocess.run(
        [sys.executable, str(fake_shim)],
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert result.returncode == 3, (
        f"expected rc=3 on ImportError branch, got rc={result.returncode}; "
        f"stderr={result.stderr!r}"
    )
    assert "cannot import fno.graph.cli" in result.stderr, (
        f"stderr should carry the ImportError-specific message, got: {result.stderr!r}"
    )
    # The underlying error must survive into the message. Reporting only
    # "install the CLI" sends the operator to reinstall something that is
    # already installed when the real cause is a missing dependency under
    # whichever interpreter ran the shim.
    assert "forced for #24 ImportError-branch test" in result.stderr, (
        f"stderr must name the underlying import failure; got: {result.stderr!r}"
    )
    # The other branch's message must NOT appear; otherwise #11's coverage
    # has regressed and the two error paths have collapsed.
    assert "fno CLI shim broken" not in result.stderr, (
        f"ImportError branch must not emit the cli/src-missing diagnostic; "
        f"stderr={result.stderr!r}"
    )


def test_shim_import_error_distinct_from_missing_clisrc(tmp_path: Path) -> None:
    """Cross-check (#24 + #11): the cli/src-missing branch emits a
    different diagnostic from the ImportError branch. Both exit 3, but
    the messages must remain distinguishable so operators can act.
    """
    # Layout: parents[1] has NO cli/src directory at all.
    fake_repo = tmp_path / "fake-repo"
    scripts_dir = fake_repo / "scripts"
    scripts_dir.mkdir(parents=True)
    fake_shim = scripts_dir / "roadmap-tasks.py"
    shutil.copy2(SHIM_PATH, fake_shim)

    result = subprocess.run(
        [sys.executable, str(fake_shim)],
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert result.returncode == 3
    assert "fno CLI shim broken" in result.stderr, (
        f"missing-cli/src branch should emit the shim-broken diagnostic, "
        f"got: {result.stderr!r}"
    )
    # The ImportError-branch message must NOT appear.
    assert "cannot import fno.graph.cli" not in result.stderr, (
        f"missing-cli/src branch must not emit the ImportError message; "
        f"stderr={result.stderr!r}"
    )
