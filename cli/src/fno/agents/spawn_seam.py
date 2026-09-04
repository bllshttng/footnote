"""The spawn seam: may this harness start on this substrate (x-a3e8).

The whole question and its refusals live here rather than inside dispatch,
which is over the shrink budget; this module is named by the question it
answers. The capability row is the only authority: ``features.spawn`` says
whether a seam arm is wired, and the four states get four answers.
"""

from __future__ import annotations


def check_spawn_harness(name: str, *, headless: bool = False) -> None:
    """Validate a harness at the thread/headless spawn seam.

    Substrate-aware: the row says which lanes exist, and a harness whose
    ``state_root_grant`` stance for the requested substrate reads
    ``"unmeasured"`` is refused here rather than silently inheriting a
    stance from the lanes that have run - the state-root gate downstream
    only refuses an ABSENT key, so an ``"unmeasured"`` value would pass it.
    """
    substrate = "headless" if headless else "thread"
    # The refusal type stays dispatch's (every caller catches it there);
    # the import is lazy so the seam can sit beside dispatch, not under it.
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.harness_map import (
        capabilities_or_undeclared,
        feature_claim,
        is_declared,
    )

    if not is_declared(name):
        # An undeclared harness HAS a lane - the pane - so this refusal
        # must name that lane rather than the accept set, and must not
        # name a successor harness the operator never mentioned.
        raise DispatchAskError(
            f"harness {name!r} declares no capability row (no entry in "
            "harness_capabilities.toml); --substrate pane is the only substrate "
            f"available to it ('fno agents spawn -H {name} --substrate pane' "
            "hosts the binary with fno as the viewport). A thread or headless "
            "lane needs the vendor's own protocol and must be measured first.\n"
            "If you meant a model VENDOR, that is -P/--provider.",
            exit_code=2,
        )
    state = feature_claim(name, "spawn")
    if state != "native":
        # capable and absent are different remedies, so they refuse in
        # different words: capable means fno has not wired the arm yet
        # (an fno-side gap), absent means there is no arm to wire. Both,
        # like unmeasured, name the probe that would settle the row.
        remedy = {
            "capable": (
                "exposes a spawn surface, but fno has no wired arm for it: "
                "the arm is unwired, which is fno's gap, not the harness's "
                "limitation. Wiring it takes a driver plus an unattended "
                "journey."
            ),
            "absent": (
                "has no spawn arm on this lane; the pane is its lane."
            ),
            "unmeasured": (
                "has not had its spawn lane measured; nobody has looked, "
                "so fno will not guess."
            ),
        }[state]
        raise DispatchAskError(
            f"harness {name!r} is refused on the {substrate} substrate: "
            f"features.spawn = {state!r} in harness_capabilities.toml. "
            f"{remedy} Settle it with 'fno agents harness probe {name}' "
            "when the instrument exists. "
            f"Use --substrate pane, which every harness hosts.",
            exit_code=2,
        )
    stance = capabilities_or_undeclared(name).get("state_root_grant", {}).get(
        substrate
    )
    # Absent refuses beside "unmeasured": a row that does not record a
    # stance for the lane has not measured it, and silence would let a
    # new member inherit a pass from the lanes that did run.
    if stance is None or stance == "unmeasured":
        raise DispatchAskError(
            f"{name!r} has a spawn arm, but its {substrate} lane is not "
            "measured: the capability row records state_root_grant."
            f"{substrate} = {stance!r} (absent or unmeasured), and nothing "
            "has run that lane unattended. An unattended journey for the "
            f"lane is what clears it (pi's thread journey is the shape: "
            "cli/tests/agents/test_pi_spawn_journey.py). "
            f"Use --substrate pane, which every harness hosts.",
            exit_code=2,
        )
