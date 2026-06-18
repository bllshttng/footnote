"""Title-derived human-handle slugs for graph nodes (ab-f82e8083).

A slug is a stable, title-derived handle that LEADS in display and is an
accepted resolution input, while ``ab-{8hex}`` stays the canonical key. The
slug is derived once when a node is first persisted, globally unique (the
collision suffix is baked in at assignment), and immutable thereafter - a later
title reword does NOT change it.

Pure helpers + one entries-mutating pass:
    derive_base_slug(title) -> str            # slugify + word/length budget
    assign_unique_slug(base, node_id, taken) -> str
    ensure_slugs(entries) -> int              # idempotent backfill/assign pass
    format_handle(node) -> str                # 'slug (ab-id)' display handle

Derivation reuses the established slugify (completion-summary.py:156):
``re.sub(r"[^a-z0-9]+","-", title.lower()).strip("-")``, trimmed to a small
word budget and length cap so the handle stays short and memorable.
"""
from __future__ import annotations

import re

from fno.graph._constants import node_id_suffix

# Collapse any run of non-alphanumeric characters into a single hyphen.
_SLUG_SUB_RE = re.compile(r"[^a-z0-9]+")

# A tiny stopword set dropped from the handle so a slug reads as the meaningful
# words of the title. Kept deliberately small: it trims connective noise
# ("for", "the") without mangling domain terms.
_STOPWORDS = frozenset(
    {"a", "an", "the", "of", "for", "to", "and", "or", "in", "on", "with"}
)

# Keep the handle short: at most this many words, capped at this many chars.
_WORD_BUDGET = 6
_LEN_CAP = 48


def derive_base_slug(title: str) -> str:
    """Slugify a title into a base handle (may be empty for all-punct titles).

    Lowercase, collapse non-alphanumeric runs to a single hyphen, drop a small
    stopword set (but never empty the result), keep at most ``_WORD_BUDGET``
    words, and cap at ``_LEN_CAP`` chars on a word boundary so the cap never
    splits a token mid-word. Returns "" when the title slugifies to nothing -
    the caller (``assign_unique_slug``) applies a hex fallback.
    """
    raw = _SLUG_SUB_RE.sub("-", (title or "").lower()).strip("-")
    if not raw:
        return ""
    words = [w for w in raw.split("-") if w]
    # Drop stopwords, but if that empties the list (an all-stopword title) keep
    # the original words rather than returning "".
    kept = [w for w in words if w not in _STOPWORDS] or words
    out: list[str] = []
    length = 0
    for w in kept[:_WORD_BUDGET]:
        add = len(w) + (1 if out else 0)  # +1 for the joining hyphen
        # Break on overflow for ANY word, including the first - gating on `out`
        # would let a single over-long word through whole and silently blow the
        # cap (it also made the truncation fallback below unreachable).
        if length + add > _LEN_CAP:
            break
        out.append(w)
        length += add
    if not out:
        # The very first word alone exceeds the cap: hard-truncate it.
        return kept[0][:_LEN_CAP]
    return "-".join(out)


def _hex_fallback_slug(node_id: str) -> str:
    """Fallback handle for a title that slugifies to empty: ``node-<8hex>``.

    Derived from the node's canonical id suffix, so it is globally unique by
    construction (the id is) and never blank.
    """
    suffix = node_id_suffix(node_id)
    return f"node-{suffix or 'unknown'}"


def assign_unique_slug(base: str, node_id: str, taken: set[str]) -> str:
    """Return a globally-unique slug from ``base``, suffixing on collision.

    Empty base -> hex fallback. A collision against ``taken`` appends a
    deterministic ``-2``, ``-3``, ... suffix until free. The returned slug is
    NOT added to ``taken`` (the caller owns the set).
    """
    candidate = base or _hex_fallback_slug(node_id)
    if candidate not in taken:
        return candidate
    n = 2
    while f"{candidate}-{n}" in taken:
        n += 1
    return f"{candidate}-{n}"


def ensure_slugs(entries: list[dict]) -> int:
    """Assign a slug to every entry lacking one; return the count assigned.

    Idempotent: an entry that already carries a non-empty slug is left untouched
    (the immutability invariant), so re-running is a no-op. Newly assigned slugs
    are unique against all existing slugs AND each other within the pass.
    Mutates ``entries`` in place; safe to call inside ``locked_mutate_graph``.
    """
    taken = {
        e["slug"]
        for e in entries
        if isinstance(e.get("slug"), str) and e.get("slug")
    }
    assigned = 0
    for e in entries:
        existing = e.get("slug")
        if isinstance(existing, str) and existing:
            continue
        base = derive_base_slug(e.get("title") or "")
        slug = assign_unique_slug(base, e.get("id") or "", taken)
        e["slug"] = slug
        taken.add(slug)
        assigned += 1
    return assigned


def format_handle(node: dict) -> str:
    """Display handle: ``slug (ab-id)`` when slugged, else ``(ab-id)``.

    The slug leads and the canonical hex trails (still copyable, greppable). A
    pre-backfill node with no slug shows the hex alone - never a blank handle.
    """
    nid = node.get("id") or "?"
    slug = node.get("slug")
    if isinstance(slug, str) and slug:
        return f"{slug} ({nid})"
    return f"({nid})"
