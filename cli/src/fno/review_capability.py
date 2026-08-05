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
from pathlib import Path
from typing import Literal, Mapping, Optional

from fno.config import _coerce_affirmative, resolvable_reviewers, ReviewerDescriptor
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
        """One report line. A rung weaker than review-evidence always says so."""
        asserts = self.descriptor.asserts if self.descriptor is not None else None
        return f"{self.status}: {self.name} - {self.reason}{_RUNG_NOTES.get(asserts, '')}"


@dataclass(frozen=True)
class LocalPeerVerdict:
    """One identity-free peer candidate for the composite local gate."""

    name: str
    status: Status
    reason: str

    @property
    def eligible(self) -> bool:
        return self.status == "satisfiable"

    def line(self) -> str:
        return f"{self.status}: {self.name} - {self.reason}"


# What each rung is allowed to claim, printed wherever a reviewer is reported.
# `review-evidence` gets no note: it is the ordinary case. The two weaker rungs
# are annotated so an operator learns what is behind their green checkmark here,
# rather than at the stop gate.
_RUNG_NOTES: dict[Optional[str], str] = {
    "self-cert": "  [self-cert: satisfies the gate, asserts no review evidence]",
    "invocation": (
        "  [invocation: proves the reviewer ran at the reviewed commit, "
        "asserts nothing about its verdict]"
    ),
}


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
        #  is documentation-only in the shared helper; not passed, so a
        # future edit to it cannot read as controlling a fallback here.
        return _coerce_affirmative(value, False)
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


def _skill_roots(cwd: Optional[Path] = None) -> list[Path]:
    """Where Claude resolves a BARE skill name: the user root and the project root.

    Deliberately short. Plugin skills live under a versioned, marketplace-shaped
    cache whose layout this repo cannot cite as an authority, and they carry a
    `plugin:skill` qualified name - so a qualified name resolves `unverifiable`
    rather than being reported absent. Reporting absence from a root set we are
    not sure is complete would refuse a session over a reviewer that IS
    installed, which is strictly worse than proceeding with the stop gate as
    the backstop.
    """
    base = Path.cwd() if cwd is None else cwd
    return [Path.home() / ".claude" / "skills", base / ".claude" / "skills"]


def _skill_id(descriptor: ReviewerDescriptor, name: str) -> str:
    """The skill name to resolve, derived from the descriptor's INVOCATION.

    Not the registry key. `invocation` is documented as the exact command that
    satisfies the gate and nothing constrains the key to match it, so a project
    registering `security-review` with `invocation = "/my-security-skill"` would
    otherwise have init glob `.claude/skills/security-review/SKILL.md` and refuse
    a skill that IS installed under its real name. It also means a
    plugin-qualified invocation is seen as qualified even under a plain key.

    Falls back to the key for an invocation with no leading token, so a
    malformed descriptor degrades to the old behavior rather than probing "".
    """
    head = descriptor.invocation.strip().split(maxsplit=1)
    return head[0].lstrip("/") if head and head[0].strip("/") else name


def _resolve_skill(name: str, session: SessionCapability) -> tuple[Status, str]:
    """`requires: skill` - is the named skill resolvable on this harness?

    Three outcomes, all onto the existing Status enum: found -> satisfiable,
    absent with at least one root readable -> unavailable (init refuses, naming
    the roots searched), and anything we cannot answer -> unverifiable, which is
    already non-blocking precisely so a bad root guess degrades rather than
    bricking a run.
    """
    proceed = "Proceeding - if the gate does go unmet, run the skill by hand"
    if session.harness != "claude":
        return (
            "unverifiable",
            f"needs a resolvable skill; footnote only knows Claude's skill roots "
            f"and this is {session.describe()}. {proceed}",
        )
    if ":" in name:
        return (
            "unverifiable",
            f"{name!r} is a plugin-qualified skill; footnote does not read the "
            f"plugin cache layout, so availability cannot be verified. {proceed}",
        )
    searched: list[str] = []
    for root in _skill_roots():
        try:
            if not root.is_dir():
                continue
            searched.append(str(root))
            if (root / name / "SKILL.md").is_file():
                return ("satisfiable", f"skill resolves at {root / name}")
        except OSError as exc:
            return (
                "unverifiable",
                f"skill root {root} is unreadable ({exc}), so availability "
                f"cannot be verified. {proceed}",
            )
    if not searched:
        return (
            "unverifiable",
            f"no skill root exists on this host (looked for "
            f"{', '.join(str(r) for r in _skill_roots())}), so availability "
            f"cannot be verified. {proceed}",
        )
    return (
        "unavailable",
        f"skill {name!r} resolves in none of the roots searched "
        f"({', '.join(searched)}); install it there or change "
        f"config.review.reviewers",
    )


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

    if descriptor.requires == "skill":
        return verdict(*_resolve_skill(_skill_id(descriptor, name), session))

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
    reviewers: list[str],
    session: Optional[SessionCapability] = None,
    registry: Optional[Mapping[str, ReviewerDescriptor]] = None,
) -> list[ReviewerVerdict]:
    """Resolve every configured reviewer against this session. Read-only.

    `registry` is `config.review.reviewer_registry`; it is unioned with the
    built-ins by the same helper the config validator uses, so the two cannot
    disagree about which names resolve.

    Never caches across sessions: two sessions on one repo resolve their own
    harness and substrate.
    """
    sess = detect_session() if session is None else session
    known = resolvable_reviewers(registry)
    out: list[ReviewerVerdict] = []
    for entry in reviewers:
        name = entry.strip().lstrip("/")
        descriptor = known.get(name)
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


@dataclass(frozen=True)
class PreShipReviewPlan:
    """What the target spine does at its pre-ship review step.

    The single codified answer the skill prose and docs defer to, so the skip
    direction cannot drift on one surface while the others keep the old one.
    A prose edit that re-inverts the default flips this contract red.
    """

    kind: Literal["native", "skip"]
    reason: str


def preship_review_plan(reviewers: list[str]) -> PreShipReviewPlan:
    """Decide the target spine's pre-ship review step from `config.review.reviewers`.

    Sigma is opt-in. When it is a configured reviewer it runs exactly once,
    post-ship, against the final HEAD (the attestation gate in
    skills/target/references/ship-and-promise.md), so the pre-ship step is
    skipped to avoid a panel whose attestation any later fix would invalidate.
    The default - no sigma reviewer - runs the invoking harness's own native
    review of the diff and never dispatches the six-agent sigma panel.
    """
    names = {str(r).strip().lstrip("/") for r in reviewers}
    if "sigma" in names:
        return PreShipReviewPlan(
            "skip",
            "sigma is configured; it runs once post-ship on final HEAD, so the "
            "pre-ship native review is skipped",
        )
    return PreShipReviewPlan(
        "native",
        "no sigma reviewer; run the invoking harness's native review and do not "
        "dispatch the sigma panel",
    )


def _model_family(provider: str, model: object = None) -> str:
    """Resolve the model family a peer actually runs, independent of transport."""
    family = provider.strip().lower()
    route = model.strip() if isinstance(model, str) else ""
    # Only the Claude transport executes an explicit route. Codex and Gemini
    # ignore `model`, so honoring it here would disagree with loop-check and
    # could advertise a same-model peer as eligible.
    if family == "claude" and "," in route:
        routed_provider, _, _routed_model = route.partition(",")
        if routed_provider.strip():
            family = routed_provider.strip().lower()
    aliases = {
        "anthropic": "claude",
        "openai": "codex",
        "google": "gemini",
    }
    return aliases.get(family, family)


def resolve_local_peers(
    peers: list[object],
    peer_identity: Optional[str] = None,
    session: Optional[SessionCapability] = None,
) -> list[LocalPeerVerdict]:
    """Classify identity-free peers for one composite cross-model gate.

    Entries with a per-peer identity, or all entries when a shared identity is
    configured, retain the legacy GitHub-posting carrier and are intentionally
    absent here. The composite gate is satisfiable when *any* returned peer is
    cross-model; same-model alternatives remain visible diagnostics, not vetoes.
    """
    sess = detect_session() if session is None else session
    if peer_identity:
        return []

    verdicts: list[LocalPeerVerdict] = []
    for entry in peers:
        if isinstance(entry, str):
            provider, model, identity = entry.strip(), None, None
        elif isinstance(entry, dict):
            provider = str(entry.get("provider") or "").strip()
            model = entry.get("model")
            identity = entry.get("identity")
        else:
            continue
        if identity:
            continue

        route = model.strip() if isinstance(model, str) else ""
        name = f"{provider} via {route}" if route and provider.lower() == "claude" else provider
        peer_family = _model_family(provider, model)
        author_family = _model_family(sess.harness)
        if sess.harness == "unknown":
            verdicts.append(
                LocalPeerVerdict(
                    name,
                    "unverifiable",
                    "author model is unknown; cross-model eligibility cannot be verified",
                )
            )
        elif peer_family == author_family:
            verdicts.append(
                LocalPeerVerdict(
                    name,
                    "unavailable",
                    f"same model family as the author ({author_family})",
                )
            )
        else:
            verdicts.append(
                LocalPeerVerdict(
                    name,
                    "satisfiable",
                    f"cross-model peer for {author_family} author",
                )
            )
    return verdicts


def local_peers_refusal_message(
    verdicts: list[LocalPeerVerdict], session: SessionCapability
) -> Optional[str]:
    """Return an init refusal only when a configured local peer gate has no option."""
    if not verdicts or any(v.eligible for v in verdicts):
        return None
    # An unknown author is not proof of an impossible gate. Preserve the stop
    # gate as the backstop, matching reviewer capability's unverifiable policy.
    if any(v.status == "unverifiable" for v in verdicts):
        return None
    lines = [
        "fno target init: config.review.peers has no eligible cross-model "
        f"local peer on {session.describe()}.",
    ]
    lines += [f"  {v.line()}" for v in verdicts]
    lines += [
        "",
        "The gate is fail-closed: configure at least one peer from a different "
        "model family, or remove config.review.peers.",
    ]
    return "\n".join(lines)


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
    # Per-CAPABILITY, for the same reason the statuses are split: "run on a
    # harness that dispatches subagents" is a remedy that provably cannot
    # install a missing skill, and a remedy that cannot work is the failure
    # class this check exists to delete. The roots searched are already in the
    # verdict line printed above.
    unavailable = [v for v in blocked if v.status == "unavailable"]
    requires = {v.descriptor.requires for v in unavailable if v.descriptor is not None}
    if "skill" in requires:
        remedies.append("install the named skill in one of the roots listed above")
    if unavailable and requires != {"skill"}:
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


# ── github_apps axis (x-b167 section 6) ──────────────────────────────────────
#
# `config.review.reviewers` (above) refuses in two seconds when the LOCAL
# attestation gate cannot be satisfied here. The `github_apps` gate had no
# equivalent, so the same config block failed at two different times depending
# on which half you got wrong: local reviewers instantly, App bots only at the
# budget ceiling an hour later. This closes that asymmetry in the same
# vocabulary: a `github_apps` login that footnote does not recognize AND has
# never acted in this repository is a typo or an uninstalled App, and gets
# refused before any work begins.

# Mirror of the Rust `BOT_PROFILES` logins (crates/fno-agents/src/loopcheck.rs)
# and the `_OPTIONAL_BOTS` tuple (cli/src/fno/pr/_reviews.py): the review Apps
# footnote recognizes. A recognized login is `satisfiable` at init even before
# it has acted on a fresh repo, because footnote knows it is a real bot (and for
# the nudgeable ones, knows how to reach it). NOTE (x-b167): this is the third
# copy of the bot-login set across two languages; folding all three behind one
# parity-checked source is a follow-up the plan explicitly deferred.
_KNOWN_REVIEW_APP_LOGINS: frozenset[str] = frozenset(
    {"chatgpt-codex-connector", "gemini-code-assist"}
)


def _app_is_recognized(login: str) -> bool:
    """Whether a CONFIGURED `github_apps` login corresponds to a known review App.

    Directional on purpose: the login is recognized only when it is a substring
    of a known login (an exact login or an intentional short alias like "codex"),
    NOT when a known login is a substring of it. The gate is satisfied by
    `actual_author.contains(configured_login)` in Rust, so a suffixed typo like
    "chatgpt-codex-connector-typo" can never be satisfied by a real review and
    must NOT be waved through here - it should be probed and refused as a typo.
    """
    lo = login.lower()
    return any(lo in k for k in _KNOWN_REVIEW_APP_LOGINS)


def _app_ever_acted(login: str, cwd: Optional[str] = None) -> Optional[bool]:
    """True if `login` authored an issue/PR comment in this repo, False if a
    clean search returned none, None if the probe could not run (no repo remote,
    no token scope, rate limit, gh missing).

    Fail-open by contract: None on ANY doubt so init never refuses on a probe it
    could not run (section 6). A clean zero is the only False, and only a login
    footnote does not recognize ever reaches this probe.
    """
    import subprocess

    def _run(args: list[str]) -> Optional[str]:
        try:
            r = subprocess.run(
                args, cwd=cwd, capture_output=True, text=True, timeout=30
            )
        except Exception:  # noqa: BLE001 - a failed probe is unverifiable, not False
            return None
        return r.stdout if r.returncode == 0 else None

    slug = _run(["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
    if not slug or not slug.strip():
        return None
    total = _run(
        [
            "gh",
            "api",
            "-X",
            "GET",
            "search/issues",
            "-f",
            f"q=repo:{slug.strip()} commenter:{login}",
            "--jq",
            ".total_count",
        ]
    )
    if total is None:
        # KNOWN BLIND SPOT, deliberately left unverifiable. The search API
        # answers 422 for a `commenter:` login with no account, so the likeliest
        # typo shape reaches None and proceeds rather than refusing.
        #
        # Do NOT "fix" this by resolving the login against `gh api users/<login>`
        # (with or without the `[bot]` suffix). A configured entry is matched by
        # the completion gate with `author_login.contains(configured)`
        # (login_matches_bot in crates/fno-agents/src/loopcheck.rs), so a short
        # alias like "reviewer" for `acme-reviewer[bot]` is a SUPPORTED config
        # and is not an account of its own: both exact lookups 404 and the run
        # would be refused at init even though that bot reviews this repo
        # normally. A false refusal blocks every run, which is worse than the
        # late wedge this gate exists to shorten.
        #
        # Proving absence under substring semantics means enumerating the
        # repository's participants, which is unbounded; until something does
        # that, unverifiable is the honest answer (x-4a60).
        return None
    try:
        return int(total.strip()) > 0
    except ValueError:
        return None


@dataclass(frozen=True)
class AppVerdict:
    """One configured `github_apps` login, resolved against this repository."""

    login: str
    status: Status  # satisfiable | unavailable | unverifiable
    reason: str

    @property
    def blocks_autonomy(self) -> bool:
        # Same fail-closed default as ReviewerVerdict: unverifiable proceeds.
        return self.status not in _NON_BLOCKING_STATUSES

    def line(self) -> str:
        return f"{self.status}: {self.login} - {self.reason}"


def resolve_github_apps(
    apps: list[str],
    *,
    probe=None,
    cwd: Optional[str] = None,
    nudge_logins: "frozenset[str] | set[str] | tuple[str, ...] | None" = None,
) -> list[AppVerdict]:
    """Resolve every `github_apps` login against this repo. Read-only.

    A recognized login is `satisfiable` with no probe. An unrecognized login is
    probed: it is `satisfiable` if it has acted here, `unavailable` (a likely
    typo) if a clean search found nothing, and `unverifiable` if the probe could
    not run - which proceeds, never refuses.

    `nudge_logins` are the keys of `config.review.nudge`: an operator who wrote an
    explicit nudge profile for an App has named a real bot, not a typo, so it is
    `satisfiable` at init even before it has ever acted here (loop-check will post
    its trigger). This is the documented first-use path for a custom App.

    `probe` defaults to the module-level `_app_ever_acted` resolved at call time
    (so a test can monkeypatch it), or pass an explicit callable.
    """
    probe = probe or _app_ever_acted
    nudge = {n.strip().lower() for n in (nudge_logins or ()) if n.strip()}
    out: list[AppVerdict] = []
    for entry in apps:
        login = entry.strip()
        if not login:
            continue
        lo = login.lower()
        # Same directional rule as _app_is_recognized: the configured login must
        # be a substring of a nudge key (an alias/exact match), never the reverse,
        # so a suffixed typo is not waved through on an operator's nudge profile.
        if _app_is_recognized(login) or any(lo in nk for nk in nudge):
            out.append(
                AppVerdict(
                    login,
                    "satisfiable",
                    "recognized review App; footnote knows how to reach it",
                )
            )
            continue
        seen = probe(login, cwd)
        if seen is True:
            out.append(
                AppVerdict(login, "satisfiable", "has reviewed or commented in this repo")
            )
        elif seen is False:
            out.append(
                AppVerdict(
                    login,
                    "unavailable",
                    "never reviewed or commented in this repository - likely a typo, "
                    "or a GitHub App not installed here",
                )
            )
        else:
            out.append(
                AppVerdict(
                    login,
                    "unverifiable",
                    "could not check whether this App has acted here (no repo "
                    "remote, token scope, rate limit, or a login GitHub does "
                    "not resolve as an account); proceeding",
                )
            )
    return out


def github_apps_refusal_message(verdicts: list[AppVerdict]) -> Optional[str]:
    """The init refusal for an unreachable `github_apps` login, or None.

    Names the login, why it cannot review, and the two ways out: fix the login,
    or move it to `config.review.optional_apps` (honored-if-present, never waited
    on). Never widens or drops the gate on its own.
    """
    blocked = [v for v in verdicts if v.blocks_autonomy]
    if not blocked:
        return None
    lines = [
        "fno target init: config.review.github_apps names a bot that cannot review here.",
    ]
    lines += [f"  {v.line()}" for v in blocked]
    lines += [
        "",
        "The gate is fail-closed: a required App that never reviews wedges the "
        "session until the budget ceiling.",
        "To proceed: fix the login in config.review.github_apps; or move it to "
        "config.review.optional_apps (honored-if-present, never waited on).",
    ]
    return "\n".join(lines)
