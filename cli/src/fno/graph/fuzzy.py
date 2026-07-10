"""Fuzzy id and domain matching for graph entries.

Near-pure: callers pass already-loaded entries (from `read_graph()`) and get
back a discriminated result dataclass the CLI can format into a user-facing
message. The one exception is `resolve_node`'s bare-hex tier, which consults
the fail-open, lru-cached `node_id_prefix()` (file I/O) to re-prefix; the import
is call-time so the module stays import-pure.

Two public functions:
    resolve_id(query, entries, *, git_branch) -> IdMatch
    suggest_domain(query, entries) -> DomainSuggestion

Design notes:
- No Levenshtein or rapidfuzz. All matching is simple token-subset substring
  against titles, or case-insensitive prefix against the domain history.
- Branch-derived tokens are extracted via a small _branch_tokens helper that
  strips `feat/`, `fix/`, etc. and drops tokens that are digits-only or
  <= 2 chars.
- Ambiguity is first-class: the matcher NEVER silently picks one of many.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Literal, Optional

Entry = dict


# Pre-compiled regexes for the ab-id prefix path. Module-level so the
# patterns are bound exactly once at import time rather than re-parsed
# on every resolve_id call. (Python's re module caches by string key,
# but explicit module constants are clearer and one fewer indirection.)
_AB_FULL_HEX_RE = re.compile(r"[0-9a-f]{8}")
_AB_PARTIAL_HEX_RE = re.compile(r"[0-9a-f]{4,7}")
# A bare node-id shape: 4-8 lowercase hex, no prefix, no hyphen. The
# autocorrect-neutral phone path (iOS rewrites `ab-` and the hyphen). Bounded
# 4-8 so a 10-char hex string is NOT mistaken for an id (AC4-ERR); the range
# spans the legacy width 8 and configured widths (setup offers 4).
_BARE_HEX_RE = re.compile(r"[0-9a-f]{4,8}")


_KNOWN_BRANCH_PREFIXES = (
    "feat/",
    "feature/",
    "fno/",  # x-ff83 W3: dispatched branches are <prefix>/<slug>-<node> (default fno)
    "fix/",
    "hotfix/",
    "bugfix/",
    "docs/",
    "chore/",
    "refactor/",
    "test/",
    "ci/",
    "perf/",
    "build/",
    "style/",
    "revert/",
)


def _branch_tokens(branch: str) -> list[str]:
    """Derive searchable tokens from a git branch name.

    - Strip known prefixes (feat/, fix/, docs/, ...)
    - Split on `-` and `_`
    - Drop digits-only tokens
    - Drop tokens of length <= 2
    - Lowercase everything
    """
    if not branch:
        return []
    stripped = branch
    lowered = branch.lower()
    for prefix in _KNOWN_BRANCH_PREFIXES:
        if lowered.startswith(prefix):
            stripped = branch[len(prefix):]
            break
    raw = stripped.lower().replace("_", " ").replace("-", " ")
    tokens = [t for t in raw.split() if t]
    return [t for t in tokens if len(t) > 2 and not t.isdigit()]


@dataclass(frozen=True)
class IdMatch:
    """Discriminated result of resolve_id.

    Fields:
        kind: one of "exact" | "fuzzy" | "branch_derived" | "none" | "ambiguous".
        id: resolved id for exact/fuzzy/branch_derived; first candidate for
            ambiguous (convenience); None for "none".
        candidates: populated (entries, not ids) for any non-empty match.
            Length 1 for exact/fuzzy/branch_derived; >=2 for ambiguous;
            empty tuple for kind="none". Lets callers read the matched
            entry without an extra O(N) scan of the entries list.
        note: human-readable reason, suitable for CLI error messages.
    """

    kind: Literal["exact", "fuzzy", "branch_derived", "none", "ambiguous"]
    id: Optional[str] = None
    candidates: tuple[Entry, ...] = field(default_factory=tuple)
    note: str = ""


def _all_tokens_in_title(tokens: list[str], title: str) -> bool:
    lt = (title or "").lower()
    return all(tok in lt for tok in tokens)


def _substring_match(
    tokens: list[str],
    entries: list[Entry],
) -> list[Entry]:
    """Return entries whose title contains all tokens (case-insensitive).

    Caller may pass already-filtered (non-done first) entries or the full list.
    """
    if not tokens:
        return []
    return [e for e in entries if _all_tokens_in_title(tokens, e.get("title") or "")]


def _score_tokens_in_title(tokens: list[str], title: str) -> int:
    lt = (title or "").lower()
    return sum(1 for t in tokens if t in lt)


def _best_scored_match(
    tokens: list[str],
    entries: list[Entry],
) -> list[Entry]:
    """Return entries tied at the highest token-match score (minimum 1).

    Unlike _substring_match, this is forgiving: branch names often contain
    descriptive suffixes (first-run-ux) that don't appear in every candidate
    title. Branch derivation uses this to pick the most-matching entry,
    surfacing a tie as ambiguity.
    """
    if not tokens or not entries:
        return []
    scored = [(_score_tokens_in_title(tokens, e.get("title") or ""), e) for e in entries]
    top_score = max((s for s, _ in scored), default=0)
    if top_score == 0:
        return []
    return [e for s, e in scored if s == top_score]


def _split_by_status(entries: list[Entry]) -> tuple[list[Entry], list[Entry]]:
    """Return (non_done, done). 'done' status is the only terminal state."""
    non_done: list[Entry] = []
    done: list[Entry] = []
    for e in entries:
        if e.get("_status") == "done":
            done.append(e)
        else:
            non_done.append(e)
    return non_done, done


def _build_match_result(
    matches: list[Entry],
    *,
    kind_single: Literal["fuzzy", "branch_derived"],
    note_prefix: str,
) -> IdMatch:
    if not matches:
        return IdMatch(kind="none", note=f"{note_prefix}: no matches")
    if len(matches) == 1:
        return IdMatch(
            kind=kind_single,
            id=matches[0].get("id"),
            candidates=(matches[0],),
            note=f"{note_prefix}: matched '{matches[0].get('title')}'",
        )
    return IdMatch(
        kind="ambiguous",
        id=matches[0].get("id"),
        candidates=tuple(matches),
        note=f"{note_prefix}: {len(matches)} candidates",
    )


def resolve_id(
    query: Optional[str],
    entries: list[Entry],
    *,
    git_branch: Optional[str] = None,
) -> IdMatch:
    """Resolve user input to a graph entry id.

    Precedence:
      1. query starts with 'ab-' and matches an entry id exactly.
      2. query empty/None AND git_branch supplied -> derive tokens + fuzzy match.
      3. query non-empty -> token-subset substring match against entry titles.
    Prefers non-done entries; falls back to done if no non-done matches.
    """
    q = (query or "").strip()

    # Exact id match is format-agnostic (any configured prefix/width AND legacy
    # ab-): a graph lookup, not a regex, so a graph holding mixed-format ids all
    # resolves. Exact equality wins regardless of suffix shape - test fixtures
    # and legacy data sometimes use ids with non-hex characters.
    for e in entries:
        if e.get("id") == q:
            return IdMatch(
                kind="exact",
                id=q,
                candidates=(e,),
                note=f"exact id match: {e.get('title', '')}",
            )

    if q.startswith("ab-"):
        # No exact hit above. The ab- partial-prefix + malformed-query guards
        # below stay ab-specific (a typed convenience; configured partials are
        # out of scope). If the suffix is 4-7 hex chars, treat as a prefix
        # query. Full 8-hex queries that didn't match exact return 'none'
        # (no false-positive prefix hits when the user typed a complete id).
        suffix = q[3:]  # everything after "ab-"
        is_full = bool(_AB_FULL_HEX_RE.fullmatch(suffix))
        is_partial = bool(_AB_PARTIAL_HEX_RE.fullmatch(suffix))

        if is_full:
            return IdMatch(
                kind="none",
                note=f"no entry with id '{q}'",
            )

        if is_partial:
            matches = [e for e in entries if (e.get("id") or "").startswith(q)]
            return _build_match_result(
                matches,
                kind_single="fuzzy",
                note_prefix=f"id prefix '{q}'",
            )

        # Malformed ab-... query (too short for prefix, or non-hex chars).
        # Return kind='none' explicitly rather than falling through to the
        # title-fuzzy path below: an "ab-" prefix is a strong user signal
        # that they want id resolution, and silently matching such a query
        # against a title that happens to contain the literal substring
        # would be a hard-to-diagnose wrong match (e.g. 'ab-9728b70b,'
        # with a trailing comma fuzzying onto an unrelated entry).
        return IdMatch(
            kind="none",
            note=f"malformed ab- query '{q}' (suffix must be 4-8 hex chars)",
        )

    if not q and git_branch:
        tokens = _branch_tokens(git_branch)
        if not tokens:
            return IdMatch(
                kind="none",
                note=f"branch '{git_branch}' produced no searchable tokens",
            )
        non_done, done = _split_by_status(entries)
        matches = _best_scored_match(tokens, non_done) or _best_scored_match(tokens, done)
        return _build_match_result(
            matches,
            kind_single="branch_derived",
            note_prefix=f"branch '{git_branch}'",
        )

    if q:
        tokens = [t for t in q.lower().split() if t]
        if not tokens:
            return IdMatch(kind="none", note="empty query after tokenization")
        non_done, done = _split_by_status(entries)
        matches = _substring_match(tokens, non_done)
        if not matches:
            matches = _substring_match(tokens, done)
        return _build_match_result(
            matches,
            kind_single="fuzzy",
            note_prefix=f"query '{q}'",
        )

    return IdMatch(
        kind="none",
        note="empty query and no git_branch provided",
    )


# -- slug + bare-hex resolution (ab-f82e8083) --------------------------------
#
# resolve_node implements the deterministic resolution tiers 1-3 for a spawn /
# lookup target: exact ab-id, exact slug, bare-8-hex re-prefix. It is the
# precise, additive complement to resolve_id (which owns the title-fuzzy +
# id-prefix + branch-derived paths used by `done` / intake / graph-resolve.sh -
# left untouched so those resolutions never broaden). On a miss it returns
# kind="none" so the caller can escalate to the model-judged describe-it tier.


def resolve_node(query: Optional[str], entries: list[Entry]) -> IdMatch:
    """Resolve a spawn/lookup target via the deterministic tiers 1-3.

    Order (stop at the first hit):
      1. exact ``ab-{8hex}`` id.
      2. exact slug -> its ab-id (slugs are globally unique).
      3. bare 4-8 lowercase-hex -> re-prefix (configured prefix, then ``ab-``)
         and match an exact id.
    Returns kind="exact" on a hit, else kind="none" (so the caller escalates to
    the describe-it tier). Never fuzzy-matches.
    """
    q = (query or "").strip()
    if not q:
        return IdMatch(kind="none", note="empty query")

    # Tier 1: exact ab-id (canonical, unchanged behavior).
    for e in entries:
        if e.get("id") == q:
            return IdMatch(
                kind="exact", id=q, candidates=(e,),
                note=f"exact id match: {e.get('title', '')}",
            )

    # Tier 2: exact slug -> ab-id. Case-insensitive: slugs are always stored
    # lowercase, so comparing against q.lower() lets a mobile-auto-capitalized
    # input (`Dashless-spawn`) resolve - the whole point of this phone-ergonomic
    # feature. Globally unique by construction, so at most one node matches.
    q_lc = q.lower()
    for e in entries:
        slug = e.get("slug")
        if isinstance(slug, str) and slug and slug == q_lc:
            return IdMatch(
                kind="exact", id=e.get("id"), candidates=(e,),
                note=f"exact slug '{q}' -> {e.get('id')}",
            )

    # Tier 3: bare hex -> re-prefix and match exactly. Try the configured node
    # id prefix first (so a repo on `x-`/4hex seeds `4af4` -> `x-4af4`), then
    # legacy `ab-` for back-compat with mixed-format graphs. Configured-first
    # makes an ambiguous hex (a key under both) resolve deterministically.
    if _BARE_HEX_RE.fullmatch(q):
        # ponytail: call-time import keeps the module import-pure; node_id_prefix
        # does file I/O (load_settings) and already fails open to `ab-`.
        from fno.graph._constants import node_id_prefix
        prefixes = list(dict.fromkeys((node_id_prefix(), "ab-")))
        by_id = {e.get("id"): e for e in entries if e.get("id")}
        for p in prefixes:
            cand = f"{p}{q}"
            e = by_id.get(cand)
            if e is not None:
                return IdMatch(
                    kind="exact", id=cand, candidates=(e,),
                    note=f"bare hex '{q}' -> {cand}",
                )
        tried = ", ".join(f"{p}{q}" for p in prefixes)
        return IdMatch(
            kind="none",
            note=f"no node matching bare hex '{q}' (tried: {tried})",
        )

    return IdMatch(
        kind="none",
        note=f"no exact id/slug/bare-hex match for '{q}'",
    )


def search_entries(
    query: Optional[str],
    entries: list[Entry],
    *,
    fields: tuple[str, ...] = ("title", "slug", "details"),
) -> list[Entry]:
    """High-recall token-substring search across ``fields`` (describe-it).

    Returns ALL entries whose concatenated field text contains every query
    token (case-insensitive), non-done first, so the model has the full
    candidate set to rank. Empty list on no tokens or no match. A missing field
    (e.g. a node with no ``details``) simply contributes nothing to match
    against - it is not an error.
    """
    tokens = [t for t in (query or "").lower().split() if t]
    if not tokens:
        return []

    def _hit(e: Entry) -> bool:
        text = " ".join(
            v.lower() for f in fields if isinstance((v := e.get(f)), str)
        )
        return all(tok in text for tok in tokens)

    non_done, done = _split_by_status(entries)
    return [e for e in non_done if _hit(e)] + [e for e in done if _hit(e)]


@dataclass(frozen=True)
class DomainSuggestion:
    """Discriminated result of suggest_domain.

    Fields:
        match: the resolved domain string (input verbatim on 'new').
        confidence: "exact" | "fuzzy" | "new".
        history: all distinct domains observed in entries, sorted, deduped.
    """

    match: str
    confidence: Literal["exact", "fuzzy", "new"]
    history: tuple[str, ...] = field(default_factory=tuple)


def _domain_history(entries: list[Entry]) -> tuple[str, ...]:
    seen = {e.get("domain") for e in entries if e.get("domain")}
    return tuple(sorted(d for d in seen if isinstance(d, str)))


def suggest_domain(query: str, entries: list[Entry]) -> DomainSuggestion:
    """Suggest a domain, matched against historical entries.

    Resolution order:
      1. Empty history and empty query -> ("code", "new", ())
      2. Empty query, non-empty history -> ("code" if present else first alphabetical, "new", history)
      3. Exact match against history -> ("<query>", "exact", history)
      4. Unique case-insensitive prefix match -> ("<match>", "fuzzy", history)
      5. No match or ambiguous prefix -> ("<query verbatim>", "new", history)
    """
    q = (query or "").strip()
    history = _domain_history(entries)

    if not q:
        if not history:
            return DomainSuggestion(match="code", confidence="new", history=())
        pick = "code" if "code" in history else history[0]
        return DomainSuggestion(match=pick, confidence="new", history=history)

    if q in history:
        return DomainSuggestion(match=q, confidence="exact", history=history)

    ql = q.lower()
    prefix_hits = [d for d in history if d.lower().startswith(ql)]
    if len(prefix_hits) == 1:
        return DomainSuggestion(match=prefix_hits[0], confidence="fuzzy", history=history)

    return DomainSuggestion(match=q, confidence="new", history=history)
