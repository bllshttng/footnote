"""Loop participation: can the fno target loop CLOSE on this harness?

The field replaced ``stop_hook``, which read ``native`` on every row and had no
consumer, so nothing here would have noticed when it stopped being true. These
tests are what makes the new field load-bearing: the measured value per harness,
the artifact behind an ``extension`` row, and the refusal that stops a looping
dispatch from producing a worker with nothing to stop it.

Every measurement here reads the capability-backed roster (``known_harnesses``),
never the complete ``KNOWN_HARNESSES`` roster: hermes and openclaw are supported
identities with no capability row, and loop participation is a property of the
row, so there is nothing to measure for a row-less harness.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.harness_map import (
    DispatchResolveError,
    capabilities,
    check_loop_participation,
    known_harnesses,
    resolve_dispatch,
)

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
    values = {capabilities(h)["loop_participation"] for h in known_harnesses()}
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
    assert len(flat) == len(known_harnesses())
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
        for h in known_harnesses()
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


def test_an_extension_harness_without_a_shipped_artifact_is_refused(monkeypatch):
    """pi's command_surface is ``slash``, so a /target resolves fine there.

    Without this refusal the dispatch succeeds and the worker runs with nothing
    to stop it, which is a hang no instrument reports. pi's artifact shipped
    (x-43bd), so the empty-artifact shape is asserted by blanking the row's
    path - the refusal must come from the CONTRACT, not from one harness's
    accident of shipping.
    """
    import fno.agents.harness_map as harness_map

    real_caps = harness_map.capabilities

    def caps_without_artifact(harness):
        caps = dict(real_caps(harness))
        caps["loop_extension"] = ""
        return caps

    monkeypatch.setattr(harness_map, "capabilities", caps_without_artifact)
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("pi", "/target x-1")
    assert "pi" in str(exc.value)
    assert "has not written yet" in str(exc.value)


def test_an_extension_harness_with_an_installed_artifact_is_dispatched(monkeypatch):
    """Shipping the artifact is not enough: the gate requires the INSTALLED
    copy at the harness's own load surface, because that is the only copy the
    harness actually loads."""
    import fno.setup.integration as integration

    monkeypatch.setattr(integration, "_opencode_is_installed", lambda: True)
    monkeypatch.setattr(integration, "_pi_is_installed", lambda: True)
    check_loop_participation("opencode", "/fno:target x-1")
    # pi joined in x-43bd: the installed footnote.ts extension satisfies the gate.
    check_loop_participation("pi", "/target x-1")


def test_an_extension_harness_without_the_artifact_installed_is_refused(monkeypatch):
    """pi on PATH with setup never run: the extension is not at pi's load
    surface, so a looping dispatch would start a worker with nothing to stop
    it. The refusal names the install path out."""
    import fno.setup.integration as integration

    monkeypatch.setattr(integration, "_pi_is_installed", lambda: False)
    with pytest.raises(DispatchResolveError) as exc:
        check_loop_participation("pi", "/target x-1")
    message = str(exc.value)
    assert "pi" in message
    assert "fno config setup" in message
    assert "absent or stale" in message


def test_an_extension_harness_with_no_declared_installer_is_refused(monkeypatch):
    """An extension row without an install arm is a gap, not a claim: the
    checker treats it as not installed so the row and its installer ship
    together, the way opencode's and pi's did."""
    import fno.agents.harness_map as harness_map
    import fno.setup.integration as integration

    monkeypatch.setattr(integration, "_opencode_is_installed", lambda: True)
    monkeypatch.setattr(integration, "_pi_is_installed", lambda: True)
    monkeypatch.setattr(
        harness_map,
        "capabilities",
        lambda h: {
            "loop_participation": "extension",
            "loop_extension": "cli/src/fno/setup/assets/future/footnote.ts",
        },
    )
    with pytest.raises(DispatchResolveError):
        check_loop_participation("future", "/target x-1")


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
    # A whitespace-only message has no first token. It used to raise IndexError
    # out of the shared family test, which three callers reach.
    check_loop_participation(harness, "   ")
    check_loop_participation(harness, "\t\n")


def test_resolve_dispatch_resolves_a_looping_target_at_pi(monkeypatch):
    """pi's loop extension shipped (x-43bd), so the resolver that used to
    refuse a looping /target here now resolves it: the worker has something
    to stop it."""
    import fno.agents.harness_map as harness_map

    monkeypatch.setattr(harness_map, "_loop_extension_installed", lambda h: True)
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
