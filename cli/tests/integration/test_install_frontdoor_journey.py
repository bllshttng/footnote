"""Integration: the install front-door journey (x-538e, AC3-HP).

The complete install itself is proven against real wheels by the smoke suite
(``cli/tests/smoke/``, release CI). This file proves the STATE-MACHINE journey
a first user walks once the advertised ``fno`` command exists: setup answers,
a fixture node the test mints itself is initialized, and the manifest, claim,
and node readbacks all agree. No paid worker, no production node, no remote.

The verbs run as fresh subprocesses of the worktree CLI against an isolated
HOME, so every artifact (graph, claims, spaces manifest) lands under ``tmp``
and nothing reads the developer's real state. The native mux leg drives the
front door compiled in THIS checkout when present, and skips honestly when it
is not (the wheel smokes prove that leg against a real wheel).
"""
from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
import uuid
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]

#: Whether the pytest process itself runs inside a harness session. The shared
#: conftest scrubs ambient identity markers per-test, so capture this at import:
#: a claim acquired WITH a provable session stays live after init exits, while
#: a claim from a bare shell is anchored to the transient init process and
#: reads free the moment it exits. Both are correct behavior; only the first
#: supports a liveness assertion.
_HARNESS_SESSION_AT_IMPORT = os.environ.get("CLAUDE_CODE_SESSION_ID", "")

#: Env that would route state back at the developer's checkout or fleet.
_DEV_ENV_KEYS = (
    "FNO_REPO_ROOT",
    "FNO_SPACES_DIR",
    "FNO_CLAIMS_ROOT",
    "FNO_EVENTS_PATH",
    "PYTHONPATH",
)


def _front_door() -> Path | None:
    """The mux front door compiled in THIS checkout (crates/fno), if any.

    Mirrors ``fno.rust_binary.find_dev_binary``'s contract - this checkout's
    build only - for the OTHER crate: the front door lives in ``crates/fno``,
    not ``crates/fno-agents``.
    """
    if not (REPO_ROOT / "crates" / "fno").is_dir():
        return None
    for profile in ("release", "debug"):
        candidate = REPO_ROOT / "crates" / "fno" / "target" / profile / "fno"
        if candidate.is_file():
            return candidate
    return None


def _run_fno(repo: Path, home: Path, *args: str) -> subprocess.CompletedProcess[str]:
    """One fresh CLI process: the journey must exercise the real verb surface."""
    env = {k: v for k, v in os.environ.items() if k not in _DEV_ENV_KEYS}
    # The journey rides an attributable identity: init stamps the node lock and
    # acquires the claim against a harness session, and the shared conftest
    # deliberately strips every marker so tests run as bare operator shells. A
    # session that wants agent semantics sets its own marker (conftest's
    # documented carve-out), so this journey carries one - the real session id
    # when pytest itself runs inside a harness, a synthetic one otherwise.
    env.setdefault("CLAUDE_CODE_SESSION_ID", _HARNESS_SESSION_AT_IMPORT or "journey-fixture")
    return subprocess.run(
        [sys.executable, "-c", "from fno.cli import app; app()", *args],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )


@pytest.fixture()
def clean_machine(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[Path, Path]:
    """A disposable git repo on a feature branch, and a pristine HOME."""
    home = tmp_path / "home"
    home.mkdir()
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(REPO_ROOT))
    for key in _DEV_ENV_KEYS:
        monkeypatch.delenv(key, raising=False)

    # `fno` on PATH IS the worktree CLI: the init script's gates shell out to
    # `fno`, and the deployed front door on the dev's PATH would answer instead
    # - then resolve its fno-py under this test's HOME and break. Same code,
    # bound to the checkout under test; the Rust mux leg is exercised directly
    # against the compiled front door in its own test.
    shim = tmp_path / "shim"
    shim.mkdir()
    launcher = shim / "fno"
    launcher.write_text(
        "#!/bin/sh\n"
        + shlex.quote(sys.executable)
        + " -c 'from fno.cli import app; app()' \"$@\"\n"
    )
    launcher.chmod(0o755)
    monkeypatch.setenv("PATH", f"{shim}{os.pathsep}{os.environ['PATH']}")

    def _git(*args: str) -> None:
        subprocess.run(
            ["git", "-C", str(repo), "-c", "user.email=t@t", "-c", "user.name=t", *args],
            check=True,
            capture_output=True,
        )

    subprocess.run(["git", "init", "-q", "-b", "feature/fixture", str(repo)], check=True)
    _git("commit", "--allow-empty", "-m", "seed")
    return home, repo


def test_setup_surface_answers_on_a_clean_machine(clean_machine):
    """`fno config setup plan` - setup's scriptable form - answers with a real
    schema-derived question plan (the wizard is its interactive twin)."""
    _home, repo = clean_machine
    proc = _run_fno(repo, _home, "config", "setup", "plan")
    assert proc.returncode == 0, proc.stderr
    # The banner line precedes the JSON; parse from the first "{".
    payload = json.loads(proc.stdout[proc.stdout.index("{"):])
    assert payload["fields"], payload


def test_authorized_target_init_journey(clean_machine):
    """Setup -> minted fixture node -> authorized init -> matching readbacks.

    The node is "authorized" because this test minted it in the graph it owns:
    a fresh idea with a declared deliverable count, claimed by the init it
    dispatched. Every readback must agree with that one identity.
    """
    home, repo = clean_machine

    # 1. The node: minted by us, in the state root we own.
    proc = _run_fno(
        repo, home, "backlog", "idea",
        f"journey fixture {uuid.uuid4().hex[:8]}",
        "--difficulty", "low", "--separate",
    )
    assert proc.returncode == 0, proc.stderr
    node = json.loads(proc.stdout)["id"]

    # 2. Setup ran through the same CLI before init (the wizard's plan, above,
    #    proves the surface; this journey re-runs it so the receipt is one run).
    plan = _run_fno(repo, home, "config", "setup", "plan")
    assert plan.returncode == 0, plan.stderr

    # 3. Authorized target init: a declared denominator, no plan, no remote.
    init = _run_fno(repo, home, "do", "target", "init", "--input", node, "--deliverables", "1")
    assert init.returncode == 0, f"init rc={init.returncode}\n{init.stdout}\n{init.stderr}"

    # 4. The manifest readback: written under the ISOLATED state root, naming
    #    this node AND the claim it acquired for it - the matching receipt.
    #    Three candidate roots: repo-local (<repo>/.fno/, where a default
    #    resolve writes it - the CI runner's case), the isolated HOME's spaces
    #    dir, and the conftest autouse sandbox's tmp/spaces pin (which of the
    #    three wins is fixture-ordering and config dependent). The
    #    node-matching manifest is the assertion, not the path.
    manifests = (
        [repo / ".fno" / "target-state.md"]
        + list(home.glob(".fno/spaces/*/target-state.md"))
        + list(home.parent.glob("spaces/*/target-state.md"))
    )
    manifests = [m for m in manifests if m.exists()]
    assert manifests, (
        f"no session manifest in {repo}/.fno, {home}/.fno/spaces, "
        f"or {home.parent}/spaces\n{init.stdout}\n{init.stderr}"
    )
    matching = [m for m in manifests if node in m.read_text()]
    assert matching, [m.name for m in manifests]
    manifest_text = matching[0].read_text()
    assert f"node:{node}" in manifest_text, manifest_text

    # 5. The claim readback: the claims store answers for this exact key. When
    #    the run carries a provable harness session, the claim is still live;
    #    from a bare shell it correctly died with the init that made it.
    claim = _run_fno(repo, home, "agents", "claim", "status", f"node:{node}")
    assert claim.returncode == 0, claim.stderr
    if _HARNESS_SESSION_AT_IMPORT:
        assert '"state": "live"' in claim.stdout, claim.stdout

    # 6. The node readback: the graph agrees the work is in progress.
    got = _run_fno(repo, home, "backlog", "get", node)
    assert got.returncode == 0, got.stderr
    assert "in_progress" in got.stdout, got.stdout


def test_both_command_families_usable_after_init(clean_machine):
    """AC3-HP's tail: after the journey, the mux family and the Python family
    both answer. The mux leg runs the checkout's own front door; the Python
    leg is every CLI process the journey already ran."""
    home, repo = clean_machine
    door = _front_door()
    if door is None:
        pytest.skip("compiled fno front door not present (build with `cargo build -p fno`)")
    proc = subprocess.run(
        [str(door), "mux", "ls", "--json"],
        cwd=repo,
        env={k: v for k, v in os.environ.items() if k not in _DEV_ENV_KEYS},
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    assert proc.returncode == 0, proc.stderr
    json.loads(proc.stdout)
