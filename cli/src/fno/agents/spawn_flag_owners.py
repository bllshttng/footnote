"""Every ``fno agents spawn`` flag, and who owns it.

``fno agents spawn`` carries 43 flags in one namespace, and nothing recorded
which of them fno actually owns. The ``--`` passthrough (which forwards
harness-native flags verbatim) cannot shrink the union until somebody can
say, per flag, whether fno branches on the value or merely translates its
spelling - and a fortieth flag could land with nobody classifying it. This
table is that classification, and ``fno doctor lint spawn-flag-owners``
introspects the live parser against it, so a flag added without a row fails
in the PR that adds it.

Three owners, and the distinction is the migration's own precondition: a
flag fno only forwards is safe to move across the separator, a flag fno
branches on is not.

- ``fno`` branches on the value to decide routing, placement, identity,
  claim, or gating. Cannot move.
- ``translated``: the harness owns the concept; fno owns only the
  per-harness spelling map, and that map is a maintenance cost. Movable
  once the passthrough covers every substrate (it is pane-only today).
- ``forwarded``: fno passes the value through unread. Movable today.
  Measured EMPTY: every harness flag fno accepts, it also translates. An
  empty class is a finding, not a gap, so the class is kept here by name
  and the lint can still name it.

``site`` is what makes a translated row checkable rather than an opinion:
the ``file:function`` holding the per-harness spelling map, which is the
code a migration deletes. A reviewer can open it. Two sites on one row
means two copies of one map, which is the ``--effort`` defect this module
records (and this PR's Rust-side fix closes; the two-site row stays until
one copy is actually deleted).

Provenance: ``sources`` names how a value can reach a spawn, in the same
four words the spawn receipt prints at runtime on ``account_source``. The
defect that forced the column: a spawn receipt named the account it launched
on but not WHO chose it, and a config injection read as a caller decision.
One vocabulary, defined here, used statically and at runtime:

- ``caller``: an explicit flag on this spawn's argv.
- ``config``: a config gate injected it (``accounts.quota.pick_on_launch``
  picking the launch account is the live case).
- ``env``: inherited from the spawning environment (the harness axis's
  invoking-harness inference is the live case).
- ``default``: nobody chose; the parser's or resolver's fallback.

The column records what the code verifiably does today; a row whose value
can only ever be caller-or-default says so, and a new injection path lands
together with its ``sources`` edit in the same diff.
"""

from __future__ import annotations

from dataclasses import dataclass

FNO = "fno"
TRANSLATED = "translated"
FORWARDED = "forwarded"

CALLER = "caller"
CONFIG = "config"
ENV = "env"
DEFAULT = "default"

#: Help-panel titles derived from the table, so the parser's ``--help``,
#: this table, and the lint cannot disagree about who owns a flag.
PANEL_FNO = "fno-owned: routing, placement, identity, gating"
PANEL_HARNESS = "harness-owned (translated; the harness's own spelling also works after `--`)"


@dataclass(frozen=True)
class FlagOwner:
    """One spawn flag: who owns it, where its translation lives, its provenance."""

    owner: str
    why: str
    #: ``file:function`` holding the per-harness spelling map. Non-empty on
    #: every translated row: it is the code a migration deletes.
    site: str = ""
    #: How the value can reach a spawn. Never empty.
    sources: tuple[str, ...] = (CALLER, DEFAULT)


FLAG_OWNERS: dict[str, FlagOwner] = {
    "--name": FlagOwner(
        FNO,
        "registry-row handle; an adjective-noun slug is minted when omitted",
    ),
    "--harness": FlagOwner(
        FNO,
        "names the binary; fno resolves it (explicit > invoking > default) "
        "and gates every downstream refusal on the result",
        sources=(CALLER, ENV, DEFAULT),
    ),
    "--provider": FlagOwner(
        FNO,
        "model-vendor axis; fno routes on it and refuses harness names in it",
    ),
    "--recorded-provider": FlagOwner(
        FNO,
        "machine-recorded vendor identity for a recovery spawn; never configures a route",
    ),
    "--once": FlagOwner(FNO, "selects the create+exchange+teardown one-shot lifecycle"),
    "--substrate": FlagOwner(FNO, "selects the argv builder itself"),
    "--headless": FlagOwner(FNO, "substrate shortcut; wins over --substrate"),
    "--sandbox-write-policy": FlagOwner(
        FNO,
        "composes the worker's one settings file; refused on the pane substrate",
    ),
    "--cwd": FlagOwner(
        FNO,
        "launch-dir policy; the canonical-root default pairs with --fresh/--here",
    ),
    "--timeout": FlagOwner(FNO, "per-spawn budget fno enforces"),
    "--from-name": FlagOwner(FNO, "mail-envelope identity"),
    "--yolo": FlagOwner(
        TRANSLATED,
        "dangerous-mode bypass; the emitted spelling differs per harness",
        site="cli/src/fno/agents/mux_spawn.py:build_pane_argv",
    ),
    "--fresh": FlagOwner(FNO, "workdir-policy alias fno reads (canonical-root default)"),
    "--here": FlagOwner(FNO, "workdir policy: stay in the caller's cwd"),
    "--role": FlagOwner(FNO, "routing role for per-spawn model selection"),
    "--route": FlagOwner(
        FNO,
        "explicit route; fno validates provider, protocol, and key, fail-closed",
    ),
    "--monitor": FlagOwner(FNO, "fno's monitor contract, resolved before spawn"),
    "--account": FlagOwner(
        FNO,
        "account pin; fno resolves the env overlay, and "
        "accounts.quota.pick_on_launch may inject one when it is omitted - "
        "the provenance specimen",
        sources=(CALLER, CONFIG, DEFAULT),
    ),
    "--dispatch-account": FlagOwner(
        FNO,
        "quota-cutover record; fno stages its env, fail-closed",
        sources=(CALLER,),
    ),
    "--model": FlagOwner(
        FNO,
        "fno routes on it, guards substitutions, and stamps model_basis",
    ),
    "--permission-mode": FlagOwner(
        TRANSLATED,
        "approval mode; the emitted spelling is per harness",
        site="cli/src/fno/agents/mux_spawn.py:permission_pane_tokens",
    ),
    "--effort": FlagOwner(
        TRANSLATED,
        "reasoning effort; TWO spelling maps (Python and Rust) that must agree",
        site="cli/src/fno/agents/mux_spawn.py:effort_tokens"
        " + crates/fno-agents/src/bin/client.rs:validate_effort_for_spawn",
    ),
    "--resume": FlagOwner(
        TRANSLATED,
        "claude-only today; the per-harness refusal disappears once resume "
        "is the harness's own flag across the separator",
        site="cli/src/fno/agents/cli.py:cmd_spawn",
    ),
    "--add-dir": FlagOwner(
        TRANSLATED,
        "workspace grant; claude/codex/agy/cursor-agent map it, the rest refuse it",
        site="cli/src/fno/agents/mux_spawn.py:tier3_pane_tokens",
    ),
    "--agent": FlagOwner(
        TRANSLATED,
        "sub-agent pin; claude/opencode map it, the rest refuse it",
        site="cli/src/fno/agents/mux_spawn.py:tier3_pane_tokens",
    ),
    "--tools": FlagOwner(
        TRANSLATED,
        "allowed-tools scope; claude maps it, the rest refuse it",
        site="cli/src/fno/agents/mux_spawn.py:tier3_pane_tokens",
    ),
    "--deny-tools": FlagOwner(
        TRANSLATED,
        "disallowed-tools scope; claude/pi map it, the rest refuse it",
        site="cli/src/fno/agents/mux_spawn.py:tier3_pane_tokens",
    ),
    "--output-format": FlagOwner(FNO, "headless JSON contract fno parses the reply out of"),
    "--workspace": FlagOwner(FNO, "mux pane placement; fno owns the mux"),
    "--squad": FlagOwner(FNO, "deprecated alias of --workspace"),
    "--split": FlagOwner(FNO, "mux pane placement"),
    "--at": FlagOwner(FNO, "mux exact-origin placement"),
    "--tab": FlagOwner(FNO, "mux tab placement"),
    "--portal": FlagOwner(FNO, "thread portal placement; fno owns portals"),
    "--bounded-placement": FlagOwner(
        FNO, "automated placement lane, serialized under the mux lease"
    ),
    "--crown": FlagOwner(FNO, "crown ladder; no harness has the concept"),
    "--succeed": FlagOwner(FNO, "crown succession; fno validates the transfer"),
    "--node": FlagOwner(FNO, "backlog identity; exports the node-provenance env"),
    "--slug": FlagOwner(FNO, "provenance override"),
    "--plan": FlagOwner(FNO, "provenance override"),
    "--session-phase": FlagOwner(FNO, "sessions-row lifecycle stamp"),
    "--force": FlagOwner(FNO, "spawn-gate bypass (cap and RAM floor)"),
    "--no-wait": FlagOwner(FNO, "spawn-gate queueing policy"),
}

#: Growth ratchet, measured at merge: 43 flags. This is a ratchet on growth,
#: not a claim that 43 is right. The number falls as flags move across the
#: ``--`` separator; it can only rise in a deliberate one-line diff a
#: reviewer sees.
SPAWN_FLAG_CAP = 43


def owner_panel(flag: str) -> str:
    """The ``--help`` panel for one flag, derived from the table.

    Raises KeyError on an unknown flag rather than falling back: a
    misspelled key would silently shelve a harness-owned option under the
    fno panel, which is exactly the mislabeling this module exists to
    prevent.
    """
    return PANEL_FNO if FLAG_OWNERS[flag].owner == FNO else PANEL_HARNESS
