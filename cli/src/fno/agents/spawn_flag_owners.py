"""Every ``fno agents spawn`` flag, who owns it, and how its value arrives.

``fno agents spawn`` carries 43 flags in one namespace, and nothing recorded
which of them fno actually branches on. This table is that classification;
``fno doctor lint spawn-flag-owners`` introspects the live parser against it,
so a flag added without a row fails in the PR that adds it.

Three owners. The distinction is the ``--`` passthrough migration's own
precondition: a flag fno only forwards is safe to move across the separator,
a flag fno branches on is not.

- ``fno`` branches on the value (routing, placement, identity, claim, gating).
  Cannot move.
- ``translated``: the harness owns the concept; fno owns only the per-harness
  spelling map named by ``site``. Movable once the passthrough covers every
  substrate (it is pane-only today). Two sites on one row means two copies of
  one map, the ``--effort`` defect this module records.
- ``forwarded``: fno passes the value through unread. Movable today. Measured
  EMPTY: every harness flag fno accepts, it also translates. An empty class is
  a finding, not a gap, so the lint can still name it.

Provenance: ``sources`` names how a value can reach a spawn, in the same four
words the spawn receipt prints at runtime on ``account_source``. The defect
that forced the column: a receipt named the account it launched on but not WHO
chose it, and a config injection read as a caller decision.

- ``caller``: an explicit flag on this spawn's argv.
- ``config``: a config gate injected it (``accounts.quota.pick_on_launch``).
- ``env``: inherited from the spawning environment (the harness axis's
  invoking-harness inference).
- ``default``: nobody chose; the resolver's fallback.

A row whose value can only ever be caller-or-default says so; a new injection
path lands together with its ``sources`` edit in the same diff. The harness
axis predates this vocabulary and keeps its own receipt wire values
(``harness_source:`` explicit / harness-inferred / builtin-default); they map
onto it as caller / env / default.
"""

from __future__ import annotations

from typing import NamedTuple

FNO = "fno"
TRANSLATED = "translated"
FORWARDED = "forwarded"

CALLER = "caller"
CONFIG = "config"
ENV = "env"
DEFAULT = "default"


class FlagOwner(NamedTuple):
    """One spawn flag: owner, why it cannot move, where its map lives, provenance.

    ``site`` is the ``file:function`` holding a translated row's spelling map -
    the code a migration deletes; a reviewer can open it. ``sources`` records
    what the code verifiably does today and never stays empty.
    """

    owner: str
    why: str
    site: str = ""
    sources: tuple[str, ...] = (CALLER, DEFAULT)


FLAG_OWNERS: dict[str, FlagOwner] = {
    "--name": FlagOwner(FNO, "registry-row handle; slug minted when omitted"),
    "--harness": FlagOwner(
        FNO, "names the binary; fno resolves and gates on it",
        sources=(CALLER, ENV, DEFAULT),
    ),
    "--provider": FlagOwner(FNO, "vendor axis; fno routes on it, refuses harness names"),
    "--recorded-provider": FlagOwner(FNO, "machine-recorded vendor for recovery; never routes"),
    "--once": FlagOwner(FNO, "selects the create+exchange+teardown lifecycle"),
    "--substrate": FlagOwner(FNO, "selects the argv builder itself"),
    "--headless": FlagOwner(FNO, "substrate shortcut; wins over --substrate"),
    "--sandbox-write-policy": FlagOwner(FNO, "composes the one settings file; refused on panes"),
    "--cwd": FlagOwner(FNO, "launch-dir policy; pairs with --fresh/--here"),
    "--timeout": FlagOwner(FNO, "per-spawn budget fno enforces"),
    "--from-name": FlagOwner(FNO, "mail-envelope identity"),
    "--yolo": FlagOwner(
        TRANSLATED, "dangerous-mode bypass; spelling differs per harness",
        site="mux_spawn.py:build_pane_argv + dispatch.py:_claude_create_path"
        " + dispatch.py:dispatch_spawn",
    ),
    "--fresh": FlagOwner(FNO, "workdir-policy alias fno reads"),
    "--here": FlagOwner(FNO, "workdir policy: stay in the caller's cwd"),
    "--role": FlagOwner(FNO, "routing role for per-spawn model selection"),
    "--route": FlagOwner(FNO, "explicit route; validated fail-closed"),
    "--monitor": FlagOwner(FNO, "fno's monitor contract, resolved before spawn"),
    "--account": FlagOwner(
        FNO, "account pin; pick_on_launch may inject one when omitted",
        sources=(CALLER, CONFIG, DEFAULT),
    ),
    "--dispatch-account": FlagOwner(
        FNO, "quota-cutover record; fno stages its env, fail-closed",
        sources=(CALLER,),
    ),
    "--model": FlagOwner(FNO, "fno routes on it and stamps model_basis"),
    "--permission-mode": FlagOwner(
        TRANSLATED, "approval mode; spelling is per harness",
        site="mux_spawn.py:permission_pane_tokens",
    ),
    "--effort": FlagOwner(
        TRANSLATED, "reasoning effort; TWO maps (Python and Rust) that must agree",
        site="mux_spawn.py:effort_tokens + crates/fno-agents/src/bin/client.rs:validate_effort_for_spawn",
    ),
    "--resume": FlagOwner(
        TRANSLATED, "claude-only; refusal dies when resume crosses the separator",
        site="cli.py:cmd_spawn",
    ),
    "--add-dir": FlagOwner(
        TRANSLATED, "workspace grant; four harnesses map it, the rest refuse",
        site="mux_spawn.py:tier3_pane_tokens",
    ),
    "--agent": FlagOwner(
        TRANSLATED, "sub-agent pin; two harnesses map it",
        site="mux_spawn.py:tier3_pane_tokens",
    ),
    "--tools": FlagOwner(
        TRANSLATED, "allowed-tools scope; claude maps it",
        site="mux_spawn.py:tier3_pane_tokens",
    ),
    "--deny-tools": FlagOwner(
        TRANSLATED, "disallowed-tools scope; claude and pi map it",
        site="mux_spawn.py:tier3_pane_tokens",
    ),
    "--output-format": FlagOwner(FNO, "headless JSON contract fno parses the reply out of"),
    "--workspace": FlagOwner(FNO, "mux pane placement; fno owns the mux"),
    "--squad": FlagOwner(FNO, "deprecated alias of --workspace"),
    "--split": FlagOwner(FNO, "mux pane placement"),
    "--at": FlagOwner(FNO, "mux exact-origin placement"),
    "--tab": FlagOwner(FNO, "mux tab placement"),
    "--portal": FlagOwner(FNO, "thread portal placement; fno owns portals"),
    "--bounded-placement": FlagOwner(FNO, "serialized placement lane under the mux lease"),
    "--crown": FlagOwner(FNO, "crown ladder; no harness has the concept"),
    "--succeed": FlagOwner(FNO, "crown succession; fno validates the transfer"),
    "--node": FlagOwner(FNO, "backlog identity; exports the node-provenance env"),
    "--slug": FlagOwner(FNO, "provenance override"),
    "--plan": FlagOwner(FNO, "provenance override"),
    "--session-phase": FlagOwner(FNO, "sessions-row lifecycle stamp"),
    "--force": FlagOwner(FNO, "spawn-gate bypass (cap and RAM floor)"),
    "--no-wait": FlagOwner(FNO, "spawn-gate queueing policy"),
}

#: Growth ratchet, measured at merge: 43 flags. Falls as flags move across the
#: ``--`` separator; rises only in a deliberate one-line diff a reviewer sees.
SPAWN_FLAG_CAP = 43
