"""Shared fixtures for the ``fno agents`` test suite."""
from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _isolate_session_discovery(monkeypatch, tmp_path_factory):
    """Point P1 live-session discovery (ab-098967b4) at an empty tmp dir.

    ``fno agents list`` discovers live sessions by default, reading Claude
    Code's ~/.claude/sessions registry. Without this, the suite would read the
    developer's real sessions dir and `agents list` JSON-shape assertions would
    be host-dependent. Tests in test_discover.py pass ``sessions_dir=``
    explicitly and are unaffected by this env override.
    """
    from fno.agents import discover

    empty = tmp_path_factory.mktemp("empty-claude-sessions")
    monkeypatch.setenv(discover.SESSIONS_DIR_ENV, str(empty))
    # Codex disk-discovery is pure mtime, not psutil-gated, so it would
    # read the developer's real ~/.codex/sessions unless isolated here.
    empty_codex = tmp_path_factory.mktemp("empty-codex-sessions")
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(empty_codex))


@pytest.fixture(autouse=True)
def _force_python_runtime(monkeypatch):
    """Default the agents tests to ``FNO_AGENTS_RUNTIME=python`` so in-process
    ``CliRunner`` invocations of routable verbs stay on the Python dispatch
    instead of ``os.execv``-replacing the pytest process with a real binary that
    happens to be on the developer's PATH (e.g. ``~/.cargo/bin/fno-agents``).

    This matters since the ab-73da4ac2 unconditional flip: ``ask`` now auto-routes
    for every provider, so ``runner.invoke(agents_app, ["ask", ...])`` would exec
    the installed binary and replace the test process whenever one is on PATH. CI
    has no installed binary, so the suite was green there; this fixture makes the
    local run match CI and removes the exec hazard. Setting the env var (rather
    than stubbing ``resolve_installed_binary``) keeps the binary-resolution unit
    tests intact and propagates to any ``python -m fno.cli`` subprocess a
    test spawns (project_rust_runtime_installed_local).

    Routing tests (``test_rust_runtime.py``) override this per-test via their own
    ``monkeypatch.delenv``/``setenv`` of ``FNO_AGENTS_RUNTIME`` (monkeypatch
    applies in order, so the test-local override wins). Parity tests
    (``test_rust_verb_parity.py``, ``test_ask_e2e_dispatch.py``) invoke the
    compiled binary directly via ``subprocess``; the binary ignores the env var,
    so they are unaffected.
    """
    from fno.agents import rust_runtime

    monkeypatch.setenv(rust_runtime.RUNTIME_ENV, "python")


@pytest.fixture(autouse=True)
def _isolate_spawn_uuid_capture(monkeypatch):
    """Keep spawn-time full-UUID resolution instant + host-independent.

    ``_claude_create_path`` best-effort resolves the full session UUID at spawn
    (ab-f1b0ccd1). By default the suite zeroes the retry backoff (no real sleep
    on the bounded window) and stubs the underlying registry reader to ``None``
    (no read of the developer's real ``~/.claude/sessions``), so a claude spawn
    leaves ``claude_session_uuid`` unresolved without slowing or host-coupling
    the suite. Tests that exercise resolution override these per-test
    (monkeypatch order: the test-local setattr wins).
    """
    from fno.agents.providers import claude

    monkeypatch.setattr(claude, "_SPAWN_UUID_RETRY_BACKOFF_SEC", 0.0)
    monkeypatch.setattr(claude, "resolve_session_uuid", lambda short_id: None)


@pytest.fixture(autouse=True)
def _block_live_provider_exec(request, monkeypatch, tmp_path_factory):
    """Guard the claude/codex/gemini subprocess seams so a test that reaches the
    Python dispatch path cannot exec a *real* provider binary and spawn a live
    session (e.g. an immortal ``claude --bg``).

    ``_force_python_runtime`` only keeps dispatch in-process; it does nothing
    about the provider subprocess underneath. The agents suite has repeatedly
    leaked live ``claude --bg`` sessions when a test drove ``dispatch_ask``
    without isolating PATH - the ambient real ``claude`` got exec'd and left a
    resident bg session that piles up and can be resumed later (ab-c1bf3552,
    generalizing PR #415, which fixed two such tests one at a time).

    The discriminator is *which* binary runs, not whether a subprocess runs at
    all: the safe pattern installs a fake claude/codex/gemini on a tmp-isolated
    PATH (``install_fake_*`` + ``monkeypatch.setenv("PATH", bin_dir)``) and lets
    the real seam exec the fake. This wrapper resolves the command's executable
    and raises only when it points at a binary OUTSIDE the pytest tmp tree (i.e.
    a real install). Tests that stub the seam with a Python callable replace this
    wrapper outright (monkeypatch order: test wins), so they are unaffected;
    out-of-process e2e/parity tests spawn a fresh interpreter and never reach
    this in-process patch.
    """
    # Real-provider smoke tests (@pytest.mark.smoke, e.g.
    # test_gemini_integration_smoke / test_codex_integration_smoke, run nightly
    # by provider-smoke.yml) intentionally exec the real binary; never guard
    # those (codex P2 review). Per-PR CI excludes `-m smoke`, so this only
    # matters for the nightly real-provider run.
    if request.node.get_closest_marker("smoke"):
        return

    import shutil
    from pathlib import Path

    from fno.agents.providers import claude as _claude
    from fno.agents.providers import codex as _codex
    from fno.agents.providers import gemini as _gemini

    # Use pytest's session basetemp (which honors a custom --basetemp) rather
    # than tempfile.gettempdir(), so the "is this a tmp-isolated fake?" check
    # stays correct under a non-default temp root (gemini review).
    tmp_root = str(tmp_path_factory.getbasetemp().resolve())
    provider_bins = {"claude", "codex", "gemini"}

    def _is_real_provider_exec(cmd) -> bool:
        argv0 = cmd[0] if isinstance(cmd, (list, tuple)) and cmd else cmd
        argv0 = str(argv0)
        if Path(argv0).name not in provider_bins:
            return False
        resolved = argv0 if Path(argv0).is_absolute() else (shutil.which(argv0) or "")
        if not resolved:
            # bare provider name with no fake on PATH: would resolve to the
            # ambient real binary (or fail), never an isolated fake -> block.
            return True
        return not str(Path(resolved).resolve()).startswith(tmp_root)

    def _guard(orig):
        def wrapper(cmd, *args, **kwargs):
            if _is_real_provider_exec(cmd):
                name = cmd[0] if isinstance(cmd, (list, tuple)) and cmd else cmd
                raise AssertionError(
                    f"live provider exec blocked under pytest: a test reached a real "
                    f"provider binary ({name!r}). Install a fake on a tmp-isolated PATH "
                    "(install_fake_claude/codex/gemini + monkeypatch.setenv PATH), stub "
                    "the seam (_subprocess_run/_subprocess_popen), or assert routing "
                    "without executing dispatch."
                )
            return orig(cmd, *args, **kwargs)

        return wrapper

    monkeypatch.setattr(_claude, "_subprocess_run", _guard(_claude._subprocess_run))
    monkeypatch.setattr(_codex, "_subprocess_popen", _guard(_codex._subprocess_popen))
    monkeypatch.setattr(_gemini, "_subprocess_popen", _guard(_gemini._subprocess_popen))
