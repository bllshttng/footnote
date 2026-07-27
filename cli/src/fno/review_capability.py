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
from dataclasses import dataclass
from typing import Literal, Mapping, Optional

from fno.config import _RESOLVABLE_REVIEWERS, ReviewerDescriptor
from fno.harness_identity import resolve_harness_identity

# Harnesses whose sessions can dispatch review subagents. The sigma panel runs
# its six reviewers through the Task/Agent tool (skills/review/SKILL.md: "it
# dispatches review subagents via the Task/Agent tool"), which is Claude Code's.
# A codex or gemini session has no path to a sigma attestation at all.
_SUBAGENT_DISPATCH_HARNESSES = frozenset({"claude"})

Status = Literal["satisfiable", "needs-operator", "unavailable"]


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
    """One configured reviewer, resolved against one session."""

    name: str
    descriptor: ReviewerDescriptor
    status: Status
    reason: str

    @property
    def blocks_autonomy(self) -> bool:
        return self.status != "satisfiable"

    def line(self) -> str:
        """One report line. A self-cert always says so (AC5)."""
        note = (
            "  [self-cert: satisfies the gate, asserts no review evidence]"
            if self.descriptor.asserts == "self-cert"
            else ""
        )
        return f"{self.status}: {self.name} - {self.reason}{note}"


def detect_session(env: Optional[Mapping[str, str]] = None) -> SessionCapability:
    """Read harness + substrate from the ambient environment.

    Substrate is derived from the dispatch env a spawner sets, because a session
    has no direct way to ask which substrate it was launched on. `attended`
    reuses the exact read `fno target init` already performs, so the capability
    check and the manifest can never disagree about it.
    """
    environ = os.environ if env is None else env
    harness = resolve_harness_identity(environ).harness or "unknown"

    bg = bool(environ.get("FNO_BG"))
    unattended_flag = environ.get("TARGET_UNATTENDED") == "1"
    spawned = bool(environ.get("FNO_AGENT_SELF"))
    attended = not (bg or unattended_flag or spawned)

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
        # Unknown harness lands here too, and refuses. Guessing "available"
        # reproduces exactly the wedge this check exists to kill, and the
        # refusal names both remedies, so a false refusal is loud and cheap.
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
                    _RESOLVABLE_REVIEWERS["declare"],
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
    lines += [
        "",
        "The gate is fail-closed: without a head-pinned review_attestation the "
        "session will block at the stop gate after the work is done.",
        "Change config.review.reviewers, or run attended.",
    ]
    return "\n".join(lines)
