"""Module-level constants shared across the fno.graph package.

These MUST match the legacy script's values exactly -- callers depend on them.
"""
from __future__ import annotations

import math
import os
import re
from pathlib import Path


def _state_dir() -> Path:
    """Return state_dir via typed paths accessor, fallback to ~/.fno.

    Broad exception handler is intentional: the docstring contract is
    fail-open. A malformed settings.yaml raises ValidationError from
    paths.state_dir(); we want graph commands to keep working against
    the default location rather than abort with a stack trace.
    """
    try:
        from fno import paths as _paths
        return _paths.state_dir()
    except Exception:
        return Path.home() / ".fno"


def _graph_json() -> Path:
    """Route through paths.graph_json() to honour config.paths.graph_json override."""
    try:
        from fno import paths as _paths
        return _paths.graph_json()
    except Exception:
        return _state_dir() / "graph.json"


def _graph_md() -> Path:
    return _state_dir() / "graph.md"


def _graph_html() -> Path:
    return _state_dir() / "graph.html"


def _graph_archive_json() -> Path:
    """Route through paths.graph_archive_json() so the archive tracks any
    config.paths.graph_json override (it is a sibling of the working graph)."""
    try:
        from fno import paths as _paths
        return _paths.graph_archive_json()
    except Exception:
        return _state_dir() / "graph-archive.json"


def _ledger_json() -> Path:
    """Route through paths.ledger_json() to honour config.paths.ledger_json override."""
    try:
        from fno import paths as _paths
        return _paths.ledger_json()
    except Exception:
        return _state_dir() / "ledger.json"


def _briefs_dir() -> Path:
    """Route through paths.briefs_dir() to honour config.paths.briefs_dir override."""
    try:
        from fno import paths as _paths
        return _paths.briefs_dir()
    except Exception:
        return _state_dir() / "briefs"


# Module-level constants preserved as lazy properties via __getattr__ so
# existing import-time references (``from fno.graph._constants import GRAPH_JSON``)
# still work but are evaluated on first access, not at module import.
# We define them as regular names for backward-compat; they resolve state_dir()
# lazily by calling the helpers above.

def __getattr__(name: str) -> Path:
    _lazy = {
        "GRAPH_JSON": _graph_json,
        "GRAPH_MD": _graph_md,
        "GRAPH_HTML": _graph_html,
        "GRAPH_ARCHIVE_JSON": _graph_archive_json,
        "LEDGER_JSON": _ledger_json,
        "BRIEFS_DIR": _briefs_dir,
    }
    if name in _lazy:
        return _lazy[name]()
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
# ---------------------------------------------------------------------------
# Node-ID scheme: strict generation, liberal resolution.
# ---------------------------------------------------------------------------
#
# The backlog mints node IDs as ``<prefix><hex>``. The prefix and hex width are
# configurable via ``config.backlog.id_prefix`` / ``config.backlog.id_hex_width``;
# when unconfigured the generator falls back to the LEGACY scheme so an existing
# install is byte-identical.
#
# Generation is STRICT (driven by validated config). Resolution is LIBERAL: an
# ID is recognized by looking it up in the graph, not by matching a fixed
# pattern, so a graph holding IDs minted under several prefix/width settings all
# resolves with no migration. This module is the single source of truth for the
# scheme: every mint routes through ``mint_node_id`` and every format check
# through ``is_wellformed_node_id`` / ``extract_node_ids``.

LEGACY_PREFIX = "ab-"
LEGACY_HEX = 8

# Back-compat alias: callers historically imported ``ID_PREFIX``. New mint paths
# call ``mint_node_id()`` instead; this remains the legacy default prefix.
ID_PREFIX = LEGACY_PREFIX

# Reserved sibling ID families that are NOT node IDs and must never be chosen as
# a node prefix: carveouts (``cv-``), follow-ups (``fu-``), and target agent
# names (``tgt-``). The config validator + setup wizard reject these.
RESERVED_PREFIXES = frozenset({"cv-", "fu-", "tgt-"})

# Liberal, bounded, config-FREE grammar for a well-formed node id: a lowercase
# prefix (1-8 chars, letter-led) + '-' + 4-8 hex. Accepts the legacy
# ``ab-{8hex}`` and any configured ``<prefix>-<4..8hex>``. Config-free on purpose
# so pydantic validators (which run without a settings context) can use it. It is
# deliberately liberal: it also matches sibling families like ``cv-12345678``, so
# callers that EXTRACT ids from free text MUST filter the candidates against real
# graph keys (never trust the grammar alone).
NODE_ID_BODY = r"[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}"
_WELLFORMED_NODE_ID_RE = re.compile(NODE_ID_BODY)
_NODE_ID_EXTRACT_RE = re.compile(r"\b" + NODE_ID_BODY + r"\b")

# Mint retry ceiling. With width 4 (65,536 values) the birthday bound puts ~50%
# collision near ~300 nodes, so the retry loop is load-bearing; this cap bounds
# it so a near-exhausted ID space raises rather than spinning forever.
_MINT_MAX_ATTEMPTS = 10_000


def is_wellformed_node_id(s: object) -> bool:
    """True when ``s`` is a syntactically valid node id (legacy or configured).

    Config-free liberal match (see ``NODE_ID_BODY``). This is a FORMAT check,
    not an identity check: a string can be well-formed yet absent from the
    graph, and a well-formed string can belong to a sibling family - resolution
    is a graph lookup, not this predicate.
    """
    return isinstance(s, str) and _WELLFORMED_NODE_ID_RE.fullmatch(s) is not None


def extract_node_ids(text: str) -> list[str]:
    """Return candidate node-id tokens found in free ``text``, in order.

    Liberal bounded extraction. The caller MUST filter the result against real
    graph keys: this also returns sibling-family tokens (``cv-…``) and any
    hex-shaped ``<prefix>-<hex>`` substring, so trusting it alone would
    misrecognize a carveout id or a coincidental token as a node (AC4-ERR).
    """
    if not isinstance(text, str):
        return []
    return _NODE_ID_EXTRACT_RE.findall(text)


def has_node_id_prefix(s: object) -> bool:
    """Liberal pre-check for resolution gates: could ``s`` be a node id?

    Accepts (a) any well-formed node id - legacy ``ab-`` OR ANY configured
    prefix/width, so a graph holding ids minted under several past prefixes all
    resolves (the mixed-format / no-migration contract; a config change that
    switches the prefix never strands existing ids) - OR (b) a string carrying
    the configured/legacy prefix with a non-strict suffix, which tolerates the
    non-hex test/legacy ids that still resolve by exact graph lookup.

    Deliberately liberal: the authoritative check is always the graph lookup
    that follows. For an unconfigured install this still admits at least every
    ``ab-``-prefixed string, byte-identical to the historical
    ``startswith(ID_PREFIX)`` gate. This only rejects obviously-non-id input
    early with a clear message.
    """
    if not isinstance(s, str):
        return False
    if is_wellformed_node_id(s):
        return True
    return s.startswith(node_id_prefix()) or s.startswith(LEGACY_PREFIX)


def node_id_suffix(node_id: str) -> str:
    """Return the part of ``node_id`` after its first ``-`` (the hex tail).

    ``ab-a3f9c1d2`` -> ``a3f9c1d2``; ``xy-a3f9`` -> ``a3f9``. Used to derive a
    prefix-independent handle (e.g. a ``tgt-`` agent name) by stripping the
    configured prefix at the ``-`` boundary rather than a hardcoded ``[3:]``.
    """
    return node_id.split("-", 1)[1] if "-" in node_id else node_id


def node_id_prefix() -> str:
    """Configured ``config.backlog.id_prefix`` or the legacy ``ab-`` fallback.

    Fail-open: a malformed settings.yaml (ValidationError) or an unset key
    yields the legacy prefix rather than raising, so minting never crashes
    (AC2-ERR).
    """
    try:
        from fno.config import load_settings
        p = load_settings().backlog.id_prefix
        return p if p else LEGACY_PREFIX
    except Exception:
        return LEGACY_PREFIX


def node_id_hex_width() -> int:
    """Configured ``config.backlog.id_hex_width`` or the legacy width 8.

    Fail-open like ``node_id_prefix``. An absent key resolves to 8 (legacy
    preservation), NOT the setup wizard's offered default of 4.
    """
    try:
        from fno.config import load_settings
        return int(load_settings().backlog.id_hex_width)
    except Exception:
        return LEGACY_HEX


def mint_node_id(existing_ids) -> str:
    """Mint a fresh, collision-free node id using the configured scheme.

    ``<prefix><hex>`` where prefix/width come from config (legacy fallback).
    Retries on collision against ``existing_ids`` (any container of current
    graph keys), bounded by ``_MINT_MAX_ATTEMPTS``; raises RuntimeError when the
    ID space is near exhaustion rather than looping forever (AC2-EDGE).

    MUST be called inside ``locked_mutate_graph`` so ``existing_ids`` is a
    consistent snapshot and two concurrent mints cannot collide (AC2-FR).
    """
    import uuid
    prefix = node_id_prefix()
    width = node_id_hex_width()
    for _ in range(_MINT_MAX_ATTEMPTS):
        candidate = f"{prefix}{uuid.uuid4().hex[:width]}"
        if candidate not in existing_ids:
            return candidate
    count = len(existing_ids) if hasattr(existing_ids, "__len__") else "?"
    raise RuntimeError(
        f"node-ID space near exhaustion: {_MINT_MAX_ATTEMPTS} mint attempts at "
        f"prefix {prefix!r} width {width} all collided ({count} existing ids). "
        "Increase config.backlog.id_hex_width."
    )


LOCK_TTL_HOURS = float(os.environ.get("TASK_LOCK_TTL_HOURS", "2"))

PRIORITY_ORDER: dict[str, int] = {"p0": 0, "p1": 1, "p2": 2, "p3": 3}
PRIORITY_MIGRATION: dict[str, str] = {"high": "p1", "medium": "p2", "low": "p3"}
DEFAULT_PRIORITY: str = "p2"

# Tags (x-6c2b wave 1): lowercase-kebab only, so they mirror cleanly into
# Obsidian frontmatter `tags:` and stay legible as Base/tag-search filters.
TAG_CHARSET_RE = re.compile(r"^[a-z0-9-]+$")

# Epic nesting cap (x-6c2b wave 3): mission -> epic -> leaf. Two epic levels
# keep rollup O(children) and the mental model flat; deeper nesting refuses.
EPIC_NEST_MAX_DEPTH: int = 2


def normalize_tag(raw: str) -> str:
    """Lowercase-trim a tag and validate its charset.

    Returns the normalized tag ([a-z0-9-]). Raises ValueError naming the
    allowed charset on anything else, so callers refuse malformed input rather
    than storing garbage the frontmatter mirror would carry.
    """
    tag = raw.strip().lower()
    if not tag or not TAG_CHARSET_RE.match(tag):
        raise ValueError(
            f"invalid tag {raw!r}: tags must be lowercase-kebab [a-z0-9-] "
            "(letters, digits, hyphens)"
        )
    return tag


def _rank_band(entry: dict) -> tuple:
    """Rank band shared by the board lane key and the walker selection key.

    Returns ``(0, float(rank))`` for a finite, non-bool ``rank`` and
    ``(1, 0.0)`` otherwise, so ranked nodes (band 0, ascending rank) precede
    ALL unranked nodes (band 1). ``bool`` is excluded (it subclasses ``int``
    but is never a real rank).

    This is the SINGLE source of truth for "what counts as ranked, in what
    order" (Locked Decision 4): ``render._lane_sort_key`` (the board)
    and ``_intake.make_selection_sort_key`` (``fno backlog next`` / the walker)
    both prepend this term, so the board can never disagree with work order.

    Degrades NaN/inf AND huge-int ranks (from a hand-edited graph.json) to
    unranked: ``float()`` guards the ``OverflowError`` a giant int raises, and
    ``isfinite`` then excludes NaN/inf so the key stays a total order (NaN
    compares False both ways). A non-finite rank must never raise here - the
    board render fires inside ``locked_mutate_graph`` and only ``OSError`` is
    swallowed upstream.
    """
    rank = entry.get("rank")
    if isinstance(rank, bool) or not isinstance(rank, (int, float)):
        return (1, 0.0)
    try:
        f = float(rank)
    except (OverflowError, ValueError):
        return (1, 0.0)
    if math.isfinite(f):
        return (0, f)
    return (1, 0.0)
