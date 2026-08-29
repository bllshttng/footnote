"""Loop participation: can the fno target loop CLOSE on this harness?

The field replaced ``stop_hook``, which read ``native`` on every row and had no
consumer, so nothing here would have noticed when it stopped being true. These
tests are what makes the new field load-bearing: the measured value per harness,
the artifact behind an ``extension`` row, and the refusal that stops a looping
dispatch from producing a worker with nothing to stop it.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.harness_map import (
    DispatchResolveError,
    capabilities,
    check_loop_participation,
    resolve_dispatch,
)
from fno.harness_names import KNOWN_HARNESSES

REPO_ROOT = Path(__file__).resolve().parents[3]

# The measurement, 2026-08-28. Each value was read off the artifact and the
# wiring that reaches it; the table's own comment carries the evidence per row.
MEASURED = {
    "claude": "native",
    "codex": "native",
    "agy": "native",
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
    values = {capabilities(h)["loop_participation"] for h in KNOWN_HARNESSES}
    assert len(values) > 1, values


# A field allowed to hold one value on every row, with the reason it is a
# measured result rather than an inherited default. Empty today, and adding to
# it is the moment to prove the uniformity rather than assume it.
UNIFORM_BY_MEASUREMENT: dict[str, str] = {}


def test_no_capability_field_is_uniform_across_every_harness():
    """The guard for the defect this node closed, aimed at the NEXT field.

    ``stop_hook`` held one value for six harnesses because nothing would have
    noticed a second one was needed, and ``send_keys_enter_delay_ms`` held one
    default across four before that. A field with one distinct value has not
    been measured for any row. ``scripts/diagnostics/capability-honesty-sweep.py``
    prints the same pass with two more the uniformity check cannot see.
    """
    import json
    import tomllib

    table = tomllib.loads(
        (REPO_ROOT / "crates/fno-agents/src/harness_capabilities.toml").read_text()
    )
    rows = table["harness"]

    def flatten(prefix, value, out):
        if isinstance(value, dict):
            for key, sub in value.items():
                flatten(f"{prefix}.{key}" if prefix else key, sub, out)
        else:
            out[prefix] = json.dumps(value, sort_keys=True)

    flat = {}
    for harness, caps in rows.items():
        collected: dict[str, str] = {}
        flatten("", caps, collected)
        flat[harness] = collected

    fields = {key for row in flat.values() for key in row}
    # Non-vacuity: an empty field set would make the assertion below pass while
    # measuring nothing, which is the absence-reads-as-success trap.
    assert len(flat) == len(KNOWN_HARNESSES)
    assert len(fields) > 30, len(fields)
    uniform = {
        field
        for field in fields
        if len({flat[h].get(field, "<absent>") for h in flat}) == 1
    }
    assert uniform <= set(UNIFORM_BY_MEASUREMENT), sorted(
        uniform - set(UNIFORM_BY_MEASUREMENT)
    )


def test_a_declared_loop_extension_exists_on_disk():
    """An ``extension`` row's artifact is the thing that closes its loop.

    Declaring a path that has been deleted or renamed is exactly the class of
    false-but-parseable value this field exists to remove, so the claim is
    checked against the tree rather than trusted.
    """
    named = {
        h: capabilities(h)["loop_extension"]
        for h in KNOWN_HARNESSES
        if capabilities(h)["loop_extension"]
    }
    assert named, "no harness declares a loop extension; the check would be vacuous"
    for harness, rel in named.items():
        assert (REPO_ROOT / rel).is_file(), f"{harness} names a missing artifact: {rel}"


@pytest.mark.parametrize("command", ["/target x-1", "/fno:target x-1", "$fno:target x-1"])
def test_a_harness_with_no_loop_boundary_is_refused(command):
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("gemini", command)
    message = str(exc.value)
    assert "gemini" in message
    assert "loop_participation" in message
    assert "never stop" in message


def test_an_extension_harness_without_a_shipped_artifact_is_refused():
    """pi's command_surface is ``slash``, so a /target resolves fine there.

    Without this refusal the dispatch succeeds and the worker runs with nothing
    to stop it, which is a hang no instrument reports.
    """
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("pi", "/target x-1")
    assert "pi" in str(exc.value)
    assert "has not written yet" in str(exc.value)


def test_an_extension_harness_with_a_shipped_artifact_is_dispatched():
    check_loop_participation("opencode", "/fno:target x-1")


@pytest.mark.parametrize("harness", ["claude", "codex", "agy"])
def test_a_native_harness_is_dispatched(harness):
    check_loop_participation(harness, "/target x-1")


@pytest.mark.parametrize("harness", sorted(MEASURED))
def test_a_non_looping_dispatch_is_never_refused(harness):
    """The gate is scoped to the /target family, so a one-shot passes untouched.

    A harness that cannot close a loop can still run research, a review, or any
    command that ends on its own.
    """
    check_loop_participation(harness, "/think what breaks here")
    check_loop_participation(harness, "opencode run --command build")
    check_loop_participation(harness, "")


def test_resolve_dispatch_refuses_a_looping_target_at_pi():
    with pytest.raises(DispatchResolveError) as exc:
        resolve_dispatch(
            harness="pi", substrate="pane", trigger="attended", node_id="x-1"
        )
    assert "loop_participation" in str(exc.value)


def test_resolve_dispatch_still_resolves_a_looping_target_at_claude():
    resolved = resolve_dispatch(harness="claude", node_id="x-1")
    assert resolved["command"].startswith("/target")
    assert resolved["loop_participation"] == "native"
