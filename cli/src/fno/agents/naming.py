"""The single owner of agent-name generation: :func:`agent_name` budgets the
64-char daemon contract; :func:`dispatch_agent_name` owns the x-84b2
source/verb vocabulary (data in ``naming-codes.yaml``). The daemon stays the
validator at the spawn boundary and must never become the generator."""

from __future__ import annotations

import re
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from typing import Optional

#: The daemon's public agent-name contract: 1-64 chars of ``[A-Za-z0-9_-]``.
MAX_LEN = 64
_VALID_NAME = re.compile(r"[A-Za-z0-9_-]{1,%d}\Z" % MAX_LEN)

#: Per-component cap for human-readable text, matching the shell dispatchers'
#: ``cut -c1-30``.
SLUG_CAP = 30

_NODE_SHAPE_RE = re.compile(r"([a-z][a-z0-9]*-[0-9a-f]+)(?:-(.*))?\Z")

#: First tokens that open a typed non-node identity rather than a graph node
#: prefix. ``session-`` wins over the node-shape regex because a session
#: handle can itself be node-shaped.
_TYPED_IDENTITY_TOKENS = frozenset({"backlog", "evals", "session"})


class AgentNameError(ValueError):
    """The required identity cannot be represented under the daemon contract."""


class BridgeUsageError(ValueError):
    """`fno agents name` invoked with no usable form (a usage error, exit 2 -
    never conflated with the exit-3 naming refusal a stale install cannot
    distinguish from a usage error otherwise)."""


def bridge_name(
    prefix: str,
    node_id: str,
    *,
    slug: Optional[str] = None,
    qualifier: Optional[str] = None,
    discriminator: Optional[str] = None,
    source: Optional[str] = None,
    verb: Optional[str] = None,
) -> str:
    """The `fno agents name` assembly: ``--verb``/``--source`` select the
    x-84b2 dispatch form; a positional prefix alone is the legacy form."""
    if verb or source:
        if prefix:
            raise BridgeUsageError(
                "pass the legacy prefix form or --source/--verb, not both"
            )
        if not verb:
            raise BridgeUsageError("--source requires --verb")
        code = verb if verb in dispatch_verbs() else verb_code_for(verb)
        return dispatch_agent_name(
            source or None, code, node_id,
            slug=slug, qualifier=qualifier, discriminator=discriminator,
        )
    if not prefix:
        raise BridgeUsageError("a prefix or --verb is required")
    return agent_name(
        prefix, node_id, slug=slug, qualifier=qualifier, discriminator=discriminator
    )


@lru_cache(maxsize=1)
def _codes() -> dict:
    """The vocabulary tables from ``naming-codes.yaml`` (see that file)."""
    import yaml

    raw = yaml.safe_load((Path(__file__).parent / "naming-codes.yaml").read_text())
    return {
        "sources": frozenset(raw["sources"]),
        "verbs": frozenset(raw["verbs"]),
        "word_codes": dict(raw["word_codes"]),
        "provenance": tuple(
            (row["site"], row["source"], row["verb"]) for row in raw["provenance"]
        ),
    }


def dispatch_sources() -> frozenset:
    return _codes()["sources"]


def dispatch_verbs() -> frozenset:
    return _codes()["verbs"]


def provenance_rows() -> tuple[tuple[str, str, str], ...]:
    """``(site, source, verb)`` per registered dispatch path."""
    return _codes()["provenance"]


def slug_component(raw: Optional[str], cap: int = SLUG_CAP) -> str:
    """Normalize free text to a name-safe tail, byte-for-byte with the shell."""
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
    """Build ``<prefix>-<node_id>[-<qualifier>][-<slug>][-<discriminator>]``.

    The name is the dedup token for ``fno agents spawn``: source, verb,
    identity, qualifier, and discriminator are required (never shaved); only
    the human slug gives way. :raises AgentNameError: over-budget required
    identity or a component outside the daemon contract.
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


def verb_code_for(word: Optional[str]) -> str:
    """The verb code for a work-verb word (``/target``, ``/fno:blueprint``,
    ``builtin``, ...). Unknown words raise: nothing defaults to ``t``."""
    v = (word or "").strip()
    if v.startswith("/fno:"):
        v = v[len("/fno:"):]
    elif v.startswith("$fno:"):
        v = v[len("$fno:"):]
    v = v.lstrip("/") or "target"
    code = _codes()["word_codes"].get(v)
    if code is None:
        raise AgentNameError(f"unknown dispatch verb {word!r}")
    return code


def dispatch_agent_name(
    source: Optional[str],
    verb: str,
    identity: str,
    *,
    slug: Optional[str] = None,
    qualifier: Optional[str] = None,
    discriminator: Optional[str] = None,
) -> str:
    """Build ``[<source>-]<verb>-<identity>[-...]`` (x-84b2). ``source``
    None is the attended manual form; unknown codes raise rather than
    fabricating provenance."""
    v = (verb or "").strip()
    if v not in dispatch_verbs():
        raise AgentNameError(f"unknown dispatch verb {verb!r}")
    if source is None:
        prefix = v
    else:
        s = source.strip()
        if s not in dispatch_sources():
            raise AgentNameError(f"unknown dispatch source {source!r}")
        prefix = f"{s}-{v}"
    return agent_name(
        prefix, identity, slug=slug, qualifier=qualifier, discriminator=discriminator
    )


@dataclass(frozen=True)
class DispatchName:
    """A parsed canonical name. ``source`` is None for the manual form;
    ``node`` is the graph node id when the identity is node-shaped, else None
    (typed identities stay opaque in ``tail``)."""

    name: str
    source: Optional[str]
    verb: str
    node: Optional[str]
    tail: str


def parse_dispatch_agent_name(name: Optional[str]) -> Optional[DispatchName]:
    """Parse ``[<source>-]<verb>-<identity>``, else None. Positional
    grammar: the first token is a source only when the second is a verb, so a
    node prefix colliding with a code cannot misread. Pre-cutover names are
    not canonical (AC3-EDGE)."""
    if not name:
        return None
    tokens = name.split("-")
    verbs = dispatch_verbs()
    source: Optional[str] = None
    if len(tokens) >= 2 and tokens[0] in dispatch_sources() and tokens[1] in verbs:
        source, rest = tokens[0], tokens[2:]
    elif tokens[0] in verbs:
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


def legacy_verb_code(name: Optional[str]) -> Optional[str]:
    """Verb code for a pre-cutover convention name (``target-*`` -> ``t``,
    ``think-*`` -> ``th``), else None: the legacy-read window helper."""
    if not name:
        return None
    if name.startswith("target-"):
        return "t"
    if name.startswith("think-"):
        return "th"
    return None
