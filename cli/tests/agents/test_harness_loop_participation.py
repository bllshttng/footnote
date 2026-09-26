"""Loop participation: can the fno target loop CLOSE on this harness?

The field replaced ``stop_hook``, which read ``native`` on every row and had no
consumer, so nothing here would have noticed when it stopped being true. These
tests are what makes the new field load-bearing: the measured value per harness,
the artifact behind an ``extension`` row, and the refusal that stops a looping
dispatch from producing a worker with nothing to stop it.

The decision itself lives in ``capability_leaves.rs`` (the ``fno-agents status
--target-family --harness`` leaf); the Rust test module carries the refusal
table. What stays here is the measured row per harness, the resolver and spawn
seams that call the gate, and one end-to-end door test on a dev build.

Every measurement here reads the capability-backed roster (``known_harnesses``),
never the complete ``KNOWN_HARNESSES`` roster: hermes and openclaw are supported
identities with no capability row, and loop participation is a property of the
row, so there is nothing to measure for a row-less harness.
"""
from __future__ import annotations

from pathlib import Path

import pytest

import fno.agents.harness_map as harness_map
from fno.agents.harness_map import (
    DispatchResolveError,
    capabilities,
    check_loop_participation,
    known_harnesses,
    resolve_dispatch,
)
from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)

# Captured at import time, before the autouse ``_hermetic_loop_gate``
# conftest fixture stubs the module attribute: the door test re-binds the
# real leaf answer so the real binary is driven end to end.
_REAL_LOOP_GATE_ANSWER = harness_map._loop_gate_answer

REPO_ROOT = Path(__file__).resolve().parents[3]

# The measurement, 2026-08-28 (grok flipped with its gate, 2026-09-26). Each
# value was read off the artifact and the wiring that reaches it; the table's
# own comment carries the evidence per row.
MEASURED = {
    "claude": "native",
    "codex": "native",
    "agy": "native",
    "grok": "native",
    "gemini": "none",
    "opencode": "extension",
    "pi": "extension",
}


@pytest.mark.parametrize("harness", sorted(MEASURED))
def test_every_harness_declares_its_measured_loop_participation(harness):
    assert capabilities(harness)["loop_participation"] == MEASURED[harness]


def test_the_table_is_not_uniform():
    """One value across every row is an inherited declaration, not a measurement.

    This is the whole defect shape: ``stop_hook`` held one value for six
    harnesses because nobody ever had to defend a second one.
    """
    values = {capabilities(h)["loop_participation"] for h in known_harnesses()}
    assert len(values) > 1, values


def test_a_declared_loop_extension_exists_on_disk():
    """An ``extension`` row's artifact is the thing that closes its loop.

    Declaring a path that has been deleted or renamed is exactly the class of
    false-but-parseable value this field exists to remove, so the claim is
    checked against the tree rather than trusted.
    """
    named = {
        h: capabilities(h)["loop_extension"]
        for h in known_harnesses()
        if capabilities(h)["loop_extension"]
    }
    assert named, "no harness declares a loop extension; the check would be vacuous"
    for harness, rel in named.items():
        assert (REPO_ROOT / rel).is_file(), f"{harness} names a missing artifact: {rel}"


@pytest.mark.parametrize("harness", sorted(MEASURED))
def test_a_non_looping_dispatch_is_never_refused(harness):
    """The gate is scoped to the /target family, so a one-shot passes untouched.

    A harness that cannot close a loop can still run research, a review, or any
    command that ends on its own.
    """
    check_loop_participation(harness, "/think what breaks here")
    check_loop_participation(harness, "opencode run --command build")
    check_loop_participation(harness, "")
    # A whitespace-only message has no first token. It used to raise IndexError
    # out of the shared family test, which three callers reach.
    check_loop_participation(harness, "   ")
    check_loop_participation(harness, "\t\n")


def test_the_gate_refusal_raises_at_the_caller(monkeypatch):
    """The Python seam is one leaf call: whatever refusal the leaf answers,
    ``check_loop_participation`` raises verbatim at the dispatch caller."""
    monkeypatch.setattr(
        harness_map,
        "_loop_gate_answer",
        lambda h, c: {
            "refusal": "refused: the grok plugin status reads absent - a loop "
            "whose stop gate never runs would never stop."
        },
    )
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("grok", "/fno:target x-1")
    assert "absent" in str(exc.value)
    assert "never stop" in str(exc.value)


def test_an_unreadable_leaf_refuses_never_admits(monkeypatch):
    """A missing or older binary must fail closed: an unreadable gate answers
    a refusal, so no harness is waved through on a broken door."""
    import fno.rust_binary

    def _missing(verb, args, **kwargs):
        return ("fno-agents binary not found", None)

    monkeypatch.setattr(fno.rust_binary, "call_binary_json", _missing)
    # The autouse hermetic stub answers without the transport; re-bind the
    # real answer so the stubbed transport is what the caller drives.
    monkeypatch.setattr(harness_map, "_loop_gate_answer", _REAL_LOOP_GATE_ANSWER)
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("gemini", "/target x-1")
    message = str(exc.value)
    assert "could not be read" in message
    assert "fno doctor update --rust" in message


def test_resolve_dispatch_resolves_a_looping_target_at_pi(monkeypatch):
    """pi's loop extension shipped, so the resolver that used to
    refuse a looping /target here now resolves it: the worker has something
    to stop it."""
    monkeypatch.setattr(harness_map, "_loop_gate_answer", lambda h, c: {"refusal": None})
    resolved = resolve_dispatch(
        harness="pi", substrate="pane", trigger="attended", node_id="x-1"
    )
    assert resolved["loop_participation"] == "extension"


def test_resolve_dispatch_still_resolves_a_looping_target_at_claude():
    resolved = resolve_dispatch(harness="claude", node_id="x-1")
    assert resolved["command"].startswith("/target")
    assert resolved["loop_participation"] == "native"


def test_the_direct_spawn_seam_still_calls_the_gate():
    """`fno agents spawn` never reaches resolve_dispatch, by its own comment.

    Guarding only the resolver would have covered every path but the one
    operators use most to launch a target worker. The spawn seam already
    re-applies the merge-posture vocabulary for the same reason, and the loop
    gate rides beside it.

    This asserts the CALL is present, which is weaker than driving the CLI. It
    is here because a spawn invocation in a unit test would move directories and
    reach a mux; the refusal itself was measured against the live verb, and this
    catches the call being dropped.
    """
    source = (REPO_ROOT / "cli/src/fno/agents/cli.py").read_text()
    assert "check_loop_participation(harness, message)" in source


@requires_rust
def test_the_real_door_refuses_a_looping_dispatch_at_gemini(monkeypatch):
    """End to end on the dev build: the leaf reads the packaged row (gemini:
    none) and the caller raises its ``never stop`` refusal. This is the door
    wiring test - a stubbed seam cannot catch a broken argument list."""
    binary = find_dev_binary()
    assert binary is not None
    monkeypatch.setenv("FNO_AGENTS_BIN", str(binary))
    monkeypatch.setattr(harness_map, "_loop_gate_answer", _REAL_LOOP_GATE_ANSWER)
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("gemini", "/target x-1")
    assert "never stop" in str(exc.value)
