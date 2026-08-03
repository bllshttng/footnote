"""Shared pytest fixtures for fno CLI tests."""
from __future__ import annotations

import os
import tempfile
from pathlib import Path

import pytest


_SERIAL_TEST_SUFFIXES = frozenset(
    {
        (
            "tests/unit/test_graph_sidecar_window.py::"
            "test_ac3hp_concurrent_writes_never_surface_corruption"
        ),
        (
            "tests/unit/test_mutex_steal.py::TestEventsMutex::"
            "test_AC3_FR_concurrent_stealers_both_land"
        ),
        (
            "tests/unit/test_mutex_steal.py::TestEventsMutex::"
            "test_AC3_FR_only_one_stealer_wins_the_rename"
        ),
        (
            "tests/agents/test_follow_signal.py::"
            "test_subprocess_follow_clean_sigint_exit"
        ),
        (
            "tests/agents/test_codex_signal_handling.py::"
            "test_create_sigint_mid_stream_propagates_and_releases_child"
        ),
    }
)


@pytest.hookimpl(tryfirst=True)
def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Keep filed parallel racers on one worker without skipping them."""
    for item in items:
        nodeid = item.nodeid.replace("\\", "/")
        if any(nodeid.endswith(suffix) for suffix in _SERIAL_TEST_SUFFIXES):
            item.add_marker(pytest.mark.serial)
            item.add_marker(pytest.mark.xdist_group(name="serial"))


@pytest.fixture(autouse=True)
def _stable_fno_py_cmd(monkeypatch):
    """Pin source self-shellouts to a bare ``["fno-py"]`` prefix (x-69b3).

    Source modules resolve the Python CLI via
    ``_subprocess_util.fno_py_cmd()``, which returns an ABSOLUTE path in most
    runner envs (the console script on PATH / beside the interpreter). Command
    assertions across the suite assert the bare ``["fno-py", <verb>, ...]``, so
    stub the resolver here to keep them stable regardless of how it resolves
    locally. The resolver's own unit tests bind the real function at import time
    and are unaffected by this module-attribute patch.
    """
    from fno import _subprocess_util

    monkeypatch.setattr(_subprocess_util, "fno_py_cmd", lambda: ["fno-py"])


# ---------------------------------------------------------------------------
# Hermetic state isolation (ab-2f78b48e)
# ---------------------------------------------------------------------------
# The fno.graph package freezes its path constants at IMPORT time -
# store.py does ``from _constants import GRAPH_JSON`` at module top and
# ``read_graph(path: Path = GRAPH_JSON)`` as a default arg - so the graph/ledger
# paths are bound to ``~/.fno`` before any per-test fixture can redirect
# them. Under cross-test contamination the graph store's fail-open
# (``Path.home() / ".fno"``) then leaked test nodes into the developer's
# REAL graph.json (observed: "promote via cli", "Stubbed node"). A fixture runs
# too late to help; the only test-side cure that beats a frozen constant is to
# move the home/state location BEFORE the import. conftest.py is imported by
# pytest before it imports the test modules that pull in fno.graph, so we
# redirect $HOME to a throwaway session dir here, at module load.
_REAL_HOME = os.environ.get("HOME") or os.path.expanduser("~")
_SESSION_HOME = tempfile.mkdtemp(prefix="fno-test-home-")
# Redirect both HOME (POSIX) and USERPROFILE (Windows, which Path.home() reads
# there) so the isolation holds regardless of platform (gemini review).
os.environ["HOME"] = _SESSION_HOME
os.environ["USERPROFILE"] = _SESSION_HOME
# Kill env-gated side effects the HOME redirect can't reach: born-with-why
# reads config.think_spawn.enabled from the AMBIENT repo (armed on a dev box),
# so fixture node births during tests spawned REAL `claude --bg` /think
# workers, burning tokens. FNO_THINK_SPAWN is the gate's highest-precedence
# input; hard-set it off (a shell-exported =1 must not leak either). Tests
# that exercise the spawn path re-arm per-test via monkeypatch.setenv.
os.environ["FNO_THINK_SPAWN"] = "0"
# Same class of hazard (x-c5cc): the spawn gate counts the HOST machine's
# real live workers (registry + claude roster), so a CLI-level spawn test on
# a busy dev box would queue for minutes behind processes the test doesn't
# own. Gate off suite-wide; the gate's own tests re-arm via monkeypatch.delenv.
os.environ["FNO_SPAWN_GATE"] = "0"
# Reap any mux server a test autospawns (x-4e30): the 2026-07-05 incident's
# leaked servers included Python-spawned `fno-test-home-*` ones, so Rust-only
# wiring would miss the population that crushed the machine. FNO_E2E arms the
# server's inactivity idle-exit (60s default grace), so a server orphaned by a
# SIGKILL'd / panic=abort'd / timed-out test self-exits instead of burning CPU
# for hours. spawn_server inherits the env (no env_clear), so the marker reaches
# even a client-autospawned setsid server.
os.environ["FNO_E2E"] = "1"
# Ambient origin provenance. Node-birth capture reads the running session's
# identity, so a suite run from inside a live /target worktree inherited that
# session's node as the origin of every node any test filed: the manifest's
# ownership proof passes (the session id genuinely matches), and FNO_NODE is
# exported by every fno-spawned worker. Harmless-looking until something reads
# the field back - then it is a foreign edge on a fixture node, or a receipt
# landing in output the test parses as JSON.
#
# Scrubbed at module load like the gates above. Tests that exercise capture arm
# what they need per-test via monkeypatch.setenv.
#
# The harness-marker half is DERIVED from harness_identity's own tuples, never
# retyped: a hand-maintained copy stops covering a marker the moment one is added,
# and the miss stays invisible until a suite run on a box that exports it. The
# legacy CLAUDE_SESSION_ID - which current_session_id() and current_session_ids()
# genuinely read - was already missing from the literal this replaces. The import
# is safe here despite the import-time-constant hazard above: harness_identity
# pulls only os/re/typing plus the typing-only harness_map, never fno.graph.
from fno.harness_identity import scrub_ambient_identity  # noqa: E402

for _ambient_key in ("FNO_NODE", "FNO_SLUG", "FNO_PLAN"):
    os.environ.pop(_ambient_key, None)
# Same local-red != CI-red hazard as the config ceiling below, one layer up.
# `resolve_plugin_script` takes CLAUDE_PLUGIN_ROOT as authoritative, so a suite
# run from inside a live Claude session resolves the DEVELOPER's checkout as the
# plugin payload - a hermetic fixture that builds its own source tree and passes
# it via FNO_REPO_ROOT is then silently overruled and compares against the wrong
# tree. CI exports neither var, so the failure only ever appears locally. Tests
# that exercise root resolution set what they need via monkeypatch.setenv.
for _ambient_key in ("CLAUDE_PLUGIN_ROOT", "CODEX_PLUGIN_ROOT"):
    os.environ.pop(_ambient_key, None)
scrub_ambient_identity()


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    """Remove the throwaway session HOME created for state isolation."""
    import shutil

    shutil.rmtree(_SESSION_HOME, ignore_errors=True)


@pytest.fixture(autouse=True, scope="session")
def _config_search_ceiling(tmp_path_factory: pytest.TempPathFactory):
    """Bound config resolution to the test tmpdirs.

    The config candidate chain climbs to the canonical checkout via
    ``git worktree list``, which the $HOME redirect above cannot bound (git
    ignores $HOME). Absent this, a full-suite run from the developer's real
    checkout reads its ~/.fno/config.toml, so local-red != CI-red. Pin the
    ceiling to the pytest basetemp AND the redirected HOME (the two roots any
    legitimate test config lives under); the real checkout falls outside both.
    """
    basetemp = tmp_path_factory.getbasetemp()
    os.environ["FNO_CONFIG_SEARCH_ROOT"] = os.pathsep.join(
        [str(basetemp), _SESSION_HOME]
    )
    yield
    os.environ.pop("FNO_CONFIG_SEARCH_ROOT", None)


@pytest.fixture(autouse=True, scope="session")
def _real_graph_leak_tripwire():
    """CI-only regression guard for ab-2f78b48e: fail the session if any test
    wrote a node into the developer's REAL ~/.fno/graph.json.

    With the $HOME redirect above this should be impossible; a non-empty delta
    means a test bypassed it (e.g. an absolute ~/.fno path). Gated on CI
    because a dev box may run a live walker/reconcile that legitimately mutates
    the real graph concurrently, which would false-positive. Node-id delta (not
    md5) is used so reconcile reformatting of existing nodes is ignored.
    """
    import json

    real_graph = Path(_REAL_HOME) / ".fno" / "graph.json"

    def node_ids() -> set[str]:
        try:
            data = json.loads(real_graph.read_text())
        except (OSError, ValueError):
            return set()
        entries = data.get("entries", []) if isinstance(data, dict) else data
        return {n.get("id") for n in entries if isinstance(n, dict) and n.get("id")}

    if not os.environ.get("CI"):
        yield
        return
    before = node_ids()
    yield
    leaked = node_ids() - before
    if leaked:
        pytest.fail(
            "tests leaked nodes into the real ~/.fno/graph.json "
            f"(ab-2f78b48e): {sorted(leaked)}",
            pytrace=False,
        )


# ---------------------------------------------------------------------------
# skip_when_implemented marker (#26)
# ---------------------------------------------------------------------------
# Loud-failure marker for reality_check stubs. The stubs return
# {"ok": False, "error": {"kind": "not-implemented", "domain": ...}}. When
# a real implementation lands, the not-implemented sentinel goes away.
# Tests decorated with @pytest.mark.skip_when_implemented(domain) then fail
# at setup, forcing the dev to remove or rewrite them.


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line(
        "markers",
        "skip_when_implemented(domain): fail if reality_check.<domain> stub "
        "has been implemented (returns kind != 'not-implemented').",
    )


def pytest_runtest_setup(item: pytest.Item) -> None:
    marker = item.get_closest_marker("skip_when_implemented")
    if marker is None:
        return
    if not marker.args:
        raise pytest.UsageError(
            "skip_when_implemented requires a domain argument: "
            "use @pytest.mark.skip_when_implemented('notion')"
        )
    domain = marker.args[0]
    module_name = f"fno.reality_check.{domain}"
    fn_name = f"check_{domain}"
    module = __import__(module_name, fromlist=[fn_name])
    check_fn = getattr(module, fn_name)
    result = check_fn()
    if result.get("error", {}).get("kind") != "not-implemented":
        pytest.fail(
            f"{item.name} should be removed; check_{domain} now implemented "
            f"(returned: {result!r})"
        )


@pytest.fixture(autouse=True)
def _clear_settings_cache() -> None:
    """Clear the load_settings() lru_cache before every test.

    Prevents test pollution when one test triggers load_settings() (e.g. via
    render_graph_html -> _load_obsidian_vault) and subsequent tests that
    monkeypatch FNO_CONFIG would otherwise get the cached result.

    Also resets config._loaded_from so paths.config_file() returns the correct
    path for the new test's settings file (Finding 3 fix isolation).
    """
    from fno import config as _cfg
    _cfg.load_settings.cache_clear()  # type: ignore[attr-defined]
    _cfg._loaded_from = None  # reset loaded_from tracker (Finding 3)
    # Also clear paths._settings and resolve_repo_root which have their own @cache
    try:
        import fno.paths as _paths
        if hasattr(_paths, "_settings"):
            _paths._settings.cache_clear()  # type: ignore[attr-defined]
        if hasattr(_paths, "resolve_repo_root"):
            _paths.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    except Exception:
        pass


MINIMAL_TARGET_STATE = """\
---
status: IN_PROGRESS
iteration: 1
session_id: 20260421T093631Z-97817-920dac
graph_id: ab-eea09178
---
# Target Session State

Initialized for testing.
"""

MINIMAL_MEGAWALK_STATE = """\
---
status: LOOPING
roadmap_id: rm-20260421-920dac
consecutive_failures: 0
total_cost_usd: 0.0
budget_cap_usd: 100.0
avg_task_cost: 5.0
tasks_completed_this_session: 0
---
# Megawalk State

Initialized for testing.
"""


@pytest.fixture
def tmp_state_file(tmp_path: Path) -> Path:
    """A temporary target-state.md with minimal valid content."""
    state = tmp_path / "target-state.md"
    state.write_text(MINIMAL_TARGET_STATE)
    return state


@pytest.fixture
def tmp_megawalk_state_file(tmp_path: Path) -> Path:
    """A temporary megawalk-state.md with minimal valid content."""
    state = tmp_path / "megawalk-state.md"
    state.write_text(MINIMAL_MEGAWALK_STATE)
    return state


@pytest.fixture
def clean_lock_dir(tmp_path: Path) -> Path:
    """A clean temp directory guaranteed to have no leftover .lock files."""
    lock_dir = tmp_path / "lock_dir"
    lock_dir.mkdir()
    yield lock_dir
    # Cleanup any leftover lock files after the test
    for lock_file in lock_dir.glob("*.lock"):
        try:
            lock_file.unlink()
        except OSError:
            pass
