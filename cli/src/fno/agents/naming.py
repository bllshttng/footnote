"""The single owner of agent-name generation: :func:`agent_name` budgets the
64-char daemon contract; :func:`dispatch_agent_name` owns the x-84b2
source/verb vocabulary (data in ``naming-codes.yaml``). The daemon stays the
validator at the spawn boundary and must never become the generator."""

from __future__ import annotations

import json
import re
import subprocess
from dataclasses import dataclass
from functools import lru_cache
from typing import Optional

#: The daemon's public agent-name contract: 1-64 chars of ``[A-Za-z0-9_-]``.
MAX_LEN = 64
#: Per-component cap for human-readable text, matching the shell dispatchers'
#: ``cut -c1-30``.
SLUG_CAP = 30

def _binary() -> str:
    from fno import rust_binary

    # The full resolver (env override -> bundled -> sibling -> PATH -> cargo
    # dev), not the installed-only one: the vocabulary owner lives in the
    # binary, so a dev checkout resolves its own fresh build.
    binary = rust_binary.resolve_binary()
    if binary is None:
        raise AgentNameError(
            "no fno-agents binary found: the name vocabulary lives in the Rust "
            "runtime; run `fno doctor update`"
        )
    return str(binary)


def _mint(*args: str) -> str:
    import os

    proc = subprocess.run(
        [_binary(), "name-mint", *args],
        capture_output=True,
        text=True,
        timeout=30,
        env={**os.environ, "FNO_AGENTS_RUNTIME": "rust"},
    )
    if proc.returncode == 0:
        lines = proc.stdout.strip().splitlines()
        return lines[-1] if lines else ""
    message = proc.stderr.strip()
    if message.startswith("error: "):
        message = message[len("error: "):]
    if proc.returncode == 3:
        raise AgentNameError(message)
    raise BridgeUsageError(message)


def _opt(value: Optional[str]) -> list[str]:
    value = (value or "").strip()
    return [] if not value else [value]


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
        code = verb if verb in dispatch_verbs() else (verb_code_for(verb) if verb else "")
        args: list[str] = []
        if _opt(source):
            args += ["--source", source or ""]
        if _opt(code):
            args += ["--verb", code]
        if not _opt(code):
            raise BridgeUsageError("--source requires --verb")
        return _mint(*args, node_id,
                     *( ["--slug", slug] if _opt(slug) else []),
                     *( ["--qualifier", qualifier] if _opt(qualifier) else []),
                     *( ["--discriminator", discriminator] if _opt(discriminator) else []))
    if not prefix:
        raise BridgeUsageError("a prefix or --verb is required")
    return agent_name(
        prefix, node_id, slug=slug, qualifier=qualifier, discriminator=discriminator
    )


@lru_cache(maxsize=1)
def _codes() -> dict:
    """The vocabulary tables, served by the binary that owns them."""
    import os

    proc = subprocess.run(
        [_binary(), "name-codes", "--json"],
        capture_output=True,
        text=True,
        timeout=30,
        env={**os.environ, "FNO_AGENTS_RUNTIME": "rust"},
    )
    if proc.returncode != 0:
        raise AgentNameError("name-codes read failed: the fno-agents binary is stale")
    raw = json.loads(proc.stdout)
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

    The name is the dedup token for ``fno agents spawn``: required identity
    never shaves; only the human slug gives way. :raises AgentNameError:
    over-budget required identity.
    """
    flags: list[str] = []
    for flag, value in (
        ("--slug", slug),
        ("--qualifier", qualifier),
        ("--discriminator", discriminator),
    ):
        if _opt(value):
            flags += [flag, value or ""]
    if not (prefix or "").strip() and not (node_id or "").strip():
        # The bridge reads empty-everything as a usage error; the Python owner
        # always refused it as a naming error - keep that contract for the
        # in-process producers.
        raise AgentNameError("agent name needs at least a prefix or a node id")
    return _mint(prefix, node_id, *flags)


def verb_code_for(word: Optional[str]) -> str:
    """The verb code for a work-verb word (``/target``, ``/fno:blueprint``,
    ``builtin``, ...). Unknown words raise: nothing defaults to ``t``. The
    word-normalization is trivial text handling; the TABLE it reads is the
    binary's (``name-codes``), so no second copy of the vocabulary exists."""
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
        # The bridge maps work-verb words; the dispatch seam takes codes only.
        raise AgentNameError(f"unknown dispatch verb {verb!r}")
    flags: list[str] = []
    for flag, value in (
        ("--slug", slug),
        ("--qualifier", qualifier),
        ("--discriminator", discriminator),
    ):
        if _opt(value):
            flags += [flag, value or ""]
    args: list[str] = []
    if _opt(source):
        args += ["--source", source or ""]
    if _opt(verb):
        args += ["--verb", verb or ""]
    return _mint(*args, identity, *flags)


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


def parse_many(names: list[str]) -> list[Optional[DispatchName]]:
    """Batch parse: one binary exec for a whole list of candidate names. The
    hot readers (cleanup candidate scan, truth-status row reads) call this;
    single-name callers use :func:`parse_dispatch_agent_name`."""
    if not names:
        return []
    import os

    proc = subprocess.run(
        [_binary(), "name-parse"],
        input="\n".join(names),
        capture_output=True,
        text=True,
        timeout=30,
        env={**os.environ, "FNO_AGENTS_RUNTIME": "rust"},
    )
    if proc.returncode != 0:
        raise AgentNameError("name-parse failed: the fno-agents binary is stale")
    out: list[Optional[DispatchName]] = []
    for line in proc.stdout.splitlines():
        row = json.loads(line)
        if row.get("verb") is None:
            out.append(None)
            continue
        out.append(
            DispatchName(
                row["name"], row.get("source"), row["verb"], row.get("node"),
                row.get("tail") or "",
            )
        )
    return out


def parse_dispatch_agent_name(name: Optional[str]) -> Optional[DispatchName]:
    """Parse ``[<source>-]<verb>-<identity>``, else None. Positional
    grammar: the first token is a source only when the second is a verb, so a
    node prefix colliding with a code cannot misread. Pre-cutover names are
    not canonical (AC3-EDGE)."""
    if not name:
        return None
    return parse_many([name])[0]


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
