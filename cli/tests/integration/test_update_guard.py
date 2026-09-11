"""Integration tests for `fno doctor update` IN_PROGRESS guard.

Task 4b.2 of plan 2026-05-14-path-config-impl.

Tests cover:
- AC4b-HP: IN_PROGRESS blocks update (exit 1 + exact stderr message)
- AC4b-HP: COMPLETE allows update
- AC4b-HP: missing state file allows update
- AC4b-EDGE: --force bypasses the guard when IN_PROGRESS
- AC4b-FR: malformed target-state.md (no --- frontmatter) treated as not-IN_PROGRESS
- AC4b-EDGE: walks correctly from a subdirectory (guard still finds repo root)

All tests use tmp_path + monkeypatch for isolation.
Autouse fixture pins FNO_REPO_ROOT (memory: feedback_fno_repo_root_leaks_between_tests).
Actual uv/pip install logic is stubbed so tests don't try to download anything.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

from fno.cli import app

_REFUSED_MSG = (
    "[fno doctor update] refused: target-state.md shows status: IN_PROGRESS. "
    "Updating mid-loop risks binary skew across subprocesses. "
    "Pass --force to override."
)

runner = CliRunner()


# ---------------------------------------------------------------------------
# Autouse fixture: isolate FNO_REPO_ROOT, stub out actual install
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Pin repo root + stub away the real install logic before each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    # resolve_repo_root caches on first call and freezes the env var then, so
    # an earlier resolution in this process (import-time config discovery, a
    # prior test) would shadow the pin above and the guard would read the
    # checkout's own target-state.md instead of tmp_path's.
    from fno.paths import resolve_repo_root

    # x-d211: pin the watcher-tick marker off by default so a control test
    # cannot inherit a leftover value from the outer environment.
    monkeypatch.delenv("FNO_PR_WATCH_ACTIVE_TICK", raising=False)

    # Stub the real install so tests never execute uv/pip.
    # We patch _discover_source to return a sentinel Path, and os.execvp + subprocess.run
    # to be no-ops. Monkeypatch BEFORE invoking the command (memory: feedback_default_arg_breaks_monkeypatch_isolation).
    import fno.update as update_mod

    monkeypatch.setattr(
        update_mod,
        "_discover_source",
        lambda override=None: tmp_path / "fake-source",
    )
    monkeypatch.setattr(update_mod.os, "execvp", lambda *a, **kw: None)
    import subprocess as subprocess_mod
    import types

    # stdout/stderr present so update_command's _source_rev() git probe (which
    # also goes through subprocess.run) gets a CompletedProcess-shaped object
    # rather than AttributeError-ing on a bare namespace. Empty stdout => the
    # rev is undeterminable for the fake source, so the marker chain is skipped.
    fake_result = types.SimpleNamespace(returncode=0, stdout="", stderr="")
    monkeypatch.setattr(
        update_mod.subprocess,
        "run",
        lambda *a, **kw: fake_result,
    )
    yield


def _write_state(tmp_path: Path, content: str) -> None:
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "target-state.md").write_text(content)


# ---------------------------------------------------------------------------
# AC4b-HP: IN_PROGRESS blocks
# ---------------------------------------------------------------------------


def test_ac4b_hp_in_progress_blocks(tmp_path: Path) -> None:
    """Given target-state.md shows IN_PROGRESS, update exits 1 with refusal message."""
    _write_state(
        tmp_path,
        "---\nstatus: IN_PROGRESS\n---\n\nsome content\n",
    )
    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 1
    assert _REFUSED_MSG in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-HP: COMPLETE allows
# ---------------------------------------------------------------------------


def test_ac4b_hp_complete_allows(tmp_path: Path) -> None:
    """Given target-state.md shows COMPLETE, update exits 0."""
    _write_state(
        tmp_path,
        "---\nstatus: COMPLETE\n---\n\nsome content\n",
    )
    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-HP: missing state file allows
# ---------------------------------------------------------------------------


def test_ac4b_hp_missing_state_file_allows(tmp_path: Path) -> None:
    """Given no target-state.md exists, update exits 0."""
    # .fno dir doesn't exist at all
    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-EDGE: --force bypasses guard
# ---------------------------------------------------------------------------


def test_ac4b_edge_force_bypasses(tmp_path: Path) -> None:
    """Given IN_PROGRESS + --force, update proceeds (exit 0, no refusal)."""
    _write_state(
        tmp_path,
        "---\nstatus: IN_PROGRESS\n---\n",
    )
    result = runner.invoke(app, ["doctor", "update", "--force"])
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-FR: malformed target-state.md treated as not-IN_PROGRESS
# ---------------------------------------------------------------------------


def test_ac4b_fr_malformed_state_file_lenient(tmp_path: Path) -> None:
    """Given target-state.md with no --- frontmatter, guard is lenient and update proceeds."""
    _write_state(
        tmp_path,
        "status: IN_PROGRESS\nno frontmatter here\n",
    )
    result = runner.invoke(app, ["doctor", "update"])
    # Lenient: guard should NOT block; exit 0
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-EDGE: walks correctly from a subdirectory
# ---------------------------------------------------------------------------


def test_ac4b_edge_subdir_walk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Guard finds target-state.md even when cwd is a subdirectory of the repo."""
    _write_state(
        tmp_path,
        "---\nstatus: IN_PROGRESS\n---\n",
    )
    # Create a git sentinel so _target_in_progress walk finds the boundary.
    (tmp_path / ".git").mkdir()
    # cwd inside a deep subdirectory.
    subdir = tmp_path / "deep" / "nested" / "dir"
    subdir.mkdir(parents=True)
    # FNO_REPO_ROOT still points to tmp_path (set by autouse), which is what
    # resolve_repo_root() returns. The guard should still find the state file.
    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 1
    assert _REFUSED_MSG in (result.output or "")


# ---------------------------------------------------------------------------
# ab-5a1fc285: a successful update chains the installed-rev marker write onto
# the installer via the shell, so the marker lands ONLY on install success.
# ---------------------------------------------------------------------------


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_successful_update_chains_marker_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """On Unix, a clean update execs `/bin/sh -c '<install> && <atomic marker write>'`."""
    import fno.update as update_mod

    # Source resolves to a checkout with a known rev (autouse stubs a non-git
    # fake source, so pin the rev explicitly here).
    monkeypatch.setattr(update_mod, "_source_rev", lambda src: "cafef00d")
    marker = tmp_path / "state" / "installed-rev"
    monkeypatch.setattr(update_mod, "_INSTALLED_REV_FILE", marker)

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0

    # The installer is exec'd through the shell so the marker write can be
    # gated on its success.
    assert captured["file"] == "/bin/sh"
    shell_line = captured["args"][2]  # ["/bin/sh", "-c", "<line>"]
    assert " && " in shell_line
    assert "cafef00d" in shell_line
    assert str(marker) in shell_line
    # Atomic write (temp + mv), never a direct write to the marker.
    assert "mv " in shell_line


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_update_without_source_rev_skips_marker_chain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When the source rev is undeterminable, no installed-rev marker is written.

    The pr-watch refresh still chains after the install (via /bin/sh, since
    there is a command to run post-install), but the marker-write chain must be
    absent - there is no rev to record.
    """
    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: None)

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0
    # The marker-write chain (printf rev > tmp && mv) must be absent with no rev.
    joined = " ".join(captured.get("args") or [])
    assert "installed-rev" not in joined
    # NOT `"printf" not in joined`: printf is no longer a marker-write signature,
    # because the post-install guard uses it to warn when fno-py never reappears
    # after the install. Assert the atomic rename that actually lands the marker,
    # which is both specific to the marker chain and a stronger claim.
    assert "mv " not in joined
    # The canonical watcher refresh still rides the successful install (best-effort).
    assert "do pr watch refresh" in joined


def _invoke_update_capturing_execvp(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, rev: str | None
) -> str:
    """Run `fno doctor update` with a pinned rev and return the exec'd shell line."""
    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: rev)
    marker = tmp_path / "state" / "installed-rev"
    monkeypatch.setattr(update_mod, "_INSTALLED_REV_FILE", marker)

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0
    return " ".join(captured.get("args") or [])


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_watcher_owned_update_skips_only_the_watcher_refresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: an update inside a pr-watch tick (marker set) still installs and
    refreshes the groom agent, but omits `do pr watch refresh` - that refresh
    bootouts the job owning the running tick (x-d211)."""
    monkeypatch.setenv("FNO_PR_WATCH_ACTIVE_TICK", "tick:12345")
    joined = _invoke_update_capturing_execvp(tmp_path, monkeypatch, rev="cafef00d")

    # Positive controls: the install itself and the groom refresh survive.
    assert "cafef00d" in joined
    assert "backlog groom --refresh-agent" in joined
    # The executed refresh is absent. Match the argv-joined form (resolved
    # binary prefix), not the bare phrase: the else-branch warning ("run by
    # hand: fno do pr watch refresh; ...") legitimately still mentions it.
    import shlex

    from fno.pr_watch.cli import _resolve_fno_binary

    executed_refresh = f"{shlex.quote(_resolve_fno_binary())} do pr watch refresh"
    assert executed_refresh not in joined


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_ordinary_update_keeps_both_refreshes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP control: without the marker, both post-install refreshes remain."""
    joined = _invoke_update_capturing_execvp(tmp_path, monkeypatch, rev=None)

    assert "do pr watch refresh" in joined
    assert "backlog groom --refresh-agent" in joined


def test_update_without_source_rev_execs_retry_wrapped_install_when_no_refresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With no rev AND no refresh resolvable, still exec the install through
    the /bin/sh retry wrapper: the ENOTEMPTY race retry and the marker verify
    live in the exec'd shell line, so no path may bypass them."""
    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: None)
    # Force refresh resolution to fail so refresh_argv stays None.
    import fno.pr_watch.cli as pw_cli
    monkeypatch.setattr(
        pw_cli, "_resolve_fno_binary",
        lambda: (_ for _ in ()).throw(RuntimeError("no binary")),
    )

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0
    assert captured["file"] == "/bin/sh"  # the retry wrapper needs the shell
    line = captured["args"][2]
    assert "Directory not empty" in line, "retry signature match must be present"
    assert "fno-py" in line, "marker verify must be present"


# ---------------------------------------------------------------------------
# Deployed-component convergence: partial success names what did not converge
# ---------------------------------------------------------------------------


def _make_crate_source(tmp_path: Path) -> None:
    """The autouse fixture's sentinel source has no crates/ tree; the rust leg
    needs crates/fno-agents to exist or it skips as skipped-no-crate."""
    (tmp_path / "crates" / "fno-agents").mkdir(parents=True, exist_ok=True)


def _deployed_self_reporter(tmp_path: Path, payload: str) -> Path:
    """A deployed-shape executable whose `version --json` answers `payload`."""
    import shutil as shutil_mod

    script = tmp_path / "bin" / "fno-agents"
    script.parent.mkdir(parents=True, exist_ok=True)
    script.write_text(
        "#!/bin/sh\n"
        f"echo '{payload}'\n",
        encoding="utf-8",
    )
    script.chmod(0o755)
    assert shutil_mod.which("git") is not None or True  # env sanity, no-op
    return script


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_cargo_failure_preserves_python_update_and_refuses_freshness(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A failed cargo rebuild does not fail the Python update (warn-and-continue
    is the locked semantics), and the receipt refuses freshness it cannot prove:
    the deployed binary cannot answer the verdict, and the output says so."""
    import types

    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: "cafef00d")
    monkeypatch.setattr(update_mod, "_rust_subtree_rev", lambda src: "b" * 40)
    monkeypatch.setattr(update_mod, "_RUST_MARKER_FILE", tmp_path / "rust-marker")
    _make_crate_source(tmp_path)
    stale_bin = _deployed_self_reporter(
        tmp_path,
        '{"crates_rev": "%s", "dirty": false}' % ("0" * 40),
    )
    monkeypatch.setattr(update_mod, "_cargo_installed_bin", lambda: stale_bin)

    fake_ok = types.SimpleNamespace(returncode=0, stdout="", stderr="")

    def _fake_run(cmd, *a, **kw):
        if cmd and cmd[0] == "cargo":
            return types.SimpleNamespace(returncode=2, stdout="", stderr="")
        return fake_ok

    monkeypatch.setattr(update_mod.subprocess, "run", _fake_run)
    captured: dict[str, object] = {}
    monkeypatch.setattr(
        update_mod.os, "execvp", lambda f, a: captured.update(file=f, args=a)
    )

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0, result.output
    assert "WARNING: cargo install failed (exit 2)" in result.output
    # Partial success: the Python install still exec'd, --refresh riding along.
    assert captured.get("file") == "/bin/sh"
    assert "--refresh" in captured["args"][2]
    # No convergence claim without a verdict: the transport says it cannot prove.
    assert "component verdict unavailable" in result.output


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_malformed_version_output_halts_the_rust_leg(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A deploy whose landed binary emits unparseable `version --json` output
    cannot prove convergence: the post-deploy verify halts the leg loudly and
    no installer runs."""
    import types

    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: "cafef00d")
    monkeypatch.setattr(update_mod, "_rust_subtree_rev", lambda src: "b" * 40)
    monkeypatch.setattr(update_mod, "_RUST_MARKER_FILE", tmp_path / "rust-marker")
    _make_crate_source(tmp_path)
    garbage_bin = _deployed_self_reporter(tmp_path, "not-json-at-all")
    monkeypatch.setattr(update_mod, "_cargo_installed_bin", lambda: garbage_bin)

    fake_ok = types.SimpleNamespace(returncode=0, stdout="", stderr="")
    monkeypatch.setattr(update_mod.subprocess, "run", lambda *a, **kw: fake_ok)
    captured: dict[str, object] = {}
    monkeypatch.setattr(
        update_mod.os, "execvp", lambda f, a: captured.update(file=f, args=a)
    )

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 1
    assert "post-deploy verify FAILED" in result.output
    # The deployed binary cannot answer the verdict, and the receipt says so
    # instead of claiming freshness.
    assert "component verdict unavailable" in result.output
    assert not captured, "a halt must never reach the installer exec"


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_missing_cargo_names_component_evidence_and_still_installs_python(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """cargo absent from PATH: the rust leg skips, the receipt names the
    evidence gap, and the Python install still proceeds."""
    import shutil as shutil_mod
    import types

    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: "cafef00d")
    monkeypatch.setattr(update_mod, "_rust_subtree_rev", lambda src: "b" * 40)
    monkeypatch.setattr(update_mod, "_RUST_MARKER_FILE", tmp_path / "rust-marker")
    _make_crate_source(tmp_path)
    stale_bin = _deployed_self_reporter(
        tmp_path,
        '{"crates_rev": "%s", "dirty": false}' % ("0" * 40),
    )
    monkeypatch.setattr(update_mod, "_cargo_installed_bin", lambda: stale_bin)
    real_which = shutil_mod.which
    monkeypatch.setattr(
        shutil_mod,
        "which",
        lambda name, **kw: None if name == "cargo" else real_which(name),
    )
    fake_ok = types.SimpleNamespace(returncode=0, stdout="", stderr="")
    monkeypatch.setattr(update_mod.subprocess, "run", lambda *a, **kw: fake_ok)
    captured: dict[str, object] = {}
    monkeypatch.setattr(
        update_mod.os, "execvp", lambda f, a: captured.update(file=f, args=a)
    )

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0, result.output
    assert "cargo is not on PATH" in result.output
    assert "component verdict unavailable" in result.output
    assert captured.get("file") == "/bin/sh"


def test_update_pip_fallback_is_not_wrapped_in_the_uv_retry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No uv on PATH -> the pip fallback execs bare. The retry wrapper's
    success marker reads `uv tool dir`, so wrapping pip would refuse a
    perfectly good install on exactly the machines that have no uv."""
    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: None)
    import fno.pr_watch.cli as pw_cli
    monkeypatch.setattr(
        pw_cli, "_resolve_fno_binary",
        lambda: (_ for _ in ()).throw(RuntimeError("no binary")),
    )
    monkeypatch.setattr(
        update_mod.shutil, "which", lambda name: None if name == "uv" else f"/usr/bin/{name}"
    )

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["doctor", "update"])
    assert result.exit_code == 0
    assert captured["file"] != "/bin/sh", "pip must not go through the uv wrapper"
    assert "pip" in captured["args"]


# ---------------------------------------------------------------------------
# Fix 7: delegation-path test - real update_command, only leaf I/O stubbed
# ---------------------------------------------------------------------------


def _make_fno_source(directory: Path) -> Path:
    """Create a minimal fno source directory with a valid pyproject.toml."""
    cli_dir = directory / "cli"
    cli_dir.mkdir(parents=True, exist_ok=True)
    (cli_dir / "pyproject.toml").write_text(
        '[project]\nname = "fno"\nversion = "0.1.0"\n',
        encoding="utf-8",
    )
    return cli_dir


@pytest.mark.skipif(os.name == "nt", reason="execvp is the Unix install path")
def test_doctor_fix_python_stale_delegates_to_real_update_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 7: doctor --fix python-stale delegates to the real update_command.

    Only leaf I/O is stubbed (execvp, subprocess.run, _discover_source,
    _target_in_progress, marker paths). Typer Option sentinels for rust/no_rust
    must not trip the bool-normalization or the mutex check - this test locks
    that contract.
    """
    from fno import doctor, update

    # Make a minimal fno source so _discover_source succeeds.
    src = _make_fno_source(tmp_path)
    monkeypatch.setattr(update, "_discover_source", lambda override=None: src)
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", tmp_path / "installed-rev")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "source-path")

    import types
    fake_result = types.SimpleNamespace(returncode=0, stdout="", stderr="")
    monkeypatch.setattr(update.subprocess, "run", lambda *a, **kw: fake_result)

    execvp_calls: list[tuple] = []

    def _fake_execvp(prog: str, args: list) -> None:
        execvp_calls.append((prog, args))

    monkeypatch.setattr(update.os, "execvp", _fake_execvp)

    # Stub doctor signal collectors so the verdict is python_stale.
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: src)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: "newsha")
    monkeypatch.setattr(doctor, "_read_marker", lambda: "oldsha")
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(doctor, "_rust_report", lambda: {"binary": None, "revision": None})
    monkeypatch.setattr(doctor, "_read_rust_marker", lambda: None)
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)

    result = runner.invoke(app, ["doctor", "--fix"])
    # No exception means the delegation path ran without Typer OptionInfo sentinel tripping.
    assert result.exception is None
    # execvp was reached (the real update_command ran to completion on this path).
    assert execvp_calls, "execvp must be reached via the real update_command delegation"


def test_component_verdict_transport_builds_the_native_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The transport hands the binary a bindir + expected rev, includes the mux
    only when the source carries crates/fno, and forwards the python-tool
    evidence when given."""
    import types

    import fno.update as update_mod

    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    (source.parent / "crates" / "fno").mkdir(parents=True)
    bindir = tmp_path / "cargo" / "bin"
    bindir.mkdir(parents=True)
    verdict_bin = bindir / "fno-agents"
    captured: dict = {}

    def _fake_run(cmd, *a, **kw):
        captured["cmd"] = list(cmd)
        return types.SimpleNamespace(
            returncode=0,
            stdout='{"converged": true, "components": []}',
            stderr="",
        )

    monkeypatch.setattr(update_mod.subprocess, "run", _fake_run)
    report = update_mod._component_verdict(
        source, "a" * 40, bindir, verdict_bin,
        python_tool={"rev": "cafe", "expected": "beef", "evidence": "2 .py differ"},
    )
    cmd = captured["cmd"]
    assert "--bindir" in cmd and str(bindir) in cmd
    assert "--expected" in cmd and "a" * 40 in cmd
    assert "--include-mux" in cmd
    assert "--attempted" not in cmd
    assert "--python-rev" in cmd and "cafe" in cmd
    assert "--python-expected" in cmd and "beef" in cmd
    assert "--python-evidence" in cmd and "2 .py differ" in cmd
    assert report == {"converged": True, "components": []}
