"""Canonical harness-name list (L0 platform data).

The COMPLETE roster of harnesses footnote supports, held equal to the union of
three shipped evidence surfaces (setup docs, the Rust provider dispatch, the
Python adapter registry) by ``scripts/ci/check-harness-roster-parity.py``.
Pure data with no ``fno`` imports, so any layer may read it without a
cross-layer edge. The runtime capability table
(``fno.agents.harness_map._HARNESS_CAPS``) asserts its keys are a SUBSET of
this list at import time: a capability row naming a harness absent here fails
loudly, while a roster entry with no capability row (hermes, openclaw today)
is a supported identity without a native fno spawn - legal, and deliberately
not a capability.

This inverts the old derivation (names read FROM the capability table) so the
platform layer (``fno.harness_identity``) no longer reaches into the runtime
for the name set, which dragged ``fno.agents`` in at import time (x-cec8). The
name set is the source of truth; the capability table validates against it.
"""
from __future__ import annotations

# Every harness supported by shipped evidence, not every harness with a
# capability row. hermes and openclaw host real sessions per docs/SETUP-*.md
# but have no native fno dispatch, so they carry no row in
# fno.agents.harness_map._HARNESS_CAPS; the map asserts capability keys stay a
# subset of this tuple at import time. Order is capability-table order, then
# newly recognized hosts appended; readers that need sorted output call
# sorted() (as known_harnesses() does).
KNOWN_HARNESSES: tuple[str, ...] = (
    "claude",
    "codex",
    "gemini",
    "agy",
    "opencode",
    "pi",
    "hermes",
    "openclaw",
    "cursor-agent",
    "grok",
)

# Thread/headless accepts opencode through its launch seam, and the
# keeper-hosted lanes: cursor-agent through `create-chat`'s callee-minted
# chat id, pi through the caller-assigned id the restart journey proved.
# gemini is pane-only and stays out of this tuple: it is deprecated and has no
# maintained dispatch lane at all.
#
# pi joins on journey evidence, not roster growth: its keeper-hosted thread
# lane survived a double SIGKILL (journey wk-x61bc,
# cli/tests/agents/test_thread_keeper_journey.py, first green 2026-09-01 on pi
# 0.84.2), and the spawn arm shipped behind that evidence
# (`dispatch_spawn`'s pi branch drives `_lane_b_thread_spawn`). pi's HEADLESS
# lane is a different story: nothing has run it, its state_root_grant row
# reads `unmeasured`, and `_check_spawn_harness` refuses that lane by stance
# rather than by absence from this tuple. The membership answers "is there a
# seam arm"; the row answers "is the lane measured".
#
# cursor-agent joins the same way (x-61bc's generic thread lane): the
# dispatch_spawn arm mints the chat id through `create-chat`, hosts the TUI
# under `fno-agents-worker --keeper`, and the journey backs it. Its `thread`
# row reads true behind that same journey.
#
# grok joins the same keeper lane (x-fd31): the dispatch_spawn arm mints the
# caller-assigned `--session-id` uuid, hosts the TUI under
# `fno-agents-worker --keeper`, and the live measurement (create, SIGKILL,
# `--resume` recall) backs the row. kimi is deliberately ABSENT: its ACP
# mint lane is built and unit-tested, but the binary refuses every turn
# until its provider is configured (the operator's who-pays axis), so no
# row and no SPAWN_HARNESSES seat can stand behind an unmeasured lane.
#
# agy joins the same keeper lane. It takes no id on the command line, so the
# dispatch_spawn arm mints one from a print-mode turn whose JSON envelope
# carries `conversation_id`, and every later process rejoins with
# `--conversation <id>` - cursor-agent's callee-minted-read-back shape.
# Measured 2026-09-03 on agy 1.1.24: the mint returned in 1.5s, the id named a
# real db under ~/.gemini/antigravity-cli/conversations, and a fresh process
# resumed that conversation INTERACTIVELY with its transcript restored. agy's
# HEADLESS lane stays unmeasured and `_check_spawn_harness` refuses it by the
# row's stance, not by absence from this tuple.
SPAWN_HARNESSES: tuple[str, ...] = (
    "claude",
    "codex",
    "opencode",
    "cursor-agent",
    "pi",
    "grok",
    "agy",
)


def unknown_thread_harness_message(name: str, *, declared: bool) -> str:
    """The one refusal every thread-substrate seam raises.

    Both halves derive from ``SPAWN_HARNESSES``: the accept list, and whether
    a pane lane is offered instead. Two seams used to render the accept list
    from this tuple and then hardcode the same pane-only sentence beside it,
    so the prose could name a harness the tuple had since admitted - and did
    (agy, until x-d145). ``declared`` keeps the pane line honest: a harness
    with a capability row always has a pane lane, a typo has no lane at all.
    """
    accepted = ", ".join(SPAWN_HARNESSES)
    lines = [
        f"unknown harness {name!r} on the thread substrate (--harness names "
        f"the CLI BINARY); accepted here: {accepted}.",
    ]
    if declared:
        lines.append(f"{name} launches on --substrate pane only.")
    lines.append("If you meant a model VENDOR, that is -P/--provider.")
    return "\n".join(lines)
