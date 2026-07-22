"""Backlog capture: the capture tier below idea nodes.

The inbox is a markdown holding-pen (default
``internal/fno/backlog/inbox.md``) of self-contained checkbox items,
each with a stable ``fu-XXXXXX`` shortcode. Capture is a cheap markdown
append, not a graph mutation. Triage promotes warranted items into real
``ab-XXXXXXXX`` graph nodes.

Line states (struck, never deleted, so autocorrect provenance survives). The
hyphen is the written separator since the Phase 2 separator migration; the
legacy em-dash still parses on read:
  - open:      ``- [ ] fu-3a8c1f - title (p1)``
  - promoted:  ``- [x] fu-3a8c1f - title (p1) -> ab-XXXXXXXX``
  - dismissed: ``- [-] fu-3a8c1f - title (p1) (dismissed: <reason>)``

Items are addressed by ``fu-id``, never by line number (Obsidian edits shift
lines). See internal/fno/design/2026-05-22-abi-backlog-inbox.md.

CLI surface: ``fno backlog capture`` (canonical) with ``fno backlog inbox``
kept as a hidden deprecated alias (same Typer app; see the graph -> backlog
precedent). The managed data file intentionally keeps its ``inbox.md`` name:
renaming user data breaks Obsidian links for zero collision relief.
"""
from __future__ import annotations

import fcntl
import json
import os
import re
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable, Optional

import typer

from fno.graph._constants import NODE_ID_BODY

# ---------------------------------------------------------------------------
# Constants + parsing
# ---------------------------------------------------------------------------

MAX_WHY_LEN = 120
SOFT_CEILING = 100
VALID_PRIORITIES = {"p0", "p1", "p2", "p3"}

# MINTED fu-id grammar: 6 lowercase hex (mint_fu_id). Used for collision
# avoidance (_existing_ids) and the bare-token unparseable-line warning. The
# RECOGNITION grammar (_FU_TOKEN) is broader: it also matches hand-authored slug
# ids like fu-cwd339 / fu-codex-errpaths that live inboxes carry (ab-932f5a92),
# so the checkbox-line item parsers see them while minting stays strictly 6-hex.
FU_RE = re.compile(r"fu-[0-9a-f]{6}")
_FU_TOKEN = r"fu-[a-z0-9][a-z0-9-]*"
_MEMORY_SLUG_RE = re.compile(r"^\s*\[\[.*\]\]\s*$")
# Item line: "- [<mark>] fu-xxxxxx <sep> <title-with-optional-(pN)-suffix>".
# Accepts BOTH separators (legacy em-dash and the target hyphen) so the fu-only
# read/write paths (parse_items/list, promote, dismiss, archive_struck, triage)
# keep working after `tidy` normalizes separators to hyphen. add/promote/dismiss
# now WRITE the hyphen (Phase 2 separator migration). The fu-token is _FU_TOKEN
# (minted 6-hex OR a hand-authored slug, ab-932f5a92); minting stays 6-hex.
_ITEM_RE = re.compile(r"^- \[([ x\-])\]\s+(" + _FU_TOKEN + r")\s+(?:—|-)\s+(.*?)\s*$")
_PRIORITY_SUFFIX_RE = re.compile(r"\s*\((p[0-3])\)\s*$")

_STATUS_BY_MARK = {" ": "open", "x": "promoted", "-": "dismissed"}

# Mechanical scan trigger language (the deterministic half; the LLM judges).
_SCAN_RE = re.compile(
    r"deferred|skipped|won't fix now|wont fix now|follow-up|followup|"
    r"\bp2\b|\bp3\b|ship.{0,3}later|\bTODO\b",
    re.IGNORECASE,
)


class InboxValidationError(ValueError):
    """Raised when an inbox write violates a field rule (maps to exit code 2)."""


class InboxLockError(RuntimeError):
    """Raised when the inbox file lock cannot be acquired (maps to exit code 1)."""


# ---------------------------------------------------------------------------
# fu-id minting
# ---------------------------------------------------------------------------

def mint_fu_id(existing: Iterable[str]) -> str:
    """Mint a fresh ``fu-XXXXXX`` id not present in ``existing``.

    16M id space; collision is effectively impossible, but the retry removes
    a silent-failure class (two items sharing an id would make promote/dismiss
    ambiguous).
    """
    taken = set(existing)
    for _ in range(1000):
        candidate = "fu-" + uuid.uuid4().hex[:6]
        if candidate not in taken:
            return candidate
    raise RuntimeError("could not mint a unique fu-id after 1000 attempts")


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

def _split_priority(raw_title: str) -> tuple[str, Optional[str]]:
    """Split a trailing ``(pN)`` suffix off a title. Returns (title, priority)."""
    m = _PRIORITY_SUFFIX_RE.search(raw_title)
    if not m:
        return raw_title.strip(), None
    return raw_title[: m.start()].strip(), m.group(1)


def parse_items(text: str, *, include_struck: bool = False) -> list[dict]:
    """Parse inbox markdown into item dicts.

    Returns ``[{"id", "title", "priority", "status", "line"}]``. By default
    only ``open`` (``[ ]``) items are returned; pass ``include_struck=True``
    to also surface promoted/dismissed lines.
    """
    items: list[dict] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        m = _ITEM_RE.match(line)
        if not m:
            continue
        mark, fu_id, rest = m.group(1), m.group(2), m.group(3)
        status = _STATUS_BY_MARK.get(mark, "open")
        if status != "open" and not include_struck:
            continue
        title, priority = _split_priority(rest)
        items.append(
            {
                "id": fu_id,
                "title": title,
                "priority": priority,
                "status": status,
                "line": lineno,
            }
        )
    return items


def _existing_ids(text: str) -> set[str]:
    return set(FU_RE.findall(text))


def find_unparseable_fu_lines(text: str) -> list[tuple[int, str]]:
    """Return ``(lineno, line)`` for lines that carry a ``fu-`` token but fail
    the strict item regex.

    Such a line is invisible to ``parse_items``/``list``/triage yet still counts
    toward collision avoidance (``_existing_ids`` uses the loose ``FU_RE``). That
    asymmetry silently drops a hand-edited item (e.g. a missing separator, or a
    non-checkbox prose mention of the token) from every read path. Surfacing it
    lets ``list`` warn instead of swallowing the line. Both the em-dash and the
    hyphen separator parse fine since the Phase 2 migration, so a hyphen is NOT
    a broken line.
    """
    bad: list[tuple[int, str]] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        if FU_RE.search(line) and not _ITEM_RE.match(line):
            bad.append((lineno, line.strip()))
    return bad


# ---------------------------------------------------------------------------
# Typed-item lens (Phase 1 / US1): generalized read-only parser
# ---------------------------------------------------------------------------
#
# A *managed item* is a checkbox line that carries a type token. Four types:
#   followup  fu-XXXXXX    (minted 6-hex OR a hand-authored slug like fu-cwd339,
#                           ab-932f5a92; only 6-hex is MINTED, see mint_fu_id)
#   carveout  cv-XXXXXXXX  (8 hex; lifecycle owned by carveouts.jsonl + retro)
#   node      ab-XXXXXXXX  (8 hex; transient - Phase 2 `tidy` ejects filed nodes)
#   human     a #jc tag    (no shortcode; the tag itself IS the type token)
#
# The discriminator is the "- [<mark>]" checkbox-line structure, never the bare
# token: a fu-/ab- mentioned inside a narrative paragraph is not a checkbox line
# and so is never an item (AC2). This is the same false-positive class the
# orphan-target / planning-session detectors hit - anchor on line structure, do
# not grep the token. Both inbox separators are accepted (the legacy em-dash
# and the target hyphen). Phase 2 completed the separator migration: _ITEM_RE
# now accepts BOTH separators and add/promote/dismiss WRITE the hyphen, so the
# fu-only paths (parse_items/list, promote, dismiss, archive_struck, triage)
# stay correct after `tidy` normalizes a line to the hyphen. This phase (1)
# added a read-only lens only.

# The node arm is the liberal, config-agnostic node-id grammar (legacy ab- and
# any configured prefix/width). fu-/cv- arms come FIRST so a sibling token is
# captured by its specific arm; the node arm catches everything else BUT a
# negative lookahead keeps it from stealing a *malformed* sibling (e.g. a 7-hex
# cv-) whose own arm failed - those stay rejected, as before. The lookahead is
# on the exact reserved prefix, so a configured prefix like cvx- is still a node.
_MANAGED_SHORTCODE_RE = re.compile(
    r"^- \[([ x\-])\]\s+"
    r"(" + _FU_TOKEN + r"|cv-[0-9a-f]{8}|(?:(?!fu-|cv-)" + NODE_ID_BODY + r"))"
    r"\s+(?:—|-)\s+(.*?)\s*$"
)
_CHECKBOX_LINE_RE = re.compile(r"^- \[([ x\-])\]\s+(.*?)\s*$")
_JC_TAG_RE = re.compile(r"(?:^|\s)#jc(?:\s|$)")
# Priority anywhere in the line, not only trailing: real ab-* lines carry the
# (pN) mid-line followed by prose (see _split_priority_lenient).
_ANY_PRIORITY_RE = re.compile(r"\((p[0-3])\)")
# Section context: nearest preceding "##"+ heading OR post-merge marker.
_SECTION_HEADING_RE = re.compile(r"^#{2,6}\s+(.*\S)\s*$")
_POST_MERGE_MARKER_RE = re.compile(r"^<!--\s*(post-merge:\S+?)\s*-->\s*$")

_TYPE_BY_PREFIX = {"fu": "followup", "cv": "carveout", "ab": "node"}
_BUCKET_BY_TYPE = {
    "human": "your_actions",
    "followup": "followups",
    "carveout": "carveouts",
    "node": "filed_nodes",
}
_BUCKET_ORDER = ("your_actions", "followups", "carveouts", "filed_nodes")
# These two tables are the load-bearing coupling: every type a shortcode/#jc
# line can yield must have a bucket, or bucket_managed_items would KeyError at
# runtime on a live inbox. Pin the invariant at import time so a future type
# added to one table but not the other fails loudly here, not in production.
assert set(_TYPE_BY_PREFIX.values()) | {"human"} == set(_BUCKET_BY_TYPE), (
    "_TYPE_BY_PREFIX and _BUCKET_BY_TYPE are out of sync"
)


def _token_type(token: str) -> str:
    """Classify a managed shortcode token into its family.

    Keys on the full reserved prefix (``fu-`` / ``cv-``), not the first two
    chars, so a configured node prefix that merely starts with ``fu``/``cv``
    (e.g. ``fux-a3f9``) is still a node. Any non-sibling well-formed token is a
    node - the configured prefix is data, not a fixed string.
    """
    if token.startswith("fu-"):
        return "followup"
    if token.startswith("cv-"):
        return "carveout"
    return "node"


def _split_priority_lenient(rest: str) -> tuple[str, Optional[str]]:
    """Split title/priority tolerating a non-trailing ``(pN)`` followed by prose.

    Real post-merge ``ab-*`` lines carry the priority mid-line with prose after
    it (``**title** (p2). <prose> source: ...``), so the strict trailing-only
    ``_split_priority`` would misread title and priority on the real file. Take
    the FIRST ``(pN)``: the title is everything before it; everything after is
    timeline prose, dropped for the lens. For the common trailing-``(pN)``
    followup/carveout shape the first match IS the trailing one, so this agrees
    with ``_split_priority`` there.
    """
    m = _ANY_PRIORITY_RE.search(rest)
    if not m:
        return rest.strip(), None
    return rest[: m.start()].strip(), m.group(1)


def parse_managed_items(text: str, *, include_struck: bool = False) -> list[dict]:
    """Parse every *managed* checkbox line into a typed item dict (read-only).

    Returns ``[{"type", "id", "title", "priority", "status", "section",
    "line"}]`` where ``type`` is ``followup``/``carveout``/``node``/``human``
    and ``id`` is ``None`` for ``human`` (#jc) lines. Mutates nothing and never
    parses timeline prose: only a ``- [<mark>]`` checkbox line carrying a
    shortcode token or a ``#jc`` tag is an item (AC2). Both the legacy em-dash
    and the target hyphen separators are accepted. By default only ``open``
    (``[ ]``) items are returned; pass ``include_struck=True`` for all marks.

    ``section`` is the nearest preceding ``##`` heading or
    ``<!-- post-merge:pr-N -->`` marker so each item is traceable to its block.
    """
    items: list[dict] = []
    section: Optional[str] = None
    for lineno, line in enumerate(text.splitlines(), start=1):
        marker = _POST_MERGE_MARKER_RE.match(line)
        if marker:
            section = marker.group(1)
            continue
        heading = _SECTION_HEADING_RE.match(line)
        if heading:
            section = heading.group(1)
            continue

        sm = _MANAGED_SHORTCODE_RE.match(line)
        if sm:
            mark, token, rest = sm.group(1), sm.group(2), sm.group(3)
            status = _STATUS_BY_MARK.get(mark, "open")
            if status != "open" and not include_struck:
                continue
            item_type = _token_type(token)
            # fu-/cv- carry their priority as the TRAILING (pN) suffix (add_item
            # mints them that way), so use the strict trailing split to preserve
            # a title that happens to contain a parenthetical like "(p2)". Only
            # ab- filed-node lines (post-merge prose) put (pN) mid-line with
            # prose after it, which needs the lenient first-(pN) split.
            split = _split_priority_lenient if item_type == "node" else _split_priority
            title, priority = split(rest)
            items.append(
                {
                    "type": item_type,
                    "id": token,
                    "title": title,
                    "priority": priority,
                    "status": status,
                    "section": section,
                    "line": lineno,
                }
            )
            continue

        cb = _CHECKBOX_LINE_RE.match(line)
        if cb and _JC_TAG_RE.search(cb.group(2)):
            mark, body = cb.group(1), cb.group(2)
            status = _STATUS_BY_MARK.get(mark, "open")
            if status != "open" and not include_struck:
                continue
            # #jc lines carry no minted shortcode and their priority is the
            # Obsidian-Tasks emoji vocabulary (⏫/🔼/🔽/📅), not a (pN) token, so
            # use the STRICT trailing split here: the lenient mid-line split
            # would mis-read a parenthetical inside the human-readable text
            # (e.g. "talk to (p2) team #jc") as the priority and truncate the
            # title. Only a genuinely trailing (pN) is taken.
            title, priority = _split_priority(body)
            items.append(
                {
                    "type": "human",
                    "id": None,
                    "title": title,
                    "priority": priority,
                    "status": status,
                    "section": section,
                    "line": lineno,
                }
            )
    return items


def bucket_managed_items(items: Iterable[dict]) -> dict[str, list[dict]]:
    """Group managed items into the four labeled buckets, preserving order."""
    buckets: dict[str, list[dict]] = {k: [] for k in _BUCKET_ORDER}
    for item in items:
        buckets[_BUCKET_BY_TYPE[item["type"]]].append(item)
    return buckets


# ---------------------------------------------------------------------------
# Scan
# ---------------------------------------------------------------------------

def scan_transcript(text: str) -> list[dict]:
    """Mechanical regex pre-filter for deferral language.

    Returns ``[{"line": N, "text": "..."}]`` for each matching line. This is
    the deterministic, provider-neutral half; the LLM filters these candidates
    and decides which warrant an ``add``.
    """
    out: list[dict] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        if _SCAN_RE.search(line):
            out.append({"line": lineno, "text": line.strip()})
    return out


# ---------------------------------------------------------------------------
# Inbox file scaffold + locked write
# ---------------------------------------------------------------------------

_SCAFFOLD = """\
---
title: Abilities — backlog inbox
description: Capture tier below idea nodes. Items here are NOT graph nodes; triage promotes warranted items to ab-XXXXXXXX.
created: {date}
updated: {ts}
---

# Abilities — backlog inbox
"""


def _now_date() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%d")


def _now_ts() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M")


def _ensure_scaffold(text: str) -> str:
    """Return ``text`` if it already has content, else a fresh scaffold."""
    if text.strip():
        return text
    return _SCAFFOLD.format(date=_now_date(), ts=_now_ts())


def _has_today_heading(text: str) -> bool:
    today = _now_date()
    return any(
        line.startswith("## ") and today in line for line in text.splitlines()
    )


def _validate_add_fields(*, source: str, why: str, priority: str) -> None:
    src = (source or "").strip()
    if not src:
        raise InboxValidationError("--source must not be empty")
    if _MEMORY_SLUG_RE.match(src):
        raise InboxValidationError(
            "--source must be a substrate reference (PR#, commit, file, URL), "
            "not a memory slug like [[feedback_x]]"
        )
    if not (why or "").strip():
        raise InboxValidationError("--why must not be empty")
    if len(why) > MAX_WHY_LEN:
        raise InboxValidationError(
            f"--why exceeds {MAX_WHY_LEN} chars (got {len(why)}); shorten it"
        )
    if priority not in VALID_PRIORITIES:
        raise InboxValidationError(
            f"--priority must be one of {sorted(VALID_PRIORITIES)}, got {priority!r}"
        )


def _find_duplicate_open_fu(
    text: str, *, title: str, where: Optional[str]
) -> Optional[dict]:
    """Return the EXISTING open ``fu-`` item that duplicates (title, where) at
    capture time as ``{"id", "title", "priority", "where"}``, or ``None``
    (Phase 3 / US3 / AC4).

    Uses the SAME exact key as tidy's ``_cluster_dedup`` -
    ``(_norm(where), _norm(title))`` - so the capture-time pre-check and the
    report-only tidy dedup agree on what "duplicate" means: a conservative exact
    match (false-split over false-merge). Only OPEN ``[ ]`` followups are
    candidates - a promoted/dismissed line, or a cv-/ab- line, never blocks a
    fresh capture. Returns the EXISTING item's fields (not the incoming call's)
    so the caller's return value and ``capture_add`` telemetry describe the item
    that already tracks the work, never a phantom. Mirrors the block-walk in
    ``_process_body``/``archive_struck`` to read each item's ``where:`` sub-line.
    """
    target = (_norm(where or ""), _norm(title))
    lines = text.splitlines()
    i, n = 0, len(lines)
    while i < n:
        m = _MANAGED_SHORTCODE_RE.match(lines[i])
        if not (m and m.group(1) == " " and m.group(2).startswith("fu-")):
            i += 1
            continue
        token, rest = m.group(2), m.group(3)
        block = [lines[i]]
        j = i + 1
        while j < n and lines[j].startswith("  ") and lines[j].strip():
            block.append(lines[j])
            j += 1
        existing_title, existing_priority = _split_priority(rest)
        existing_where = _extract_where(block)
        if (_norm(existing_where or ""), _norm(existing_title)) == target:
            return {
                "id": token,
                "title": existing_title,
                "priority": existing_priority,
                "where": existing_where,
            }
        i = j
    return None


def add_item(
    path: Path,
    *,
    title: str,
    source: str,
    why: str,
    where: Optional[str] = None,
    priority: str = "p2",
    lock_retries: int = 50,
) -> dict:
    """Validate, lock, append a shaped item line + sub-lines. Returns the item.

    Creates the inbox file with a frontmatter scaffold on first use. The write
    is serialized by an exclusive flock on ``{path}.lock`` so concurrent adds
    (including sibling worktrees sharing the symlinked target) never lose an
    append.
    """
    _validate_add_fields(source=source, why=why, priority=priority)
    title = title.strip()
    if not title:
        raise InboxValidationError("title must not be empty")

    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = path.with_name(path.name + ".lock")

    fd = _acquire_lock(lock_path, lock_retries)
    try:
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        text = _ensure_scaffold(text)

        # Dedup pre-check (inside the lock so it is race-free with concurrent
        # adds): if an open followup already covers this (title, where), return
        # ITS fields and skip minting + the file write. Honors priority-2's
        # "check if something already exists" at capture time (AC4). id / title /
        # priority / where describe the EXISTING item (so output + inbox_add
        # telemetry never mislabel it); source / why record the deduped attempt.
        dup = _find_duplicate_open_fu(text, title=title, where=where)
        if dup is not None:
            return {
                "id": dup["id"],
                "title": dup["title"],
                "priority": dup["priority"] or priority,
                "source": source.strip(),
                "why": why.strip(),
                "where": dup["where"],
                "status": "open",
                "deduped": True,
            }

        fu_id = mint_fu_id(_existing_ids(text))

        block_lines = []
        if not _has_today_heading(text):
            heading = f"\n## {_now_date()}\n"
            block_lines.append(heading)
        block_lines.append(f"- [ ] {fu_id} - {title} ({priority})")
        block_lines.append(f"  source: {source.strip()}")
        block_lines.append(f"  why: {why.strip()}")
        if where and where.strip():
            block_lines.append(f"  where: {where.strip()}")

        if not text.endswith("\n"):
            text += "\n"
        text += "\n".join(block_lines) + "\n"
        _atomic_write(path, text)
    finally:
        _release_lock(fd)

    return {
        "id": fu_id,
        "title": title,
        "priority": priority,
        "source": source.strip(),
        "why": why.strip(),
        "where": (where or "").strip() or None,
        "status": "open",
        "deduped": False,
    }


def _acquire_lock(lock_path: Path, retries: int) -> int:
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR, 0o666)
    # flock(LOCK_EX) blocks until acquired; retries are a defensive bound in
    # case a non-blocking variant is ever swapped in.
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
    except OSError as exc:  # pragma: no cover - blocking flock rarely raises
        os.close(fd)
        raise InboxLockError(f"could not lock {lock_path}: {exc}") from exc
    return fd


def _release_lock(fd: int) -> None:
    try:
        fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)


def _atomic_write(path: Path, content: str) -> None:
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    tmp.write_text(content, encoding="utf-8")
    os.replace(tmp, path)


# ---------------------------------------------------------------------------
# Line rewrite (promote / dismiss) + node creation
# ---------------------------------------------------------------------------

_NODE_ID_RE = re.compile(r"->\s*(" + NODE_ID_BODY + r")")


def _find_item(text: str, fu_id: str) -> Optional[re.Match[str]]:
    for line in text.splitlines():
        m = _ITEM_RE.match(line)
        if m and m.group(2) == fu_id:
            return m
    return None


def _replace_item_line(text: str, fu_id: str, new_line: str) -> str:
    """Pure: return ``text`` with the line whose item id is ``fu_id`` replaced by
    ``new_line``. Raises ``InboxValidationError`` if no line matches.

    The caller is responsible for holding the inbox lock around the read that
    produced ``text`` and the write of the returned text, so the check-then-write
    is atomic (no TOCTOU)."""
    lines = text.splitlines()
    found = False
    for idx, line in enumerate(lines):
        m = _ITEM_RE.match(line)
        if m and m.group(2) == fu_id:
            lines[idx] = new_line
            found = True
            break
    if not found:
        raise InboxValidationError(f"unknown fu-id: {fu_id}")
    new_text = "\n".join(lines)
    if text.endswith("\n"):
        new_text += "\n"
    return new_text


def _create_graph_node(*, title: str, priority: str, domain: str = "code", graph_path: Optional[Path] = None) -> str:
    """Create a plan-less idea node on the graph; return its ab-id.

    Reuses the canonical node-build path so a schema addition shows up here
    too. Imported lazily to avoid the import cycle (graph.cli imports this
    module at load to register the subapp)."""
    from fno.graph._constants import mint_node_id
    from fno.graph._intake import detect_project_from_settings
    from fno.graph.cli import _build_backlog_node, _graph_path
    from fno.graph.store import locked_mutate_graph

    gpath = graph_path or _graph_path()
    resolved_cwd = os.getcwd()
    project = detect_project_from_settings(resolved_cwd)

    holder: list[str] = []

    def mutator(entries):
        new_id = mint_node_id({e.get("id") for e in entries})
        holder.append(new_id)
        node = _build_backlog_node(
            title=title,
            priority=priority,
            domain=domain,
            project=project,
            cwd=resolved_cwd,
            known_ids={e.get("id") for e in entries},
        )
        node["id"] = new_id
        entries.append(node)
        return entries

    locked_mutate_graph(gpath, mutator)
    return holder[0]


def promote_item(
    path: Path,
    fu_id: str,
    *,
    priority: Optional[str] = None,
    graph_path: Optional[Path] = None,
) -> dict:
    """Promote an inbox item to a graph node and strike its checkbox.

    Idempotent: re-promoting an already-promoted item returns the existing
    node id without creating a duplicate. The node is created BEFORE the
    strike so a failed strike surfaces loudly (a node with an un-struck line)
    rather than silently diverging (AC4-FR).

    The whole critical section (read, idempotency check, node creation, strike)
    runs under the inbox lock so two concurrent promotes of the same id cannot
    both pass the check and mint duplicate nodes.
    """
    if priority is not None and priority not in VALID_PRIORITIES:
        raise InboxValidationError(
            f"--priority must be one of {sorted(VALID_PRIORITIES)}, got {priority!r}"
        )
    path = Path(path)
    lock_path = path.with_name(path.name + ".lock")
    fd = _acquire_lock(lock_path, 50)
    try:
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        m = _find_item(text, fu_id)
        if m is None:
            raise InboxValidationError(f"unknown fu-id: {fu_id}")

        mark = m.group(1)
        if mark == "x":
            node_match = _NODE_ID_RE.search(m.group(0))
            return {
                "id": fu_id,
                "node_id": node_match.group(1) if node_match else None,
                "status": "already_promoted",
            }
        if mark == "-":
            raise InboxValidationError(f"{fu_id} was dismissed; cannot promote")

        title, parsed_priority = _split_priority(m.group(3))
        node_priority = priority or parsed_priority or "p2"
        # Node creation acquires the graph lock; we hold the inbox lock. The
        # ordering is always inbox -> graph (add/promote/dismiss never take the
        # graph lock first), so there is no deadlock.
        node_id = _create_graph_node(title=title, priority=node_priority, graph_path=graph_path)

        new_line = f"- [x] {fu_id} - {m.group(3)} -> {node_id}"
        _atomic_write(path, _replace_item_line(text, fu_id, new_line))
        return {"id": fu_id, "node_id": node_id, "status": "promoted"}
    finally:
        _release_lock(fd)


def dismiss_item(path: Path, fu_id: str, reason: str) -> dict:
    """Strike an item as dismissed (``[-]``), preserving the line."""
    if not (reason or "").strip():
        raise InboxValidationError("dismiss requires a non-empty reason")
    reason = reason.strip()
    path = Path(path)
    lock_path = path.with_name(path.name + ".lock")
    fd = _acquire_lock(lock_path, 50)
    try:
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        m = _find_item(text, fu_id)
        if m is None:
            raise InboxValidationError(f"unknown fu-id: {fu_id}")
        if m.group(1) == "-":
            return {"id": fu_id, "status": "already_dismissed"}
        if m.group(1) == "x":
            raise InboxValidationError(f"{fu_id} was promoted; cannot dismiss")

        new_line = f"- [-] {fu_id} - {m.group(3)} (dismissed: {reason})"
        _atomic_write(path, _replace_item_line(text, fu_id, new_line))
        return {"id": fu_id, "status": "dismissed", "reason": reason}
    finally:
        _release_lock(fd)


def archive_struck(path: Path, archive_path: Optional[Path] = None) -> dict:
    """Sweep struck items (promoted/dismissed) into a sibling archive file.

    Open items stay in place. Each item's sub-lines (source/why/where) move
    with it. Returns ``{"archived": N, "archive_path": ...}``.
    """
    path = Path(path)
    if archive_path is None:
        archive_path = path.with_name("inbox-archive.md")
    archive_path = Path(archive_path)

    lock_path = path.with_name(path.name + ".lock")
    fd = _acquire_lock(lock_path, 50)
    try:
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        lines = text.splitlines()
        kept: list[str] = []
        moved: list[str] = []
        archived = 0

        i = 0
        while i < len(lines):
            line = lines[i]
            m = _ITEM_RE.match(line)
            if not m:
                kept.append(line)
                i += 1
                continue
            # Collect the item block: the item line + immediately-following
            # indented sub-lines (stop at a blank line, heading, or next item).
            block = [line]
            j = i + 1
            while j < len(lines) and lines[j].startswith("  ") and lines[j].strip():
                block.append(lines[j])
                j += 1
            if m.group(1) in ("x", "-"):
                moved.extend(block)
                archived += 1
            else:
                kept.extend(block)
            i = j

        if archived == 0:
            return {"archived": 0, "archive_path": str(archive_path)}

        new_text = "\n".join(kept)
        if new_text and not new_text.endswith("\n"):
            new_text += "\n"
        _atomic_write(path, new_text)

        existing_archive = (
            archive_path.read_text(encoding="utf-8") if archive_path.exists() else ""
        )
        if not existing_archive.strip():
            existing_archive = (
                "---\n"
                "title: Abilities — backlog inbox archive\n"
                f"created: {_now_date()}\n"
                "---\n\n# Abilities — backlog inbox archive\n"
            )
        if not existing_archive.endswith("\n"):
            existing_archive += "\n"
        existing_archive += f"\n## archived {_now_ts()}\n" + "\n".join(moved) + "\n"
        archive_path.parent.mkdir(parents=True, exist_ok=True)
        _atomic_write(archive_path, existing_archive)

        return {"archived": archived, "archive_path": str(archive_path)}
    finally:
        _release_lock(fd)


# ---------------------------------------------------------------------------
# Tidy (Phase 2 / US2): one idempotent pass
# ---------------------------------------------------------------------------
#
# `tidy` does four things in a single locked read-modify-write:
#   1. EJECT filed nodes whose graph node completed (completed_at /
#      superseded_by read directly, NOT status which read_graph does not
#      recompute) - both raw `ab-*` lines and promoted `[x] fu- -> ab-` lines.
#   2. DEDUP open items by (where + title), report-only (no line mutated).
#   3. NORMALIZE the item separator em-dash -> hyphen on every managed line it
#      keeps (only the token separator; em-dashes inside prose survive).
#   4. DIGEST: rebuild a pinned block between regenerable markers listing open
#      `#jc` actions (deduped; dated ascending then undated in source order) and
#      open followups grouped by priority.
#
# Idempotent: the digest block is stripped before parsing (so it never feeds
# back into itself), the timeline's blank edges are trimmed every run, and the
# separator rewrite is a no-op once already hyphenated - so a second run with no
# intervening edit reproduces the first run's output byte-for-byte (AC5).

DIGEST_START = "<!-- inbox-digest:start -->"
DIGEST_END = "<!-- inbox-digest:end -->"
_H1_RE = re.compile(r"^# \S")
_JC_DATE_RE = re.compile(r"\U0001F4C5\s*(\d{4}-\d{2}-\d{2})")  # 📅 YYYY-MM-DD
_WHERE_SUBLINE_RE = re.compile(r"^\s+where:\s*(.*\S)\s*$")


def _norm(s: str) -> str:
    """Whitespace-collapsed, lower-cased normalization for dedup/digest keys."""
    return re.sub(r"\s+", " ", (s or "").strip()).lower()


def _completed_node_ids(
    graph_path: Optional[Path], *, include_deferred: bool = False
) -> tuple[set[str], Optional[str]]:
    """Return ``(complete_ids, warning)`` from graph.json.

    A node is 'complete' when ``completed_at`` OR ``superseded_by`` is set
    (read directly, since ``read_graph`` does not recompute ``status``).
    ``deferred_at`` counts only with ``include_deferred``. Fail-safe: a missing
    or unreadable graph returns an EMPTY id set plus a warning, so ``tidy``
    ejects nothing rather than archiving a live item on a read miss.
    """
    if graph_path is None:
        return set(), None
    graph_path = Path(graph_path)
    if not graph_path.exists():
        return set(), f"graph.json not found at {graph_path}; skipping eject"
    try:
        from fno.graph.store import _apply_graph_defaults, _read_json

        # Use the raw read (not read_graph, which swallows GraphCorruptError to
        # []): a corrupt graph must surface here as a warning on the manual tidy
        # path, not silently skip eject. _read_json raises GraphCorruptError on a
        # malformed file; the except below turns it into the fail-safe warning.
        nodes = _apply_graph_defaults(_read_json(graph_path))
    except Exception as exc:  # noqa: BLE001 - any read error must fail safe
        return set(), f"graph.json unreadable ({exc}); skipping eject"

    done: set[str] = set()
    for node in nodes:
        nid = node.get("id")
        if not nid:
            continue
        if node.get("completed_at") or node.get("superseded_by"):
            done.add(nid)
        elif include_deferred and node.get("deferred_at"):
            done.add(nid)
    return done, None


def _strip_digest_block(lines: list[str]) -> list[str]:
    """Remove an existing digest block (markers inclusive).

    Refuses on marker drift: more than one start/end or an end before its start
    raises rather than guessing (a hand-edited/duplicated block must not let
    ``tidy`` append a third digest or clobber prose). Zero markers is fine - the
    caller inserts one fresh.
    """
    starts = [i for i, ln in enumerate(lines) if ln.strip() == DIGEST_START]
    ends = [i for i, ln in enumerate(lines) if ln.strip() == DIGEST_END]
    if not starts and not ends:
        return lines
    if len(starts) != 1 or len(ends) != 1 or ends[0] < starts[0]:
        raise InboxValidationError(
            "inbox digest marker drift: expected exactly one "
            f"{DIGEST_START} / {DIGEST_END} pair (found {len(starts)} start, "
            f"{len(ends)} end); refusing to guess. Fix the markers by hand."
        )
    return lines[: starts[0]] + lines[ends[0] + 1 :]


def _split_head_body(lines: list[str]) -> tuple[list[str], list[str]]:
    """Split into (head, body). Head = frontmatter + the first H1 heading line;
    body = everything after the H1. The digest is pinned at the top of body."""
    idx = 0
    if lines and lines[0].strip() == "---":
        for j in range(1, len(lines)):
            if lines[j].strip() == "---":
                idx = j + 1
                break
    k = idx
    while k < len(lines) and not lines[k].strip():
        k += 1
    if k < len(lines) and _H1_RE.match(lines[k]):
        return lines[: k + 1], lines[k + 1 :]
    return lines[:idx], lines[idx:]


def _rstrip_blank(lines: list[str]) -> list[str]:
    end = len(lines)
    while end > 0 and not lines[end - 1].strip():
        end -= 1
    return lines[:end]


def _trim_blank_edges(lines: list[str]) -> list[str]:
    start, end = 0, len(lines)
    while start < end and not lines[start].strip():
        start += 1
    while end > start and not lines[end - 1].strip():
        end -= 1
    return lines[start:end]


def _extract_where(block: list[str]) -> Optional[str]:
    for sub in block[1:]:
        wm = _WHERE_SUBLINE_RE.match(sub)
        if wm:
            return wm.group(1)
    return None


def _process_body(
    body: list[str], complete_ids: set[str]
) -> tuple[list[str], list[str], int, list[list[str]]]:
    """Walk the timeline body. Eject completed filed nodes (block-aware),
    normalize the separator on kept managed lines, and collect open items for
    the report-only dedup. Returns (kept, moved, ejected, dedup_clusters)."""
    kept: list[str] = []
    moved: list[str] = []
    ejected = 0
    open_items: list[tuple[str, str, Optional[str]]] = []  # (token, title, where)

    i, n = 0, len(body)
    while i < n:
        line = body[i]
        m = _MANAGED_SHORTCODE_RE.match(line)
        if not m:
            kept.append(line)
            i += 1
            continue

        mark, token, rest = m.group(1), m.group(2), m.group(3)
        # Collect the item block: item line + immediately-following indented
        # sub-lines (stop at blank, heading, or next item) - mirrors
        # archive_struck so a node's prose/sub-lines move with it.
        block = [line]
        j = i + 1
        while j < n and body[j].startswith("  ") and body[j].strip():
            block.append(body[j])
            j += 1

        node_id: Optional[str] = None
        if _token_type(token) == "node":
            node_id = token
        else:
            nm = _NODE_ID_RE.search(line)  # promoted "-> <node-id>"
            if nm:
                node_id = nm.group(1)

        if node_id and node_id in complete_ids:
            moved.extend(block)
            ejected += 1
            i = j
            continue

        # Keep: normalize the token separator to ' - ' (only here, never in the
        # title prose). Re-emitting in canonical form is a no-op once hyphenated.
        block[0] = f"- [{mark}] {token} - {rest}"
        kept.extend(block)
        if mark == " ":
            split = _split_priority_lenient if _token_type(token) == "node" else _split_priority
            title, _prio = split(rest)
            open_items.append((token, title, _extract_where(block)))
        i = j

    return kept, moved, ejected, _cluster_dedup(open_items)


def _cluster_dedup(items: list[tuple[str, str, Optional[str]]]) -> list[list[str]]:
    """Group open items by exact (normalized where + normalized title). Report
    clusters of >=2 only. Report-only in v1 (no auto-merge) so a capture is
    never silently lost; threshold is conservative (exact match)."""
    groups: dict[tuple[str, str], list[str]] = {}
    order: list[tuple[str, str]] = []
    for token, title, where in items:
        key = (_norm(where or ""), _norm(title))
        if key not in groups:
            groups[key] = []
            order.append(key)
        groups[key].append(token)
    return [groups[key] for key in order if len(groups[key]) >= 2]


def _build_digest(open_items: list[dict]) -> list[str]:
    """Build the pinned digest block (markers inclusive) from open items.

    `#jc` actions: deduped by normalized text, dated ascending by 📅 then
    undated in stable source order. Followups: count + ids per priority tier.
    """
    jc = [it for it in open_items if it["type"] == "human"]
    fus = [it for it in open_items if it["type"] == "followup"]

    annotated = [
        (_JC_DATE_RE.search(it["title"]), idx, it["title"])
        for idx, it in enumerate(jc)
    ]
    dated = sorted(
        (a for a in annotated if a[0] is not None),
        key=lambda a: (a[0].group(1) if a[0] is not None else "", a[1]),
    )
    undated = [a for a in annotated if a[0] is None]  # stable source order
    jc_lines: list[str] = []
    seen: set[str] = set()
    for _date, _idx, title in [*dated, *undated]:
        key = _norm(title)
        if key in seen:
            continue
        seen.add(key)
        jc_lines.append(f"- {title}")

    fu_lines: list[str] = []
    for tier in ("p0", "p1", "p2", "p3"):
        ids = [it["id"] for it in fus if it["priority"] == tier]
        suffix = f" ({', '.join(ids)})" if ids else ""
        fu_lines.append(f"- {tier}: {len(ids)}{suffix}")
    unprioritized = [it["id"] for it in fus if it["priority"] not in VALID_PRIORITIES]
    if unprioritized:
        fu_lines.append(
            f"- (unprioritized): {len(unprioritized)} ({', '.join(unprioritized)})"
        )

    block = [DIGEST_START, "## Open #jc actions"]
    block += jc_lines or ["_(none)_"]
    block += ["", "## Open followups by priority"]
    block += fu_lines
    block += [DIGEST_END]
    return block


def _assemble(head: list[str], digest: list[str], timeline: list[str]) -> str:
    head = _rstrip_blank(head)
    timeline = _trim_blank_edges(timeline)
    parts: list[str] = []
    if head:
        parts += head + [""]
    parts += digest
    if timeline:
        parts += [""] + timeline
    return "\n".join(parts).rstrip("\n") + "\n"


def tidy(
    path: Path,
    *,
    archive_path: Optional[Path] = None,
    graph_path: Optional[Path] = None,
    include_deferred: bool = False,
    lock_retries: int = 50,
) -> dict:
    """Run one idempotent tidy pass over the inbox. Returns a summary dict.

    Eject (completed filed nodes) -> archive; dedup (report-only); separator
    normalize; rebuild the pinned digest. Serialized by the same ``flock`` the
    other write verbs use so a concurrent ``add``/``promote`` append never
    races the read-modify-write.
    """
    path = Path(path)
    if archive_path is None:
        archive_path = path.with_name("inbox-archive.md")
    archive_path = Path(archive_path)

    # Resolve completion outside the lock (read_graph takes the graph lock, not
    # the inbox lock; ordering inbox->graph matches promote, so no deadlock).
    complete_ids, graph_warning = _completed_node_ids(
        graph_path, include_deferred=include_deferred
    )

    lock_path = path.with_name(path.name + ".lock")
    fd = _acquire_lock(lock_path, lock_retries)
    try:
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        text = _ensure_scaffold(text)
        lines = _strip_digest_block(text.splitlines())
        head, body = _split_head_body(lines)

        kept, moved, ejected, dedup_clusters = _process_body(body, complete_ids)

        open_items = parse_managed_items("\n".join(kept), include_struck=False)
        digest = _build_digest(open_items)

        # Durability ordering: append the ejected blocks to the archive BEFORE
        # truncating the inbox. A crash (or a failing archive write) between the
        # two then leaves a recoverable DUPLICATE - the line is still in the
        # inbox AND now in the archive, and the next idempotent tidy re-ejects it
        # - rather than a HOLE where the line is gone from both files. This is
        # the create-before-strike discipline promote_item already documents.
        if moved:
            _archive_append(archive_path, moved)
        _atomic_write(path, _assemble(head, digest, kept))
    finally:
        _release_lock(fd)

    return {
        "ejected": ejected,
        "archive_path": str(archive_path),
        "dedup_clusters": dedup_clusters,
        "jc_actions": sum(1 for it in open_items if it["type"] == "human"),
        "followups_open": sum(1 for it in open_items if it["type"] == "followup"),
        "graph_warning": graph_warning,
    }


def _archive_append(archive_path: Path, moved_lines: list[str]) -> None:
    """Append ejected blocks under a timestamped heading in the sibling archive
    (same format as ``archive_struck``)."""
    existing = (
        archive_path.read_text(encoding="utf-8") if archive_path.exists() else ""
    )
    if not existing.strip():
        existing = (
            "---\n"
            "title: Abilities — backlog inbox archive\n"
            f"created: {_now_date()}\n"
            "---\n\n# Abilities — backlog inbox archive\n"
        )
    if not existing.endswith("\n"):
        existing += "\n"
    existing += f"\n## archived {_now_ts()}\n" + "\n".join(moved_lines) + "\n"
    archive_path.parent.mkdir(parents=True, exist_ok=True)
    _atomic_write(archive_path, existing)


# ---------------------------------------------------------------------------
# Empty-pass artifact
# ---------------------------------------------------------------------------

def write_empty_pass_artifact(
    *,
    session_id: str,
    reason: str,
    scan_candidates: int = 0,
    artifacts_dir: Path,
) -> Path:
    """Write the deferrals gate artifact for an honest empty pass.

    Frontmatter carries ``phase: deferrals`` + ``session_id`` (the stop-hook
    factor-2 check) plus ``entries_written: 0`` and ``scan_candidates: N``
    (the anti-rubber-stamp audit trail).
    """
    artifacts_dir = Path(artifacts_dir)
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    artifact = artifacts_dir / f"deferrals-{session_id}.md"
    content = (
        "---\n"
        "phase: deferrals\n"
        f"session_id: {session_id}\n"
        "entries_written: 0\n"
        f"scan_candidates: {scan_candidates}\n"
        f"completed_at: {datetime.now(timezone.utc).isoformat()}\n"
        "---\n"
        f"Empty deferrals pass: {reason}\n"
    )
    artifact.write_text(content, encoding="utf-8")
    return artifact


def write_capture_artifact(
    *,
    session_id: str,
    entries_written: int,
    artifacts_dir: Path,
) -> Path:
    """Write the deferrals gate artifact for the capture path (>=1 item).

    Symmetric with write_empty_pass_artifact but records the real capture
    count. No `reason` is needed: captured items are self-justifying.
    """
    artifacts_dir = Path(artifacts_dir)
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    artifact = artifacts_dir / f"deferrals-{session_id}.md"
    content = (
        "---\n"
        "phase: deferrals\n"
        f"session_id: {session_id}\n"
        f"entries_written: {entries_written}\n"
        f"completed_at: {datetime.now(timezone.utc).isoformat()}\n"
        "---\n"
        f"Captured {entries_written} deferral item(s) into the backlog capture tier.\n"
    )
    artifact.write_text(content, encoding="utf-8")
    return artifact


def _count_session_events(
    event_types: tuple[str, ...], session_id: str, events_path: Path
) -> int:
    """Count events whose type is in ``event_types`` for ``session_id``.

    Takes a tuple so readers can dual-accept the ``capture_*`` vocabulary and
    the legacy ``inbox_*`` one (events.jsonl is append-only history; rows
    written by a pre-rename binary keep their old type forever).
    """
    events_path = Path(events_path)
    if not events_path.exists():
        return 0
    n = 0
    # Iterate line-by-line rather than read_text().splitlines() so the whole
    # events log is never held in memory (it grows unbounded over a project's life).
    with events_path.open(encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                evt = json.loads(line)
            except json.JSONDecodeError:
                continue
            if evt.get("type") in event_types and (evt.get("data") or {}).get("session_id") == session_id:
                n += 1
    return n


def count_capture_adds(session_id: str, events_path: Path) -> int:
    """Count capture_add (and legacy inbox_add) events for ``session_id``."""
    return _count_session_events(("capture_add", "inbox_add"), session_id, events_path)


# ---------------------------------------------------------------------------
# Session + event helpers
# ---------------------------------------------------------------------------

def _repo_root() -> Path:
    from fno.paths import resolve_repo_root
    try:
        return resolve_repo_root()
    except Exception:
        return Path.cwd()


def _events_path() -> Path:
    return _repo_root() / ".fno" / "events.jsonl"


def _detect_session_id() -> str:
    """Read session_id from target-state.md; fall back to 'manual'."""
    state_path = _repo_root() / ".fno" / "target-state.md"
    if not state_path.exists():
        return "manual"
    try:
        from fno.state.io import read_frontmatter
        fm, _ = read_frontmatter(state_path)
        sid = fm.get("fno_id") or fm.get("session_id")
        return str(sid) if sid else "manual"
    except Exception:
        return "manual"


def _emit(event_type: str, data: dict) -> bool:
    """Best-effort event emit. Returns ``True`` if the event was appended.

    Failure prints a warning and returns ``False`` rather than aborting. For
    ``add``/``dismiss``/``promote`` a missing event is telemetry loss, not data
    loss (the markdown write already succeeded), so callers ignore the return.
    For ``empty-pass`` the event IS the gate's third honesty factor, so that
    caller checks the return and fails loudly (see ``cmd_empty_pass``)."""
    try:
        from fno.events import _build, append_event
        event = _build(event_type, "backlog", data)
        append_event(event, events_path=_events_path())
        return True
    except Exception as exc:  # noqa: BLE001
        typer.echo(f"warning: failed to emit {event_type} event: {exc}", err=True)
        return False


# ---------------------------------------------------------------------------
# Typer CLI
# ---------------------------------------------------------------------------

cli = typer.Typer(
    name="capture",
    help="Backlog capture tier (fu-* items below idea nodes).",
    no_args_is_help=True,
)


def _inbox_path() -> Path:
    from fno.paths import inbox_path
    return inbox_path()


def _graph_path_for_tidy() -> Path:
    """Resolve the canonical graph.json for tidy's completion lookup.

    Lazy import avoids the graph.cli <-> inbox import cycle (graph.cli imports
    this module at load to register the subapp), mirroring _create_graph_node.
    """
    from fno.graph.cli import _graph_path
    return _graph_path()


@cli.command("add")
def cmd_add(
    title: str = typer.Argument(..., help="Short item title."),
    source: str = typer.Option(..., "--source", "-s", help="Substrate ref (PR#, commit, file, URL). NOT a memory slug."),
    why: str = typer.Option(..., "--why", help="One-line rationale (<=120 chars)."),
    where: Optional[str] = typer.Option(None, "--where", "-w", help="Where the work lands (file/area)."),
    priority: str = typer.Option("p2", "--priority", "-p", help="p0|p1|p2|p3."),
) -> None:
    """Capture an item into the backlog capture tier.

    Dedup pre-check: if an open item already covers this (title + where), its id
    is returned and no new fu- is minted (the JSON carries ``"deduped": true``).
    """
    try:
        item = add_item(
            _inbox_path(),
            title=title,
            source=source,
            why=why,
            where=where,
            priority=priority,
        )
    except InboxValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2)
    except InboxLockError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    # Emit capture_add whether or not the item deduped: the capture pass ran and
    # the work is tracked, so the deferrals_captured gate's third factor stays
    # satisfied. The fu_id is the existing item's id on a dedup hit. (The event
    # schema is unchanged; the deduped flag rides only in the JSON output.)
    _emit(
        "capture_add",
        {
            "session_id": _detect_session_id(),
            "fu_id": item["id"],
            "title": item["title"],
            "priority": item["priority"],
        },
    )
    if item.get("deduped"):
        typer.echo(
            f"deduped: open item {item['id']} already covers this; no new fu- minted",
            err=True,
        )
    typer.echo(json.dumps(item))


_BUCKET_LABELS = {
    "your_actions": "Your actions (#jc)",
    "followups": "Followups (fu-*)",
    "carveouts": "Carveouts (cv-*)",
    "filed_nodes": "Filed nodes (ab-*)",
}


def _render_by_type(text: str, *, as_json: bool, include_struck: bool) -> None:
    """Render the four-bucket typed lens (the body of ``list --by-type``).

    Read-only: groups every managed line into Your-actions / Followups /
    Carveouts / Filed-nodes with each item's priority and source section. With
    ``--json`` it emits the four-key bucket object (empty inbox => four empty
    lists). It does NOT run the fu-only unparseable / soft-ceiling warnings the
    plain ``list`` emits: those grep the bare fu- token and would mis-flag a
    token quoted in timeline prose, which is exactly what this lens avoids.
    """
    buckets = bucket_managed_items(
        parse_managed_items(text, include_struck=include_struck)
    )
    if as_json:
        typer.echo(json.dumps(buckets))
        return
    total = sum(len(v) for v in buckets.values())
    if total == 0:
        typer.echo("(no managed items)", err=True)
        return
    for key in _BUCKET_ORDER:
        bucket = buckets[key]
        typer.echo(f"{_BUCKET_LABELS[key]}: {len(bucket)}")
        for it in bucket:
            id_part = f"{it['id']}  " if it["id"] else ""
            prio = f" ({it['priority']})" if it["priority"] else ""
            status = "" if it["status"] == "open" else f" [{it['status']}]"
            section = f"  <{it['section']}>" if it["section"] else ""
            typer.echo(f"  {id_part}{it['title']}{prio}{status}{section}")


@cli.command("list")
def cmd_list(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit a JSON array to stdout."),
    include_struck: bool = typer.Option(False, "--all", "-A", help="Include promoted/dismissed items."),
    by_type: bool = typer.Option(
        False,
        "--by-type",
        help="Group ALL managed types (followups/carveouts/filed-nodes/#jc) "
        "into four labeled buckets. Read-only identify lens.",
    ),
) -> None:
    """List inbox items (open by default)."""
    path = _inbox_path()
    try:
        text = path.read_text(encoding="utf-8") if path.exists() else ""
    except (OSError, UnicodeDecodeError) as exc:
        # A present-but-unreadable inbox (permission denied / non-UTF-8) must exit
        # cleanly, not dump a raw traceback (ab-0625107e).
        typer.echo(f"error: cannot read inbox {path}: {exc}", err=True)
        raise typer.Exit(code=1)
    if by_type:
        _render_by_type(text, as_json=as_json, include_struck=include_struck)
        return
    items = parse_items(text, include_struck=include_struck)

    for lineno, raw in find_unparseable_fu_lines(text):
        typer.echo(
            f"warning: line {lineno} has a fu- id but is not a parseable item "
            f"(check the '—' separator / checkbox); it is hidden from listings: {raw}",
            err=True,
        )

    open_count = sum(1 for i in items if i["status"] == "open")
    if open_count > SOFT_CEILING:
        typer.echo(
            f"warning: {open_count} open inbox items (soft ceiling {SOFT_CEILING}); "
            f"run `fno backlog capture archive` or triage some out.",
            err=True,
        )

    if as_json:
        typer.echo(json.dumps(items))
        return
    if not items:
        typer.echo("(inbox empty)", err=True)
        return
    for i in items:
        prio = f" ({i['priority']})" if i["priority"] else ""
        typer.echo(f"{i['id']}  [{i['status']}]  {i['title']}{prio}")


@cli.command("scan")
def cmd_scan(
    transcript: Optional[Path] = typer.Argument(
        None, help="Transcript file to scan; reads stdin when omitted."
    ),
) -> None:
    """Mechanical regex pre-filter for deferral language. Emits JSON candidates."""
    if transcript is not None:
        if not transcript.exists():
            typer.echo(f"error: transcript not found: {transcript}", err=True)
            raise typer.Exit(code=2)
        text = transcript.read_text(encoding="utf-8")
    else:
        text = sys.stdin.read()

    candidates = scan_transcript(text)
    _emit("capture_scan", {"session_id": _detect_session_id(), "candidates": len(candidates)})
    typer.echo(json.dumps(candidates))


@cli.command("empty-pass")
def cmd_empty_pass(
    reason: str = typer.Option(..., "--reason", "-R", help="Why nothing was captured (mandatory)."),
    session_id: Optional[str] = typer.Option(None, "--session-id", help="Defaults to target-state session_id."),
    scan_candidates: int = typer.Option(0, "--scan-candidates", help="Count from a prior scan (auditable)."),
) -> None:
    """Declare an honest empty deferrals pass (no captures this session)."""
    if not (reason or "").strip():
        typer.echo("error: --reason must not be empty (anti-rubber-stamp rule)", err=True)
        raise typer.Exit(code=2)

    sid = session_id or _detect_session_id()
    artifacts_dir = _repo_root() / ".fno" / "artifacts"
    artifact = write_empty_pass_artifact(
        session_id=sid,
        reason=reason,
        scan_candidates=scan_candidates,
        artifacts_dir=artifacts_dir,
    )
    emitted = _emit(
        "capture_empty_pass",
        {"session_id": sid, "reason": reason, "scan_candidates": scan_candidates},
    )
    # The capture_empty_pass event is the gate's third honesty factor. Some inline
    # emit paths return rc=0 without writing (feedback_emit_gate_transition_silent_failure),
    # so read back rather than trust the emit. A missing event means the gate
    # would later be rejected far from here, so fail loudly now.
    if not emitted or _count_session_events(
        ("capture_empty_pass", "inbox_empty_pass"), sid, _events_path()
    ) == 0:
        typer.echo(
            "error: capture_empty_pass event did not land; the deferrals gate would "
            "be rejected later. Check .fno/events.jsonl and re-run.",
            err=True,
        )
        raise typer.Exit(code=1)
    typer.echo(json.dumps({"session_id": sid, "artifact": str(artifact), "entries_written": 0}))


@cli.command("capture-pass")
def cmd_capture_pass(
    session_id: Optional[str] = typer.Option(None, "--session-id", help="Defaults to target-state session_id."),
) -> None:
    """Seal a deferrals capture pass (>=1 item): write the gate artifact.

    Counts this session's capture_add events and writes deferrals-<sid>.md with
    that count. If zero items were captured, use `empty-pass --reason ...`
    instead (the anti-rubber-stamp rule requires a reason for an empty pass).
    """
    sid = session_id or _detect_session_id()
    n = count_capture_adds(sid, _events_path())
    if n == 0:
        typer.echo(
            "error: no capture_add events for this session; "
            "use 'fno backlog capture empty-pass --reason ...' for an honest empty pass",
            err=True,
        )
        raise typer.Exit(code=2)
    artifacts_dir = _repo_root() / ".fno" / "artifacts"
    artifact = write_capture_artifact(session_id=sid, entries_written=n, artifacts_dir=artifacts_dir)
    typer.echo(json.dumps({"session_id": sid, "artifact": str(artifact), "entries_written": n}))


@cli.command("promote")
def cmd_promote(
    fu_id: str = typer.Argument(..., help="The fu-XXXXXX id to promote."),
    priority: Optional[str] = typer.Option(None, "--priority", help="Override node priority (else inherits the item's)."),
) -> None:
    """Promote an inbox item to a graph node (idempotent)."""
    try:
        result = promote_item(_inbox_path(), fu_id, priority=priority)
    except InboxValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2)

    if result["status"] == "promoted":
        _emit("capture_promote", {"fu_id": fu_id, "node_id": result["node_id"]})
    typer.echo(json.dumps(result))


@cli.command("dismiss")
def cmd_dismiss(
    fu_id: str = typer.Argument(..., help="The fu-XXXXXX id to dismiss."),
    reason: str = typer.Option(..., "--reason", "-R", help="Why it's dismissed (preserved, never deleted)."),
) -> None:
    """Dismiss an inbox item (struck [-], preserved for provenance)."""
    if not (reason or "").strip():
        typer.echo("error: --reason must not be empty", err=True)
        raise typer.Exit(code=2)
    try:
        result = dismiss_item(_inbox_path(), fu_id, reason)
    except InboxValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2)

    if result["status"] == "dismissed":
        _emit("capture_dismiss", {"fu_id": fu_id, "reason": result["reason"]})
    typer.echo(json.dumps(result))


@cli.command("archive")
def cmd_archive() -> None:
    """Sweep struck (promoted/dismissed) items into a sibling archive file."""
    result = archive_struck(_inbox_path())
    typer.echo(json.dumps(result))


@cli.command("tidy")
def cmd_tidy(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the summary as JSON."),
    include_deferred: bool = typer.Option(
        False,
        "--include-deferred",
        help="Also eject filed nodes that are deferred (default: only "
        "completed/superseded).",
    ),
) -> None:
    """One idempotent pass: eject completed filed nodes, dedup (report-only),
    normalize separators, rebuild the pinned #jc + followups digest."""
    try:
        result = tidy(
            _inbox_path(),
            graph_path=_graph_path_for_tidy(),
            include_deferred=include_deferred,
        )
    except InboxValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2)
    except InboxLockError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    if result.get("graph_warning"):
        typer.echo(f"warning: {result['graph_warning']}", err=True)
    _emit(
        "capture_tidy",
        {"session_id": _detect_session_id(), "ejected": result["ejected"]},
    )

    if as_json:
        typer.echo(json.dumps(result))
        return
    typer.echo(
        f"tidy: ejected {result['ejected']} filed node(s), "
        f"{result['followups_open']} open followup(s), "
        f"{result['jc_actions']} #jc action(s), "
        f"{len(result['dedup_clusters'])} dedup cluster(s)."
    )
    for cluster in result["dedup_clusters"]:
        typer.echo(f"  dup: {', '.join(cluster)}", err=True)
