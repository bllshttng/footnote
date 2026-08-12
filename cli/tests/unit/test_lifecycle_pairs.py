"""Every lifecycle transition names its correction, or says why it has none.

The defect this gate exists to stop is not three missing verbs, it is a habit:
the state machine's forward edges ship and correction is treated as out of
scope. Three instances were reported on 2026-08-11 (no inverse for ``done``, a
``remove`` that exists but names itself nowhere, no in-place edit for a
carve-out) and the audit that followed immediately found a fourth (``archive``).
Fixing them one at a time leaves the fifth to be discovered the same way, so the
rule gets a test instead of a resolution.

Two properties make this hold rather than decorate:

1. The table is checked against the LIVE typer apps, never a second hardcoded
   inventory. A second inventory goes stale on the first rename and keeps
   passing while the real surface drifts.
2. Discoverability is asserted as well as existence. ``fno backlog remove`` was
   real, working, and undiscoverable for long enough that an agent ruled no
   delete verb existed and cost another project 23 nodes it believed
   un-file-able. A verb that cannot be found is, for decision-making purposes, a
   verb that does not exist, so "the inverse exists" is not the assertion -
   "the forward verb names its inverse" is.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import click
import pytest
import typer.main

from fno.carveout.cli import carveout_app
from fno.graph.cli import cli as backlog_app

APPS = {"backlog": backlog_app, "carveout": carveout_app}


@dataclass(frozen=True)
class Pair:
    """One lifecycle transition and its correction.

    ``inverse=None`` means the transition genuinely cannot be corrected, and
    ``reason`` must then say why in words the forward verb's own help repeats.
    ``forward=None`` means the correcting verb's forward transition lives
    outside this app, so there is no local help text to carry the pointer.
    """

    app: str
    forward: Optional[str]
    inverse: Optional[str]
    reason: Optional[str] = None


LIFECYCLE_PAIRS: tuple[Pair, ...] = (
    # -- status transitions --
    Pair("backlog", "defer", "undefer"),
    Pair("backlog", "queue", "unqueue"),
    Pair("backlog", "supersede", "unsupersede"),
    Pair("backlog", "done", "reopen"),
    # reconcile closes nodes off merged PRs, so it reaches the same terminal
    # state `done` does and is corrected by the same verb.
    Pair("backlog", "reconcile", "reopen"),
    Pair("backlog", "archive", "unarchive"),
    # -- existence transitions --
    Pair("backlog", "add", "remove"),
    Pair("backlog", "idea", "remove"),
    Pair("backlog", "new", "remove"),
    Pair("backlog", "intake", "remove"),
    # -- self-inverse: the same verb reverses itself --
    Pair("backlog", "rank", "rank"),
    Pair("backlog", "update", "update"),
    # -- corrections whose forward transition is not a verb in this app --
    Pair(
        "backlog",
        None,
        "unclaim",
        "the forward transition is a claim acquired by `fno target init`",
    ),
    Pair("backlog", None, "release", "alias of `unclaim`; same forward"),
    # -- carve-outs --
    Pair("carveout", "add", "update"),
)

# Every command each app advertises or hides, as of this commit. Its only job is
# to fail when the surface changes, so a NEW verb forces a one-line
# classification decision at authoring time rather than being discovered by an
# operator two months later. Add the name here AND either give it a row above or
# accept that it lands in the derived non-transition set.
KNOWN_COMMANDS: dict[str, frozenset[str]] = {
    "backlog": frozenset({
        "add", "advance", "archive", "backfill-slugs", "bases", "batch",
        "briefs", "capture", "collisions", "cost", "decompose", "defer",
        "dispatch-lanes", "done", "epic", "exec-graph", "find", "get", "groom",
        "idea", "intake", "lane-fill", "lanes", "maintain", "new",
        "next", "note", "pick", "project-root", "provenance", "queue", "queued",
        "rank", "ready", "reconcile", "reconcile-findings", "rehash",
        "relatedness", "release", "remove", "reopen", "reprioritize", "roadmap",
        "schedule", "session", "status", "supersede", "tree", "triage",
        "unarchive", "unclaim", "undefer", "unqueue", "unsupersede", "update",
        "validate", "view",
    }),
    "carveout": frozenset({"add", "list", "resolve", "update"}),
}


def _commands(app_name: str) -> dict[str, click.Command]:
    """The live command registry for one app, read the way `--help` reads it."""
    group = typer.main.get_command(APPS[app_name])
    ctx = click.Context(group)
    out: dict[str, click.Command] = {}
    for name in group.list_commands(ctx):
        cmd = group.get_command(ctx, name)
        if cmd is not None:
            out[name] = cmd
    return out


def _help_text(cmd: click.Command) -> str:
    """Everything a caller reading `--help` sees: docstring plus epilog.

    Both halves count. The epilog is where the paired-verb pointer lives (the
    pattern `cmd_defer` established), and the docstring is where a verb explains
    itself; a pointer in either one is findable.
    """
    return f"{cmd.help or ''}\n{cmd.epilog or ''}"


def _names_its_inverse(app: str, forward: str, inverse: str) -> bool:
    cmds = _commands(app)
    if forward not in cmds:
        return False
    return inverse in _help_text(cmds[forward])


def test_the_table_is_well_formed():
    """A row that names neither side classifies nothing."""
    for pair in LIFECYCLE_PAIRS:
        assert pair.forward or pair.inverse, f"empty row: {pair}"
        if pair.inverse is None or pair.forward is None:
            assert pair.reason, f"row {pair} must say why one side is absent"


def test_every_named_verb_resolves_against_the_live_app():
    """A rename fails here, not in a stale doc."""
    missing: list[str] = []
    for pair in LIFECYCLE_PAIRS:
        cmds = _commands(pair.app)
        for verb in (pair.forward, pair.inverse):
            if verb is not None and verb not in cmds:
                missing.append(f"fno {pair.app} {verb}")
    assert not missing, (
        "named in LIFECYCLE_PAIRS but absent from the live app: "
        + ", ".join(sorted(set(missing)))
    )


def test_the_resolver_can_fail():
    """Positive control for the test above.

    Without this, a resolver that returned every name as present would pass the
    existence check forever while the surface rotted underneath it.
    """
    assert "no-such-verb" not in _commands("backlog")


def test_every_forward_verb_names_its_inverse():
    """The assertion that would have caught the `remove` instance.

    `fno backlog remove` existed and worked; nothing a caller deciding whether a
    node was deletable would read ever mentioned it.
    """
    silent: list[str] = []
    for pair in LIFECYCLE_PAIRS:
        if pair.forward is None or pair.inverse is None:
            continue
        if pair.forward == pair.inverse:
            continue  # self-inverse: the pointer would point at itself
        if not _names_its_inverse(pair.app, pair.forward, pair.inverse):
            silent.append(
                f"`fno {pair.app} {pair.forward}` never names `{pair.inverse}`"
            )
    assert not silent, (
        "a correction nobody can find is a correction that does not exist:\n  "
        + "\n  ".join(silent)
        + "\nFix: add `epilog=\"Paired verb: ...\"` to the forward command "
        "(the pattern `cmd_defer` uses)."
    )


def test_the_help_check_can_fail():
    """Positive control for the test above: a verb that names nothing fails it."""
    assert not _names_its_inverse("backlog", "get", "no-such-inverse")


def test_a_transition_with_no_inverse_states_why_in_its_own_help():
    """`inverse=None` is allowed, silence is not.

    No row currently takes this branch, so the positive control below is what
    keeps the test from being a green light that never ran.
    """
    unexplained: list[str] = []
    for pair in LIFECYCLE_PAIRS:
        if pair.inverse is not None or pair.forward is None:
            continue
        assert pair.reason
        if pair.reason.lower() not in _help_text(
            _commands(pair.app)[pair.forward]
        ).lower():
            unexplained.append(f"`fno {pair.app} {pair.forward}`: {pair.reason}")
    assert not unexplained, (
        "a transition with no inverse must say so where the caller reads:\n  "
        + "\n  ".join(unexplained)
    )


def test_the_no_inverse_check_can_fail():
    """Positive control: an unstated reason really is detected."""
    reason = "this reason appears in no help text anywhere"
    assert reason.lower() not in _help_text(_commands("backlog")["get"]).lower()


@pytest.mark.parametrize("app_name", sorted(KNOWN_COMMANDS))
def test_every_command_is_classified(app_name: str):
    """A new verb forces a one-line decision instead of a later discovery."""
    live = set(_commands(app_name))
    known = set(KNOWN_COMMANDS[app_name])
    added = sorted(live - known)
    dropped = sorted(known - live)
    assert not added and not dropped, (
        f"`fno {app_name}` surface changed.\n"
        f"  new: {added or 'none'}\n"
        f"  gone: {dropped or 'none'}\n"
        "Fix: update KNOWN_COMMANDS, and if the new verb changes a node's "
        "lifecycle state or existence, give it a LIFECYCLE_PAIRS row naming its "
        "correction (or an inverse=None row saying why it cannot have one)."
    )
