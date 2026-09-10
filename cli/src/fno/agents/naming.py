"""The single owner of agent-name generation.

Every dispatcher that assembles a provenance-carrying worker name
(``<verb>-<node-id>-<slug>``) routes through :func:`agent_name`. Before x-3218
four call sites carried their own copy of the policy and only one of them
capped the ASSEMBLED name at the daemon's 64-character limit, so a long
configured node id produced a name ``fno agents spawn`` rejected - a silent
dispatch loss with no session and no event.

The daemon (``crates/fno-agents/src/daemon.rs``) stays the validator at the
protected spawn boundary; it must never become the generator, because
truncating there would make the name a caller reasons about differ from the
name the runtime registers.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Optional

#: The daemon's public agent-name contract: 1-64 chars of ``[A-Za-z0-9_-]``.
MAX_LEN = 64
_VALID_NAME = re.compile(r"[A-Za-z0-9_-]{1,%d}\Z" % MAX_LEN)

#: Per-component cap for human-readable text, matching the shell dispatchers'
#: ``cut -c1-30``. The assembled budget below is what actually protects the
#: daemon contract; this only keeps one runaway title from eating it all.
SLUG_CAP = 30

#: Dispatch source codes (x-84b2). ``ab`` covers both active-backlog rows
#: (parallel fill and mission converge): same owner, same gate, and the name
#: describes the source that launched the worker, not the internal branch.
#: Attended operator/king launches carry no source at all.
DISPATCH_SOURCES = frozenset(
    {
        "ab", "ac", "sob", "rd", "th", "pm", "pw", "rec", "kg",
        "gr", "ro", "ev", "kl", "oh", "sh", "ex", "jn",
    }
)

#: Dispatch verb codes, following the kings' prefixes.
DISPATCH_VERBS = frozenset({"t", "bp", "r", "th", "f"})

#: First tokens that open a typed non-node identity rather than a graph node
#: prefix. ``session-`` wins over the node-shape regex because a session
#: handle can itself be node-shaped.
_TYPED_IDENTITY_TOKENS = frozenset({"backlog", "evals", "session"})

_NODE_SHAPE_RE = re.compile(r"([a-z][a-z0-9]*-[0-9a-f]+)(?:-(.*))?\Z")


class AgentNameError(ValueError):
    """The required identity cannot be represented under the daemon contract."""


def slug_component(raw: Optional[str], cap: int = SLUG_CAP) -> str:
    """Normalize free text to a name-safe tail, byte-for-byte with the shell.

    Mirrors ``sanitize_name`` in skills/agent/scripts/normalize.sh: lowercase,
    any non-``[a-z0-9-]`` run becomes a hyphen, repeats collapse, ends trim,
    then cut and re-trim a hyphen the cut exposed.
    """
    if not raw:
        return ""
    s = re.sub(r"-+", "-", re.sub(r"[^a-z0-9-]", "-", raw.lower())).strip("-")
    return s[:cap].rstrip("-")


def agent_name(
    prefix: str,
    node_id: str,
    *,
    slug: Optional[str] = None,
    qualifier: Optional[str] = None,
    discriminator: Optional[str] = None,
) -> str:
    """Build the canonical worker name ``<prefix>-<node_id>[-<qualifier>][-<slug>][-<discriminator>]``.

    Budget precedence, highest first: the operation ``prefix``, the full
    ``node_id``, any ``qualifier`` (a lifecycle reason) and ``discriminator``
    (a per-invocation uniqueness token), and last the expendable human-readable
    ``slug``, which absorbs whatever budget is left over.

    The slug is the only component that gives way. Shaving a discriminator
    instead would silently collapse two distinct dispatches onto one name, and
    the name IS the deduplication token for `fno agents spawn`. When the
    required components alone overflow, this raises rather than inventing an
    altered identity.

    :raises AgentNameError: on an empty or over-budget required identity, or a
        required component carrying characters outside the daemon contract.
    """
    prefix = (prefix or "").strip()
    node_id = (node_id or "").strip()
    qualifier = (qualifier or "").strip()
    disc = slug_component(discriminator)

    required_parts = [p for p in (prefix, node_id, qualifier, disc) if p]
    if not required_parts:
        raise AgentNameError("agent name needs at least a prefix or a node id")
    required = "-".join(required_parts)
    if len(required) > MAX_LEN:
        raise AgentNameError(
            f"required agent-name identity is {len(required)} chars, over the "
            f"{MAX_LEN}-char runtime limit: prefix={prefix!r} node={node_id!r}"
            + (f" qualifier={qualifier!r}" if qualifier else "")
            + (f" discriminator={disc!r}" if disc else "")
        )

    human = slug_component(slug)
    if human:
        avail = MAX_LEN - len(required) - 1  # -1 for the joining hyphen
        human = human[:avail].rstrip("-") if avail > 0 else ""

    name = "-".join(p for p in (prefix, node_id, qualifier, human, disc) if p)
    if not _VALID_NAME.fullmatch(name):
        raise AgentNameError(
            f"generated agent name {name!r} violates the runtime contract "
            f"[A-Za-z0-9_-]{{1,{MAX_LEN}}} (node={node_id!r})"
        )
    return name


def dispatch_agent_name(
    source: Optional[str],
    verb: str,
    identity: str,
    *,
    slug: Optional[str] = None,
    qualifier: Optional[str] = None,
    discriminator: Optional[str] = None,
) -> str:
    """Build the canonical dispatch name ``[<source>-]<verb>-<identity>[-...]``.

    The x-84b2 vocabulary owner: ``source`` is one of :data:`DISPATCH_SOURCES`
    (or ``None`` for an attended operator/king launch, which mints the
    source-less manual form) and ``verb`` is one of :data:`DISPATCH_VERBS`.
    ``identity`` is the full configured node id, or a typed non-node identity
    such as ``backlog``, ``evals``, or ``session-<handle>``.

    Unknown sources and verbs raise; a dispatcher never falls back to ``t``:
    fabricating provenance in the name is exactly what the vocabulary exists
    to stop. Budget precedence is :func:`agent_name`'s - source, verb, and
    identity are required; only the slug gives way.
    """
    v = (verb or "").strip()
    if v not in DISPATCH_VERBS:
        raise AgentNameError(
            f"unknown dispatch verb {verb!r}; known: {', '.join(sorted(DISPATCH_VERBS))}"
        )
    if source is None:
        prefix = v
    else:
        s = source.strip()
        if s not in DISPATCH_SOURCES:
            raise AgentNameError(
                f"unknown dispatch source {source!r}; known: "
                f"{', '.join(sorted(DISPATCH_SOURCES))}"
            )
        prefix = f"{s}-{v}"
    return agent_name(
        prefix, identity, slug=slug, qualifier=qualifier, discriminator=discriminator
    )


@dataclass(frozen=True)
class DispatchName:
    """A parsed canonical dispatch name.

    ``source`` is ``None`` for the manual (attended) form. ``node`` is the
    extracted graph node id when the identity is node-shaped, else ``None``
    (typed identities stay opaque: ``tail`` carries the remainder verbatim).
    """

    name: str
    source: Optional[str]
    verb: str
    node: Optional[str]
    tail: str


def parse_dispatch_agent_name(name: Optional[str]) -> Optional[DispatchName]:
    """Parse a canonical ``[<source>-]<verb>-<identity>`` name, else ``None``.

    Positional grammar: the first token is a source only when the second
    token is a verb, so a node id whose configured prefix collides with a
    code (``t-ab-4040eee8``) cannot misread. Pre-cutover names (``target-*``,
    ``think-*``, ``reconcile-*``, ``j-*``) are NOT canonical and return
    ``None`` - rollout readers keep their legacy fallbacks (AC3-EDGE), the
    legacy-read window is documented in
    ``docs/architecture/fno-agents-registry-and-dispatch.md``.
    """
    if not name:
        return None
    tokens = name.split("-")
    source: Optional[str] = None
    if len(tokens) >= 2 and tokens[0] in DISPATCH_SOURCES and tokens[1] in DISPATCH_VERBS:
        source, rest = tokens[0], tokens[2:]
    elif tokens[0] in DISPATCH_VERBS:
        rest = tokens[1:]
    else:
        return None
    verb = tokens[0] if source is None else tokens[1]
    if not rest:
        return None
    if rest[0] in _TYPED_IDENTITY_TOKENS:
        return DispatchName(name, source, verb, None, "-".join(rest))
    m = _NODE_SHAPE_RE.match("-".join(rest))
    if m:
        return DispatchName(name, source, verb, m.group(1), m.group(2) or "")
    return DispatchName(name, source, verb, None, "-".join(rest))
