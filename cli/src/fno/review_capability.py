"""Can this session's configured reviewers actually run, here (node x-cdc7)?

`config.review.reviewers` names a gate that only a head-pinned
`review_attestation` clears. Whether anything in THIS session can produce that
attestation depends on the harness and the substrate, not on the reviewer's
name. On PR #618 nothing checked: a `reviewers: [sigma]` gate was handed to a
session that would not dispatch subagents, so the gate was unsatisfiable from
the first turn and said so only at the stop gate, fifteen turns later.

This module answers the question once, read-only, at init time. It joins the
descriptor table (`fno.config._RESOLVABLE_REVIEWERS`) to the ambient session
identity (`fno.harness_identity`), and belongs to neither.

It never selects a reviewer for you. In particular it never substitutes
`declare` for an unavailable one: a self-cert that fires when the real reviewer
is missing is a green gate with no review behind it.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass
from typing import Literal, Mapping, Optional

from fno.config import _RESOLVABLE_REVIEWERS, _coerce_affirmative, ReviewerDescriptor
from fno.harness_identity import resolve_harness_identity

# Harnesses that can run the sigma panel to a verdict, and so can produce its
# attestation. Claude dispatches the six reviewers through the Task/Agent tool;
# Codex reaches the same panel through project custom agents / `spawn_agent`,
# and a Codex surface lacking that primitive reports the downgrade and runs the
# panel SEQUENTIALLY - slower, but it still reaches a verdict and still attests
# (docs/HARNESSES.md "Parallel subagent dispatch", docs/SKILL-COMPAT-MATRIX.md
# "CDX"). Refusing codex here would hard-exit `fno target init` on a
# configuration the project documents as supported, which is a worse failure
# than the one this check exists to prevent.
#
# Gemini stays out deliberately: its project-agent mode is experimental and
# opt-in (AGENTS.md), so it resolves `unavailable` rather than being assumed.
_SUBAGENT_DISPATCH_HARNESSES = frozenset({"claude", "codex"})

Status = Literal["satisfiable", "needs-operator", "unavailable", "unverifiable"]

# The allowlist half of the fail-closed default; see `blocks_autonomy`.
_NON_BLOCKING_STATUSES: frozenset[str] = frozenset({"satisfiable", "unverifiable"})


@dataclass(frozen=True)
class SessionCapability:
    """What this session can do, as far as the environment will admit."""

    harness: str  # claude | codex | gemini | unknown
    substrate: str  # bg | headless | pane | interactive
    attended: bool

    def describe(self) -> str:
        return f"harness={self.harness} substrate={self.substrate}"


@dataclass(frozen=True)
class ReviewerVerdict:
    """One configured reviewer, resolved against one session.

    `descriptor` is None for a name absent from the table: borrowing another
    reviewer's descriptor would render an unknown name with that reviewer's
    kind, invocation, and self-cert note.
    """

    name: str
    descriptor: Optional[ReviewerDescriptor]
    status: Status
    reason: str

    @property
    def blocks_autonomy(self) -> bool:
        """Whether this verdict should stop the run before it starts.

        `unverifiable` does NOT block. A shell with no harness marker is not a
        session that cannot dispatch subagents, it is one we cannot classify -
        and refusing it would break a plain-terminal `fno target init` in every
        repo that configures a reviewer. The cost of guessing wrong is now
        bounded: if the gate does turn out to be unsatisfiable, the stop-gate
        message names the reviewer and its invocation instead of blaming a bot.

        Written as "not in the non-blocking set" rather than "in the blocking
        set" so a Status added later blocks until someone deliberately clears
        it. The inverse would let an unclassified status run autonomously,
        which is the wrong default for a gate whose whole point is fail-closed.
        """
        return self.status not in _NON_BLOCKING_STATUSES

    def line(self) -> str:
        """One report line. A self-cert always says so (AC5)."""
        note = (
            "  [self-cert: satisfies the gate, asserts no review evidence]"
            if self.descriptor is not None and self.descriptor.asserts == "self-cert"
            else ""
        )
        return f"{self.status}: {self.name} - {self.reason}{note}"


def _unattended_in_config() -> bool:
    """`config.unattended.enabled` - the manifest's second `attended` input.

    The settings model is `extra: ignore`, so this key never survives
    `load_settings()`; reading the file directly is the only way Python can see
    what `hooks/helpers/init-target-state.sh` sees.
    """
    from fno.config import (
        _settings_yaml_locations,
        config_read_candidates,
        read_config_flat,
    )

    try:
        candidates = config_read_candidates(_settings_yaml_locations())
    except Exception as exc:  # noqa: BLE001 - a broken probe must not block init
        # False (attended) is the safe DIRECTION: guessing "unattended" would
        # refuse a plain terminal run whenever the probe hiccups. But guessing
        # silently is how an operator reviewer looks satisfiable and then wedges
        # at the stop gate, so the guess is always announced.
        print(
            f"WARN review capability: attendedness probe degraded "
            f"(settings unreadable: {exc}); assuming attended",
            file=sys.stderr,
        )
        return False
    # Per USABLE VALUE, not per block or per key: `load_settings` deep-merges
    # layers and the shell's get_config skips a candidate whose value is empty
    # or null, so stopping at the first file that merely MENTIONS the key would
    # answer differently from both authorities this claims parity with.
    #
    # `_coerce_affirmative` is the project's one bash-get_config truth table,
    # imported rather than copied: a second copy of a cross-language truth table
    # is the exact drift this node's own parity script exists to prevent.
    for path in candidates:
        block = read_config_flat(path).get("unattended")
        if not isinstance(block, dict) or "enabled" not in block:
            continue
        value = block["enabled"]
        if value is None or value == "":
            continue
        return _coerce_affirmative(value, default=False)
    return False


def detect_session(
    env: Optional[Mapping[str, str]] = None,
    unattended_configured: Optional[bool] = None,
) -> SessionCapability:
    """Read harness + substrate from the ambient environment.

    Substrate is derived from the dispatch env a spawner sets, because a session
    has no direct way to ask which substrate it was launched on.

    `attended` must match the manifest's own derivation
    (`hooks/helpers/init-target-state.sh`: `TARGET_UNATTENDED=1` OR
    `config.unattended.enabled`) or the check clears a gate the run cannot
    satisfy - an `operator` reviewer looks fine here and then wedges at the stop
    gate. The env markers below are additive: a bg or spawned session is also
    unattended, and neither the shell nor this function may be the only one to
    know it.

    Two residual divergences are known and deliberately tolerated, and BOTH push
    only toward UNATTENDED here, i.e. toward refusing - never toward clearing a
    gate. First, the shell needs `yq` to read a dotted key, so on a host without
    it the shell falls back to its "false" default while this reads the file
    directly. Second, the shell tests the rendered value against the literal
    string "true", so it reads `1` / `yes` / `on` as attended while
    `_coerce_affirmative` accepts them. Closing either properly means one reader
    for the key, which is a larger change than this node.
    """
    environ = os.environ if env is None else env
    harness = resolve_harness_identity(environ).harness or "unknown"

    bg = bool(environ.get("FNO_BG"))
    unattended_flag = environ.get("TARGET_UNATTENDED") == "1"
    spawned = bool(environ.get("FNO_AGENT_SELF"))
    if unattended_configured is None:
        unattended_configured = _unattended_in_config()
    attended = not (bg or unattended_flag or spawned or unattended_configured)

    if bg:
        substrate = "bg"
    elif unattended_flag:
        substrate = "headless"
    elif spawned:
        substrate = "pane"
    else:
        substrate = "interactive"

    return SessionCapability(harness=harness, substrate=substrate, attended=attended)


def _resolve_one(
    name: str, descriptor: ReviewerDescriptor, session: SessionCapability
) -> ReviewerVerdict:
    def verdict(status: Status, reason: str) -> ReviewerVerdict:
        return ReviewerVerdict(name, descriptor, status, reason)

    if descriptor.requires == "none":
        return verdict("satisfiable", f"run `{descriptor.invocation}`")

    if descriptor.requires == "operator":
        if session.attended:
            return verdict("satisfiable", f"ask the operator to run `{descriptor.invocation}`")
        return verdict(
            "needs-operator",
            f"kind={descriptor.kind}, never autonomously satisfiable; this run is "
            f"unattended ({session.describe()}). Not a misconfiguration - run "
            f"attended, or configure a reviewer this session can drive",
        )

    if descriptor.requires == "subagent-dispatch":
        if session.harness in _SUBAGENT_DISPATCH_HARNESSES:
            return verdict("satisfiable", f"run `{descriptor.invocation}`")
        if session.harness == "unknown":
            return verdict(
                "unverifiable",
                f"needs subagent-dispatch; no harness marker in this environment, "
                f"so capability cannot be verified. Proceeding - if the gate does "
                f"go unmet, run `{descriptor.invocation}`",
            )
        return verdict(
            "unavailable",
            f"needs subagent-dispatch, unavailable on {session.describe()}",
        )

    return verdict(
        "unavailable",
        f"declares an unknown capability {descriptor.requires!r}",
    )


def resolve_reviewers(
    reviewers: list[str], session: Optional[SessionCapability] = None
) -> list[ReviewerVerdict]:
    """Resolve every configured reviewer against this session. Read-only.

    Never caches across sessions: two sessions on one repo resolve their own
    harness and substrate.
    """
    sess = detect_session() if session is None else session
    out: list[ReviewerVerdict] = []
    for entry in reviewers:
        name = entry.strip().lstrip("/")
        descriptor = _RESOLVABLE_REVIEWERS.get(name)
        if descriptor is None:
            # The config validator already rejects these, so this is only
            # reachable when a caller hands in an unvalidated list.
            out.append(
                ReviewerVerdict(
                    name,
                    None,
                    "unavailable",
                    "unknown reviewer; not in the descriptor table",
                )
            )
            continue
        out.append(_resolve_one(name, descriptor, sess))
    return out


def refusal_message(
    verdicts: list[ReviewerVerdict], session: SessionCapability
) -> Optional[str]:
    """The init refusal, or None when every reviewer can run here.

    Names the reviewer, the missing capability, the harness and substrate, and
    both ways out. Never proposes `declare` as the fix.
    """
    blocked = [v for v in verdicts if v.blocks_autonomy]
    if not blocked:
        return None
    lines = [
        f"fno target init: config.review.reviewers cannot be satisfied on "
        f"{session.describe()}.",
    ]
    lines += [f"  {v.line()}" for v in blocked]
    # Per-status, because attendedness is irrelevant to `unavailable`: the
    # subagent-dispatch branch never reads session.attended, so "run attended"
    # there is a remedy that provably cannot work - the failure class this
    # whole check exists to delete.
    remedies = ["change config.review.reviewers"]
    if any(v.status == "needs-operator" for v in blocked):
        remedies.append("run attended so an operator can drive it")
    if any(v.status == "unavailable" for v in blocked):
        remedies.append(
            "run on a harness that dispatches subagents, or run the review by "
            "hand and attest with `bash skills/review/scripts/emit-attestation.sh "
            "<reviewer>`"
        )
    lines += [
        "",
        "The gate is fail-closed: without a head-pinned review_attestation the "
        "session will block at the stop gate after the work is done.",
        f"To proceed: {'; or '.join(remedies)}.",
    ]
    return "\n".join(lines)
