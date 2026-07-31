#!/usr/bin/env python3
"""Tests for the broken-symlink branch of scripts/metrics/merge-stray-ledger.py.

``Path.exists()`` FOLLOWS symlinks, so a dangling stray - or one pointing at
itself (ELOOP) - is indistinguishable from "absent". The tool used to take its
"no stray ledger; nothing to do." branch on exactly the breakage it exists to
repair, which is how a self-referencing canonical ledger link survived
unreported. These tests pin both directions: it must report-and-remove a broken
link, and must still stay quiet when the stray is genuinely absent.

Each test runs the script as a subprocess with HOME pointed at a sandbox, so
the real ~/.fno ledger is never touched.

Run: python3 tests/metrics/test_merge_stray_ledger_broken_link.py
 OR: cd cli && uv run pytest ../tests/metrics/test_merge_stray_ledger_broken_link.py -q
"""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
MERGE = REPO_ROOT / "scripts" / "metrics" / "merge-stray-ledger.py"


def _interpreter() -> str:
    """First interpreter that can actually ``import fno``.

    The script imports ``fno.paths`` before it ever looks at the stray, so an
    interpreter without fno parks at the import and every assertion below would
    pass vacuously. Refuse instead of testing nothing.
    """
    candidates = [sys.executable, str(REPO_ROOT / "cli" / ".venv" / "bin" / "python")]
    for py in candidates:
        if not py or not Path(py).exists():
            continue
        probe = subprocess.run(
            [py, "-c", "import fno.paths"], capture_output=True, text=True
        )
        if probe.returncode == 0:
            return py
    raise SystemExit(
        "no interpreter can import fno; refusing to run a vacuous suite. "
        "Try: cd cli && uv run pytest ../tests/metrics/"
        "test_merge_stray_ledger_broken_link.py -q"
    )


PY = _interpreter()


def _run(home: Path, stray: Path, *args: str) -> subprocess.CompletedProcess:
    env = dict(os.environ, HOME=str(home))
    return subprocess.run(
        [PY, str(MERGE), "--stray", str(stray), *args],
        capture_output=True,
        text=True,
        env=env,
    )


def _sandbox(tmp: str) -> tuple[Path, Path]:
    """Return (home, stray_dir) with a real global ledger the tool can resolve."""
    home = Path(tmp) / "home"
    (home / ".fno").mkdir(parents=True)
    (home / ".fno" / "ledger.json").write_text('{"entries": []}', encoding="utf-8")
    stray_dir = Path(tmp) / "repo" / ".fno"
    stray_dir.mkdir(parents=True)
    return home, stray_dir


def test_self_referencing_link_is_reported_and_removed() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        home, stray_dir = _sandbox(tmp)
        stray = stray_dir / "ledger.json"
        stray.symlink_to(stray)  # -> itself: ELOOP

        assert not stray.exists(), "precondition: exists() must read the loop as absent"
        assert stray.is_symlink()

        res = _run(home, stray, "--apply")
        assert res.returncode == 0, res.stderr
        assert "broken stray ledger symlink" in res.stdout, res.stdout
        assert "nothing to do" not in res.stdout, "must not report a false all-clear"
        assert not stray.is_symlink(), "the self-loop must be removed"


def test_dangling_link_is_reported_but_dry_run_does_not_remove() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        home, stray_dir = _sandbox(tmp)
        stray = stray_dir / "ledger.json"
        stray.symlink_to(stray_dir / "gone.json")  # dangling

        res = _run(home, stray)  # no --apply
        assert res.returncode == 0, res.stderr
        assert "broken stray ledger symlink" in res.stdout, res.stdout
        assert "[dry-run]" in res.stdout
        assert stray.is_symlink(), "dry-run must not remove anything"


def test_absent_stray_still_reports_nothing_to_do() -> None:
    """Opposite-direction control: a fix that always claims breakage fails here."""
    with tempfile.TemporaryDirectory() as tmp:
        home, stray_dir = _sandbox(tmp)
        stray = stray_dir / "ledger.json"  # never created

        res = _run(home, stray, "--apply")
        assert res.returncode == 0, res.stderr
        assert "nothing to do" in res.stdout, res.stdout
        assert "broken stray ledger symlink" not in res.stdout


if __name__ == "__main__":
    test_self_referencing_link_is_reported_and_removed()
    test_dangling_link_is_reported_but_dry_run_does_not_remove()
    test_absent_stray_still_reports_nothing_to_do()
    print("ok: merge-stray-ledger broken-link branch")
