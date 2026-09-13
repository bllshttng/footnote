"""Shared fixtures for the ``fno agents`` test suite."""
from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _reset_once_per_process_notice(monkeypatch):
    """Keep one-shot rm notices isolated when xdist changes test grouping."""
    from fno.agents import rm_notice

    monkeypatch.delenv(rm_notice.NOTICE_SHOWN_ENV, raising=False)


@pytest.fixture(autouse=True)
def _collapse_pane_binding_window(monkeypatch):
    """Collapse the pane binding window so no test pays its wall-clock.

    Production waits `_BINDING_WINDOW_S` (8s) for a pane to bind a session: long
    enough to outlast a slow rollout, and comfortably under the 20s dispatch
    subprocess kill that a longer window would run into. That ceiling is only
    ever paid on the ambiguous path, but a test
    whose fake mux reports a permanently live, never-binding pane would sit out
    the whole thing. Tests that exercise the window pass ``window_s=``
    explicitly and ignore this.
    """
    monkeypatch.setenv("FNO_PANE_BINDING_WINDOW_S", "0.01")


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
    monkeypatch.setenv("CODEX_HOME", str(tmp_path_factory.mktemp("empty-codex-home")))
    # Same for the opencode lane. Defense-in-depth, NOT the load-bearing
    # barrier: the parent conftest redirects $HOME before any test module
    # imports, and the default resolves through expanduser("~"), so the real
    # store is already out of reach. This pins the lane directly so it survives
    # a refactor away from expanduser.
    empty_opencode = tmp_path_factory.mktemp("empty-opencode-storage")
    monkeypatch.setenv(discover.OPENCODE_STORAGE_DIR_ENV, str(empty_opencode))


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
def _stub_codex_cli_version(monkeypatch):
    """Never let a test's codex-argv build reach the real ``codex --version``.

    ``_codex_cli_version`` shells out with a bare ``subprocess.run`` (not the
    guarded ``_codex._subprocess_popen`` seam ``_block_live_provider_exec``
    covers), and it is ``functools.lru_cache``-memoized per process - so an
    unstubbed first call would both exec whatever real ``codex`` happens to be
    on the developer's PATH and then pin that version's answer for the rest
    of the pytest run, making --dangerously-bypass-hook-trust presence
    host-dependent. Pin to a known-supporting version; a test that needs a
    different answer overrides this per-test (monkeypatch order: test wins).
    """
    from fno.agents import mux_spawn

    monkeypatch.setattr(mux_spawn, "_codex_cli_version", lambda: (0, 148, 0))


@pytest.fixture(autouse=True)
def _isolate_spawn_uuid_capture(monkeypatch):
    """Keep spawn-time full-UUID resolution instant + host-independent.

    ``_claude_create_path`` best-effort resolves the full session UUID at spawn
    (ab-f1b0ccd1). By default the suite zeroes the retry backoff (no real sleep
    on the bounded window) and stubs the underlying registry reader to ``None``
    (no read of the developer's real ``~/.claude/sessions``), so a claude spawn
    leaves ``claude_session_uuid`` unresolved without slowing or host-coupling
    the suite. The spawn seed check reads claude's real job state the same
    way, so it is stubbed to verified. Tests that exercise resolution override
    these per-test (monkeypatch order: the test-local setattr wins).
    """
    from fno.agents.harnesses import _claude_session_registry, claude

    monkeypatch.setattr(claude, "_SPAWN_UUID_RETRY_BACKOFF_SEC", 0.0)
    monkeypatch.setattr(claude, "resolve_session_uuid", lambda short_id: None)
    monkeypatch.setattr(_claude_session_registry, "seed_unverified_reason", lambda *a, **k: None)


# The provider-exec guard lives in the ROOT cli/tests/conftest.py
# (_block_live_provider_exec) so every cli test is covered, not just this
# directory. x-ec81: the agents-only copy left 36 test files outside
# cli/tests/agents/ reaching a spawn seam unguarded, and test_spawn_guard.py
# leaked three live claude daemons. Do not re-add a copy here.
