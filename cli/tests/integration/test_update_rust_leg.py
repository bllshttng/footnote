"""Integration-tier journey for the ``fno update`` rust-bins leg (ab-054fd162).

The unit tier (tests/unit/test_update.py) covers every gating outcome with
monkeypatched internals; sigma-review on PR #438 flagged that no
integration-tier journey exercises the leg through the real CLI. This test
runs ``fno update`` as a subprocess against a real git repo fixture, with
stub ``cargo``/``uv`` executables on PATH and an isolated HOME - so the
stale-marker -> cargo install -> marker write -> installer-handoff chain
(including the post-execvp installed-rev write) runs with no in-process
patching at all.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

SRC_DIR = Path(__file__).resolve().parents[2] / "src"

# No in-git-repo skip: _make_repo creates its own repo under tmp_path, so the
# test does not depend on the test process running inside a repository.
pytestmark = [
    pytest.mark.skipif(os.name == "nt", reason="POSIX sh stubs + execvp handoff journey"),
    pytest.mark.skipif(shutil.which("git") is None, reason="git CLI not available"),
]


def _git(repo: Path, *args: str) -> str:
    res = subprocess.run(
        [
            "git", "-C", str(repo),
            "-c", "user.email=test@example.com", "-c", "user.name=test",
            *args,
        ],
        capture_output=True, text=True, check=True,
    )
    return res.stdout.strip()


def _make_repo(root: Path) -> tuple[Path, str, str]:
    """Real git repo holding a cli/ source dir + crates/fno-agents.

    Returns (cli_src, head_rev, crates_rev). A python-only commit follows
    the crate commit so head_rev != crates_rev - the rust marker must
    record the crates subtree rev, not HEAD.
    """
    repo = root / "repo"
    crate = repo / "crates" / "fno-agents"
    crate.mkdir(parents=True)
    (crate / "Cargo.toml").write_text('[package]\nname = "fno-agents"\n', encoding="utf-8")
    cli_src = repo / "cli"
    cli_src.mkdir()
    (cli_src / "pyproject.toml").write_text('[project]\nname = "fno"\n', encoding="utf-8")
    _git(repo, "init", "-q")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "crate + cli source")
    crates_rev = _git(repo, "log", "-1", "--format=%H", "--", "crates/")
    (cli_src / "README.md").write_text("python-only change\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "python-only commit")
    head_rev = _git(repo, "rev-parse", "HEAD")
    assert head_rev != crates_rev, "fixture must separate HEAD from the crates subtree rev"
    return cli_src, head_rev, crates_rev


def _stub(path: Path, log: Path) -> None:
    """Executable that records its args and exits 0."""
    path.write_text(f'#!/bin/sh\necho "$@" >> "{log}"\nexit 0\n', encoding="utf-8")
    path.chmod(0o755)


def _run_update(cli_src: Path, env: dict[str, str], cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable, "-c", "from fno.cli import app; app()",
            "update", "--source", str(cli_src),
        ],
        capture_output=True, text=True, env=env, cwd=str(cwd), timeout=120,
    )


def test_update_rust_leg_journey(tmp_path: Path) -> None:
    """Journey: stale rust marker -> real ``fno update`` subprocess runs the
    stub cargo, converges the marker to the crates subtree rev, hands off to
    the stub installer (which chains the installed-rev write) -> a second
    run short-circuits as fresh without re-invoking cargo."""
    cli_src, head_rev, crates_rev = _make_repo(tmp_path)
    repo = cli_src.parent

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    cargo_home = tmp_path / "cargo-home"
    (cargo_home / "bin").mkdir(parents=True)
    (cargo_home / "bin" / "fno-agents").write_text("stale binary", encoding="utf-8")

    fakebin = tmp_path / "fakebin"
    fakebin.mkdir()
    cargo_log = tmp_path / "cargo.log"
    uv_log = tmp_path / "uv.log"
    _stub(fakebin / "cargo", cargo_log)
    _stub(fakebin / "uv", uv_log)

    rust_marker = home / ".fno" / "installed-rust-rev"
    rust_marker.write_text("0" * 40 + "\n", encoding="utf-8")  # stale

    git_bin = Path(shutil.which("git") or "/usr/bin/git").parent
    env = {
        "PATH": f"{fakebin}:{git_bin}:/usr/bin:/bin",
        "HOME": str(home),
        "CARGO_HOME": str(cargo_home),
        "PYTHONPATH": str(SRC_DIR),
        "FNO_SKIP_MIGRATION": "1",
        "NO_COLOR": "1",
        "TERM": "dumb",
        "COLUMNS": "200",
    }

    # --- Run 1: stale marker -> cargo refresh + installer handoff ---
    result = _run_update(cli_src, env, cwd=repo)
    assert result.returncode == 0, (
        f"exit {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )
    assert "refreshing rust bins" in result.stdout, result.stdout
    assert f"rust bins refreshed (rev {crates_rev[:12]})" in result.stdout, result.stdout

    # Marker converged to the crates subtree rev, NOT HEAD: the trailing
    # python-only commit must not be recorded as the rust rev.
    assert rust_marker.read_text(encoding="utf-8").strip() == crates_rev

    # Stub cargo got the pinned-root install command, exactly once.
    cargo_lines = cargo_log.read_text(encoding="utf-8").strip().splitlines()
    assert len(cargo_lines) == 1, cargo_lines
    assert "install --path" in cargo_lines[0]
    assert str(Path("crates") / "fno-agents") in cargo_lines[0]
    assert "--bins" in cargo_lines[0]
    assert f"--root {cargo_home}" in cargo_lines[0]

    # Installer handoff happened after the rust leg, and the chained
    # installed-rev write (post-execvp, gated on installer exit 0) recorded
    # the source HEAD.
    uv_text = uv_log.read_text(encoding="utf-8")
    assert "tool install --reinstall" in uv_text
    assert str(cli_src.resolve()) in uv_text
    installed_rev = home / ".fno" / "installed-rev"
    assert installed_rev.read_text(encoding="utf-8").strip() == head_rev

    # --- Run 2: marker now fresh -> rust leg short-circuits, no cargo ---
    result2 = _run_update(cli_src, env, cwd=repo)
    assert result2.returncode == 0, (
        f"exit {result2.returncode}\nstdout:\n{result2.stdout}\nstderr:\n{result2.stderr}"
    )
    assert f"rust bins fresh (rev {crates_rev[:12]})" in result2.stdout, result2.stdout
    cargo_lines_after = cargo_log.read_text(encoding="utf-8").strip().splitlines()
    assert cargo_lines_after == cargo_lines, "fresh short-circuit must not re-invoke cargo"
