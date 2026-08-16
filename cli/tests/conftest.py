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

_SERIAL_TEST_FILES = frozenset(
    {
        "tests/hooks/test_init_target_state_skip_flags.py",
    }
)


@pytest.hookimpl(tryfirst=True)
def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Keep filed parallel racers on one worker without skipping them."""
    for item in items:
        nodeid = item.nodeid.replace("\\", "/")
        serial_file = nodeid.split("::", 1)[0]
        if serial_file in _SERIAL_TEST_FILES or any(
            nodeid.endswith(suffix) for suffix in _SERIAL_TEST_SUFFIXES
        ):
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


@pytest.fixture(autouse=True)
def _neutral_host_harness(monkeypatch):
    """Keep synthetic session markers independent of the pytest host harness.

    Identity tests set their own Claude, Codex, or Gemini markers.  A real
    harness ancestor belongs to the test runner rather than the synthetic
    session, so letting the process-tree prover observe it makes local results
    depend on which harness launched pytest.  Tests of proven ownership pin the
    resolver explicitly after this fixture runs.
    """
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness",
        lambda from_pid=None: None,
    )


@pytest.fixture(autouse=True)
def _hermetic_promise_carveout_gate(monkeypatch):
    """Default condition D of the promise gate to an empty carve-out ledger.

    ``resolve_promise_evidence`` (consulted by every close verb) reads
    ``.fno/carveouts.jsonl`` off the canonical repo root; in the test process
    that resolves to the real checkout, so it would read the real ledger and
    refuse every close. Default the reader to empty so unrelated close tests
    stay hermetic; tests that exercise condition D override it (``carveout_reader``
    on the unit gate, or re-patch this attribute for the close verbs).
    """
    import fno.graph._reconcile as rec

    monkeypatch.setattr(rec, "_unharvested_deferred_carveouts", lambda cwd: [])


@pytest.fixture(autouse=True)
def _hermetic_claim_reap(monkeypatch):
    """Neutralise cmd_reconcile's claim-GC leg during tests.

    ``reap_dead_claims()``'s default roots resolve through
    ``resolve_canonical_repo_root()`` (cwd-based ``git worktree list``
    discovery - the SAME channel hermetic.py documents as unclosable by any
    env var, see its ``FNO_REPO_ROOT`` comment) and ``global_claims_root()``
    (HOME, which IS sandboxed). A reconcile test run from inside a real
    checkout would otherwise archive real dead claims on the host machine.
    Closed at the reader, same pattern as ``_hermetic_promise_carveout_gate``
    above. ``cmd_reconcile`` imports ``reap_dead_claims`` fresh inside the
    function body, so patching the defining module's attribute reaches it;
    a test module that imports the name directly (``test_claim_reap.py``)
    binds its own reference at collection time and is unaffected.
    """
    import fno.claims.core as claims_core

    def _noop_reap(*, roots=None, apply=False):
        return {
            "scanned": 0, "reaped": 0, "would_reap": 0, "kept_live": 0,
            "kept_suspect": 0, "kept_offhost": 0, "corrupted": 0, "vanished": 0,
            "contended": 0, "reap_failed": [], "apply": apply, "roots": [],
        }

    monkeypatch.setattr(claims_core, "reap_dead_claims", _noop_reap)


# ---------------------------------------------------------------------------
# Hermetic state isolation
# ---------------------------------------------------------------------------
# Applied at MODULE LOAD, not as a fixture. The fno.graph package freezes its
# path constants at IMPORT time - store.py does ``from _constants import
# GRAPH_JSON`` at module top and ``read_graph(path: Path = GRAPH_JSON)`` as a
# default arg - so the graph/ledger paths bind to ``~/.fno`` before any per-test
# fixture can redirect them. Under cross-test contamination the graph store's
# fail-open (``Path.home() / ".fno"``) then leaked test nodes into the
# developer's REAL graph.json. A fixture runs too late; the only cure that beats
# a frozen constant is to move the state location BEFORE the import, and
# conftest.py is imported before the test modules that pull in fno.graph.
#
# WHAT gets neutralised lives in fno/hermetic.py, deny-by-default over the whole
# ambient surface, so this file no longer carries a hand-maintained list that
# loses to the next var somebody adds. `fno test` applies the same function at
# `_child_env`, which covers the shell and cargo trees; this call is what covers
# a developer running a bare `pytest cli/tests/...`.
#
# The import is safe despite the import-time-constant hazard above: hermetic
# pulls only os/pathlib/typing plus harness_identity, never fno.graph.
from fno.hermetic import neutralise  # noqa: E402

_REAL_HOME = os.environ.get("HOME") or os.path.expanduser("~")
_SANDBOX = tempfile.mkdtemp(prefix="fno-test-sandbox-")
_hermetic_env = neutralise(os.environ, Path(_SANDBOX))
os.environ.clear()
os.environ.update(_hermetic_env)


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    """Remove the throwaway sandbox created for state isolation."""
    import shutil

    shutil.rmtree(_SANDBOX, ignore_errors=True)


@pytest.fixture(autouse=True, scope="session")
def _config_search_ceiling(tmp_path_factory: pytest.TempPathFactory):
    """Widen the config ceiling to include the pytest basetemp.

    ``neutralise`` pins the ceiling to the sandbox, which is the right default
    for every caller. Tests additionally write settings files under ``tmp_path``,
    and that basetemp is only known once pytest has started, so it is appended
    here rather than being another thing hermetic.py has to guess about.
    """
    basetemp = str(tmp_path_factory.getbasetemp())
    previous = os.environ.get("FNO_CONFIG_SEARCH_ROOT", "")
    os.environ["FNO_CONFIG_SEARCH_ROOT"] = os.pathsep.join(
        [basetemp, previous] if previous else [basetemp]
    )
    yield
    os.environ["FNO_CONFIG_SEARCH_ROOT"] = previous


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
def cwd_tmp(tmp_path: Path, monkeypatch):
    """Collapse both claims roots (global + cwd-local) onto one tmp dir.

    HOME=cwd means global_claims_root() and the canonical-repo-root
    claims_dir(None) are the SAME directory, so a test can write with plain
    acquire_claim(root=tmp_path) and know both of `list`/`reap`'s default
    roots see it. Shared by test_claim_reap.py and test_claims_cli.py (both
    exercise the same claims CLI surface against a hermetic HOME).
    """
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    yield tmp_path


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


@pytest.fixture(autouse=True)
def _no_review_coverage_recompute(monkeypatch):
    """Hermetic default for the coverage recompute (x-3a3f).

    `fno pr merge`/`status` recompute a missing coverage row by shelling out to
    the `fno-agents review-coverage` verb. In the test environment that
    resolver can find a real installed binary (PATH or the cargo dev target),
    so an unstubbbed no-event path would spawn it against real gh. This stub
    makes the recompute report "unavailable" by default - the fail-closed
    branch, which is also the pre-recompute behavior, so only tests that
    explicitly exercise the recompute (they re-monkeypatch
    `fno.pr._reviews._fire_review_coverage_verb`) see one.
    """
    from fno.pr import _reviews

    monkeypatch.setattr(
        _reviews, "_fire_review_coverage_verb", lambda *a, **k: (False, "disabled in test")
    )
