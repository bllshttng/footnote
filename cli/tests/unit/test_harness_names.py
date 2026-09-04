"""x-cec8: the harness-name set is platform-layer data, and importing the
platform layer no longer drags the runtime in at import time."""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def test_capability_keys_are_a_nonempty_subset_of_the_complete_roster():
    """Adding a capability row is a coupled change: the name lands in
    KNOWN_HARNESSES and a row lands in _HARNESS_CAPS, or the table's
    import-time assertion fires. The converse is deliberately NOT required:
    hermes and openclaw sit on the roster with no capability row, so the
    relation is subset, never equality."""
    from fno.agents.harness_map import _HARNESS_CAPS, known_harnesses
    from fno.harness_names import KNOWN_HARNESSES

    assert _HARNESS_CAPS, "capability table is empty"
    assert set(_HARNESS_CAPS) <= set(KNOWN_HARNESSES), sorted(
        set(_HARNESS_CAPS) - set(KNOWN_HARNESSES)
    )
    assert set(known_harnesses()) == set(_HARNESS_CAPS)
    assert known_harnesses() == sorted(known_harnesses())


def test_the_complete_roster_carries_the_evidence_backed_hosts():
    """AC1-HP: KNOWN_HARNESSES is the COMPLETE supported roster - the eight
    capability-backed names plus hermes and openclaw, which host real sessions
    per docs/SETUP-*.md. scripts/ci/check-harness-roster-parity.py holds this
    union against the shipped evidence surfaces in CI."""
    from fno.harness_names import KNOWN_HARNESSES

    assert set(KNOWN_HARNESSES) == {
        "claude",
        "codex",
        "gemini",
        "agy",
        "opencode",
        "pi",
        "cursor-agent",
        "grok",
        "hermes",
        "openclaw",
    }
    from fno.agents.harness_map import known_harnesses

    # The capability-backed roster stays at eight; the wider names ride the
    # roster only, which is the asymmetry this change exists to declare.
    assert set(known_harnesses()) == {
        "claude",
        "codex",
        "gemini",
        "agy",
        "opencode",
        "pi",
        "cursor-agent",
        "grok",
    }


def test_install_adapters_are_not_harness_evidence():
    """AC4-INSTALL: setup installation discovery (build_adapters) deliberately
    omits hermes/openclaw - their install surfaces are unverified (locked
    decision 4) - while the canonical roster carries both. build_adapters is
    an install surface, not harness evidence, so its narrower set stays legal
    beside the wider roster."""
    from fno.harness_names import KNOWN_HARNESSES
    from fno.setup.integration import build_adapters

    installed = {adapter.cli for adapter in build_adapters()}
    assert "hermes" not in installed
    assert "openclaw" not in installed
    assert {"hermes", "openclaw"} <= set(KNOWN_HARNESSES)


def test_thread_refusal_renders_from_the_roster_without_the_runtime():
    """x-d145: the pane-only sentence reads the roster beside the accept list,
    not the capability table. A rostered-but-unrowed name (hermes) gets it -
    the pane lane execs whatever is on PATH, so a row is not what earns one -
    and an unrecognized name gets no promise about a binary nobody looked for.

    The subprocess is the positive marker for the boundary the builder's
    caller depends on: `fno.graph.cli` renders this refusal at L1 core, so
    reaching a capability row would be an L1 -> L5 edge. Importing only
    fno.harness_names and rendering both branches must pull in no fno.agents.
    """
    from fno.harness_names import unknown_thread_harness_message

    rostered = unknown_thread_harness_message("hermes")
    assert "hermes has no measured thread lane yet; use --substrate pane." in rostered
    typo = unknown_thread_harness_message("claud")
    assert "use --substrate pane" not in typo
    assert "accepted here: " in typo

    repo_src = Path(__file__).resolve().parents[3] / "cli" / "src"
    env = dict(os.environ, PYTHONPATH=str(repo_src))
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            "import sys; from fno.harness_names import unknown_thread_harness_message as m; "
            "m('gemini'); m('claud'); "
            "print(sorted(k for k in sys.modules if k.startswith('fno.agents')) or 'NONE')",
        ],
        env=env,
        capture_output=True,
        text=True,
        check=True,
    )
    assert "NONE" in result.stdout, (
        f"the refusal builder dragged the runtime: {result.stdout.strip()}"
    )


def test_importing_dispatch_flags_does_not_drag_the_runtime():
    """x-cec8 defect: importing fno.dispatch_flags used to eagerly import
    fno.agents (via harness_identity -> harness_map). The name set now lives at
    L0, so the platform layer imports no runtime module at import time.

    Run in a SUBPROCESS: it makes the import genuinely fresh AND, critically,
    keeps this test from mutating the test process's sys.modules. Deleting
    fno.agents here would re-run its module body on the next import and split
    class identity (another module's AgentNameError vs a re-imported one), so a
    later test's ``pytest.raises(AgentNameError)`` would not catch the raised
    error and it would propagate. A subprocess cannot pollute the parent."""
    repo_src = Path(__file__).resolve().parents[3] / "cli" / "src"
    env = dict(os.environ, PYTHONPATH=str(repo_src))
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            "import sys, fno.dispatch_flags; "
            "print(sorted(k for k in sys.modules if k.startswith('fno.agents')) or 'NONE')",
        ],
        env=env,
        capture_output=True,
        text=True,
        check=True,
    )
    assert "NONE" in result.stdout, (
        f"dispatch_flags dragged the runtime at import: {result.stdout.strip()}"
    )
