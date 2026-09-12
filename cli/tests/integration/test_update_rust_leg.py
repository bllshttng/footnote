"""Integration-tier journey for the ``fno doctor update`` rust-bins leg (ab-054fd162).

The unit tier (tests/unit/test_update.py) covers every gating outcome with
monkeypatched internals; sigma-review on PR #438 flagged that no
integration-tier journey exercises the leg through the real CLI. This test
runs ``fno doctor update`` as a subprocess against a real git repo fixture, with
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


def _cargo_stub(
    path: Path, log: Path, cargo_home: Path, crates_rev: str, head_rev: str
) -> None:
    """Stub ``cargo`` that logs its args and, on ``install``, writes a fake triad
    (client/daemon/worker) whose ``version --json`` self-reports the given revs.

    update now gates the rust leg on the binary's embedded ``crates_rev`` and
    runs a post-deploy verify that executes the deployed artifact, so a stub that
    only logs (never producing a runnable, self-reporting binary) can no longer
    stand in for a real ``cargo install``.
    """
    bindir = cargo_home / "bin"
    version_json = '{"crates_rev": "%s", "git_rev": "%s", "dirty": false}' % (
        crates_rev, head_rev,
    )
    inner = (
        "#!/bin/sh\n"
        'if [ "$1" = "component-verdict" ]; then\n'
        '  shift\n'
        '  exec "$FNO_AGENTS_BIN" component-verdict "$@"\n'
        "fi\n"
        'if [ "$1" = "version" ] && [ "$2" = "--json" ]; then\n'
        f"  echo '{version_json}'\n"
        "fi\n"
    )
    script = (
        "#!/bin/sh\n"
        f'echo "$@" >> "{log}"\n'
        'if [ "$1" = "install" ]; then\n'
        f'  mkdir -p "{bindir}"\n'
        "  for b in fno-agents fno-agents-daemon fno-agents-worker; do\n"
        f'    cat > "{bindir}/$b" <<\'EOF\'\n'
        f"{inner}"
        "EOF\n"
        f'    chmod +x "{bindir}/$b"\n'
        "  done\n"
        "fi\n"
        "exit 0\n"
    )
    path.write_text(script, encoding="utf-8")
    path.chmod(0o755)


def _uv_stub(path: Path, log: Path, tmp_path: Path) -> None:
    """Stub ``uv`` that logs its args, exits 0, and answers ``tool dir`` with a
    dir carrying the post-install marker (executable ``fno-py`` plus one
    ``.pyc``). The installer's retry wrapper verifies that marker after every
    success, so a stub that only logs would make a "successful" install be
    correctly refused."""
    tools = tmp_path / "uvtools"
    entry = tools / "fno/bin/fno-py"
    entry.parent.mkdir(parents=True, exist_ok=True)
    entry.write_text("#!/bin/sh\n", encoding="utf-8")
    entry.chmod(0o755)
    pyc = tools / "fno/lib/python3.13/site-packages/fno/__pycache__/x.pyc"
    pyc.parent.mkdir(parents=True, exist_ok=True)
    pyc.write_text("", encoding="utf-8")
    path.write_text(
        "#!/bin/sh\n"
        f'echo "$@" >> "{log}"\n'
        'if [ "$1 $2" = "tool dir" ]; then\n'
        f'  echo "{tools}"\n'
        "fi\n"
        "exit 0\n",
        encoding="utf-8",
    )
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
    """Journey: stale rust marker -> real ``fno doctor update`` subprocess runs the
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
    _cargo_stub(fakebin / "cargo", cargo_log, cargo_home, crates_rev, head_rev)
    _uv_stub(fakebin / "uv", uv_log, tmp_path)

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
    # Run 2 reclaims run 1's install-guard claim through the native claim
    # door, and that door is a subprocess: without a resolvable fno-agents
    # binary the acquire fails closed ("held") and the rust-fresh
    # short-circuit never runs. Forward the caller's binary; skip when the
    # caller has none - this journey needs the real instrument, not a stub.
    if not os.environ.get("FNO_AGENTS_BIN"):
        pytest.skip(
            "FNO_AGENTS_BIN unset: the claim-door reclaim this journey drives "
            "needs a resolvable fno-agents binary"
        )
    env["FNO_AGENTS_BIN"] = os.environ["FNO_AGENTS_BIN"]

    # --- Run 1: stale marker -> cargo refresh + installer handoff ---
    result = _run_update(cli_src, env, cwd=repo)
    assert result.returncode == 0, (
        f"exit {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )
    assert "refreshing rust bins" in result.stdout, result.stdout
    assert f"rust bins refreshed (rev {crates_rev[:12]})" in result.stdout, result.stdout

    # Stub cargo got the pinned-root install command, exactly once.
    cargo_lines = cargo_log.read_text(encoding="utf-8").strip().splitlines()
    assert len(cargo_lines) == 1, cargo_lines
    assert "install --path" in cargo_lines[0]
    assert str(Path("crates") / "fno-agents") in cargo_lines[0]
    assert "--bins" in cargo_lines[0]
    assert f"--root {cargo_home}" in cargo_lines[0]

    # Installer handoff happened after the rust leg, and the chained
    # installed-rev write (post-execvp, gated on installer exit 0) recorded
    # the source HEAD. --refresh-package fno must ride along: without it a
    # uv wheel-cache hit reinstalls the same stale bytes and the update never
    # converges. Narrow form, not --reinstall: the wide form strips every
    # package out of the shared tool venv under running processes.
    uv_text = uv_log.read_text(encoding="utf-8")
    assert "tool install --reinstall-package fno" in uv_text
    assert "--refresh-package fno" in uv_text
    assert "--reinstall " not in uv_text
    assert str(cli_src.resolve()) in uv_text
    installed_rev = home / ".fno" / "installed-rev"
    assert installed_rev.read_text(encoding="utf-8").strip() == head_rev

    # --- Run 2: marker now fresh -> rust leg short-circuits, no cargo ---
    result2 = _run_update(cli_src, env, cwd=repo)
    assert result2.returncode == 0, (
        f"exit {result2.returncode}\nstdout:\n{result2.stdout}\nstderr:\n{result2.stderr}"
    )
    # The gate now reads the binary's self-reported rev, so the message quotes it
    # "from binary". The verdict is native: with the fixture's crates/ holding no
    # mux crate, only the triad is classified, the deployed binary answers the
    # component-verdict transport itself, and all three prove Fresh.
    assert f"rust bins fresh (rev {crates_rev[:12]} from binary)" in result2.stdout, result2.stdout
    cargo_lines_after = cargo_log.read_text(encoding="utf-8").strip().splitlines()
    assert cargo_lines_after == cargo_lines, "fresh short-circuit must not re-invoke cargo"


def test_component_verdict_binary_journey(tmp_path: Path) -> None:
    """Journey: the real binary probes a bindir of deployed-shape executables
    ITSELF (no Python probe in the loop) and proves convergence only when
    every component's probe matches. Stale bytes keep the fleet unconverged
    and name the executable repair; an unanswerable binary is Unknown with
    the named instrument."""
    binary = os.environ.get("FNO_AGENTS_BIN")
    if not binary:
        pytest.skip(
            "FNO_AGENTS_BIN unset: this journey drives the real fno-agents binary"
        )

    import json as jsonlib
    import subprocess as subproc

    def ask(*args: str) -> dict:
        proc = subproc.run(
            [binary, "component-verdict", *args], capture_output=True, text=True, timeout=60,
        )
        assert proc.returncode == 0, proc.stderr
        return jsonlib.loads(proc.stdout)

    bindir = tmp_path / "bin"
    bindir.mkdir()
    fresh_json = '{"crates_rev": "%s", "dirty": false}' % ("a" * 40)
    stale_json = '{"crates_rev": "%s", "dirty": false}' % ("0" * 40)

    def deploy(name: str, payload: str) -> None:
        p = bindir / name
        p.write_text(f"#!/bin/sh\necho '{payload}'\n", encoding="utf-8")
        p.chmod(0o755)

    deploy("fno-agents", fresh_json)
    deploy("fno-agents-daemon", fresh_json)
    deploy("fno-agents-worker", fresh_json)
    deploy("fno", fresh_json)

    common = [
        "--bindir", str(bindir),
        "--expected", "a" * 40,
        "--include-mux",
        "--agents-dir", "/src/crates/fno-agents",
    ]
    fresh = ask(*common, "--python-rev", "a" * 40, "--python-expected", "a" * 40)
    assert fresh["converged"] is True
    names = {c["component"] for c in fresh["components"]}
    assert names == {"fno-agents", "fno-agents-daemon", "fno-agents-worker", "fno", "python-tool"}

    # A stale worker keeps the fleet unconverged and names the repair.
    deploy("fno-agents-worker", stale_json)
    stale = ask(*common)
    assert stale["converged"] is False
    worker = [c for c in stale["components"] if c["component"] == "fno-agents-worker"][0]
    assert worker["status"] == "stale"
    assert "cargo install --path /src/crates/fno-agents" in (worker["repair"] or "")

    # An executable the probe cannot answer is Unknown with the named
    # instrument, never collapsed into fresh or missing (AC3-HP).
    deploy("fno-agents-worker", "not-json-at-all")
    junk = ask(*common)
    assert junk["converged"] is False
    worker = [c for c in junk["components"] if c["component"] == "fno-agents-worker"][0]
    assert worker["status"] == "unknown"
    assert "unparseable" in (worker["detail"] or "")
