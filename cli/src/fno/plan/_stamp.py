#!/usr/bin/env python3
"""fno.plan._stamp -- write completion stamps into plan frontmatter.

In-package module (formerly scripts/lib/stamp-plan.py). Invoked by target and
megawalk after a PR ships. Updates the plan's YAML frontmatter with shipping
metadata.

Usage:
    python3 -m fno.plan._stamp stamp \\
        --plan-path <file> \\
        --session-id <id> \\
        --url <pr-url>              # may repeat
        [--expected-url-count N]   # default 1; used by cross-project graduation
        [--dry-run]

    python3 -m fno.plan._stamp graduate \\
        --plan-path <file> \\
        [--dry-run]

Frontmatter is parsed and rewritten with a minimal line-based parser that
handles scalars, flat inline lists (key: [v1, v2, ...]), and block-list of
scalars (key:\n  - v1\n  - v2). Nested block structures (block-list of
mappings like kill_criteria, or block-mapping of mappings like projects)
are preserved opaquely as RawBlock - the parser captures the indented child
lines verbatim and the serializer emits them back unchanged. stamp-plan
never reads or modifies these nested values; only the well-known mutable
scalar/list keys (status, shipped_at, urls, session_ids, expected_url_count)
are touched.

stdlib-only: no PyYAML, no third-party packages.
"""
from __future__ import annotations

import argparse
import os
import re
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from fno.plan._status import canonical_status
from fno.plan.locking import plan_doc_lock


# ---------------------------------------------------------------------------
# Frontmatter parsing / serialization
# ---------------------------------------------------------------------------

_FRONT_RE = re.compile(r"^---\n(.*?)\n---(?:\n|$)", re.DOTALL)
_INLINE_LIST_RE = re.compile(r"^\[(?P<body>.*)\]$")
# Resolve the escapes `_serialize_item` emits inside a double-quoted item.
_DQ_ESCAPE_RE = re.compile(r'\\(["\\])')
# An inline item carrying either of these cannot be written bare.
_NEEDS_QUOTE_RE = re.compile(r'[,"]')


class RawBlock:
    """Marker wrapping raw frontmatter lines for opaque pass-through.

    Used for keys whose value is a YAML structure the stdlib parser does
    not understand (e.g. block-list of mappings like `kill_criteria:`).
    The parser stores the raw indented child lines verbatim; the
    serializer emits them back unchanged. stamp-plan never reads these
    values - it only mutates the well-known scalar/list keys (status,
    shipped_at, urls, session_ids, expected_url_count).
    """
    __slots__ = ("text",)

    def __init__(self, text: str) -> None:
        self.text = text

    def __repr__(self) -> str:
        lines = self.text.splitlines()
        first_line = lines[0] if lines else ""
        return f"RawBlock({first_line!r}...)"


class BlockList(list):
    """A list that arrived as a block sequence and is emitted as one.

    Only a marker: it IS a list, so every consumer and every equality check
    against a plain list is unaffected. Without it this writer flattens every
    block list to inline while the vault's Obsidian stack re-expands it to
    block on the next write, so a plan doc's frontmatter never settles. Keys
    this writer ADDS are plain lists and stay inline.
    """
    __slots__ = ()


def _valid_count(value: Any) -> bool:
    """True when value parses as an integer >= 1.

    Used by `stamp`'s first-writer-wins guard: a present-but-malformed
    expected_url_count is treated as absent so it gets overwritten (self-heal)
    rather than stranding `graduate` on its default-to-1 fallback.
    """
    try:
        return int(str(value)) >= 1
    except (ValueError, TypeError):
        return False


def _parse_scalar(raw: str) -> str:
    """Strip surrounding quotes (single or double) from a scalar value.

    A double-quoted value also has its `\\"` / `\\\\` escapes resolved, which is
    what `_serialize_item` emits. Other backslash sequences are left verbatim -
    this reader stores every scalar raw and is not a full YAML unescaper.
    """
    raw = raw.strip()
    if len(raw) >= 2 and raw[0] == '"' and raw[-1] == '"':
        return _DQ_ESCAPE_RE.sub(r"\1", raw[1:-1])
    if len(raw) >= 2 and raw[0] == "'" and raw[-1] == "'":
        return raw[1:-1]
    return raw


def _split_inline_items(body: str) -> list[str]:
    """Split an inline-list body on commas that sit outside a double-quoted item.

    Double quotes only. A bare apostrophe is ordinary text in a plan doc
    (`don't`), so treating `'` as an opening quote here would swallow the rest
    of the list; `_parse_scalar` still strips a surrounding single-quote pair
    per item, exactly as before. `_serialize_item` only ever emits double
    quotes, so everything this module writes reads back through this path.

    An unterminated quote yields one long item rather than raising: a malformed
    doc must degrade, because `project_node_to_plan` never raises.
    """
    items: list[str] = []
    buf: list[str] = []
    in_quote = False
    escaped = False
    for ch in body:
        if escaped:
            escaped = False
        elif in_quote and ch == "\\":
            escaped = True
        elif in_quote:
            in_quote = ch != '"'
        elif ch == '"':
            in_quote = True
        elif ch == ",":
            items.append("".join(buf))
            buf = []
            continue
        buf.append(ch)
    items.append("".join(buf))
    return items


def _parse_inline_list(raw: str) -> list[str]:
    """Parse an inline YAML list like [a, b, c] into a Python list."""
    raw = raw.strip()
    m = _INLINE_LIST_RE.match(raw)
    if not m:
        return [_parse_scalar(raw)] if raw else []
    body = m.group("body").strip()
    if not body:
        return []
    items = []
    for item in _split_inline_items(body):
        item = item.strip()
        if item:
            items.append(_parse_scalar(item))
    return items


def _quote_item(item: str) -> str:
    """Wrap an item in double quotes, escaping what `_parse_scalar` resolves."""
    return '"' + item.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _bare_is_ambiguous(item: str) -> bool:
    """True when an unquoted item would not read back as itself.

    Shared by both list forms: surrounding whitespace is eaten by the reader's
    `.strip()`, an empty item is dropped entirely, and a leading quote
    character makes `_parse_scalar` try to unwrap the item.
    """
    return not item or item != item.strip() or item[0] in "\"'"


def _serialize_item(item: str) -> str:
    """Emit an inline list item, quoting it when the form would be ambiguous.

    Quote-on-write is half of the round-trip; a quote-aware reader alone still
    loses an unquoted comma item on the next read. On top of the shared cases,
    an inline item needs quoting when a comma would split it or an interior
    quote character would open a run that swallows its neighbours.
    """
    if _NEEDS_QUOTE_RE.search(item) or _bare_is_ambiguous(item):
        return _quote_item(item)
    return item


def _serialize_block_item(item: str) -> str:
    """Emit a block sequence item, which carries a comma without quoting.

    Deliberately laxer than `_serialize_item`: quoting a comma item here would
    be undone by the vault's formatter (block form needs no quotes, so it
    strips them) and re-added on the next projection, which is the oscillation
    block-form preservation exists to stop.
    """
    return _quote_item(item) if _bare_is_ambiguous(item) else item


def _serialize_inline_list(items: list[str]) -> str:
    """Serialize a list back to inline YAML format: [item1, item2]."""
    if not items:
        return "[]"
    formatted = ", ".join(_serialize_item(i) for i in items)
    return f"[{formatted}]"


def parse_frontmatter(content: str) -> tuple[dict[str, Any], str, str]:
    """Parse YAML frontmatter from markdown content.

    Returns (fields, raw_frontmatter_block, rest_of_content).
    fields maps key -> str (scalar) or list[str] (inline list).

    Content with no frontmatter is not an error: it returns empty fields and the
    content untouched.

    Raises ValueError if a line cannot be parsed - a nested structure, a wrapped
    scalar, or a line with no `key:` at all.
    """
    m = _FRONT_RE.match(content)
    if not m:
        # No frontmatter - treat as empty
        return {}, "", content

    block = m.group(1)
    rest = content[m.end():]
    fields: dict[str, Any] = {}

    # Manual index walk so a bare-key line can peek ahead for block-list
    # children (`urls:\n  - https://...`). External formatters normalize
    # the writer's inline-list output into block form, and graduate must
    # still be able to read its own files after that round-trip.
    lines = block.splitlines()
    i = 0
    # Last top-level key seen, so the nested-line error below can name the key
    # whose value ran on. The offending line is the continuation, which carries
    # no key of its own; without this the reader is told a line number and has
    # to go find the key itself.
    last_key: str | None = None
    while i < len(lines):
        line = lines[i]
        lineno = i + 1
        i += 1
        stripped = line.strip()
        if not stripped:
            continue
        # Skip comment lines. Template frontmatter commonly carries commented
        # examples like `# Optional: depends_on:` - those must not crash the parser.
        # Indented comments are also safe to ignore; they can't be a nested value
        # because comments never hold data.
        if stripped.startswith("#"):
            # Clear the attribution rather than carry it past a comment. A
            # commented-out key followed by an indented line (`# depends_on:`
            # then `  - x`) would otherwise blame the last REAL key above it,
            # and a confidently wrong key name is worse than none: the message
            # degrades to the unnamed form on its own.
            last_key = None
            continue
        # An indented line at this level is leftover from an unclosed parent;
        # the inner block-list reader below consumes its own children, so
        # anything that surfaces here is genuinely nested = error.
        if line.startswith(" ") or line.startswith("\t"):
            whose = f" (continuation of {last_key!r})" if last_key else ""
            raise ValueError(
                f"Malformed frontmatter at line {lineno}{whose}: frontmatter scalars "
                f"must be single-line - a wrapped or indented continuation line is "
                f"not supported. Put the whole value on the key's own line. "
                f"Offending line: {line!r}"
            )
        if ":" not in line:
            raise ValueError(
                f"Malformed frontmatter at line {lineno}: cannot parse {line!r}"
            )
        key, _, raw_val = line.partition(":")
        key = key.strip()
        raw_val = raw_val.strip()
        last_key = key

        if raw_val.startswith("["):
            fields[key] = _parse_inline_list(raw_val)
        elif raw_val == "":
            # Bare key. Try block-list-of-scalars first; on first child line
            # that breaks that shape (mapping item like `- key: value`, or a
            # non-`- ` continuation line indicating a continuation of an
            # already-accepted item), switch to RawBlock pass-through and
            # treat the entire indented block as opaque verbatim text.
            # stamp-plan never reads nested values - it only mutates the
            # well-known scalar/list keys (status, urls, session_ids,
            # shipped_at, expected_url_count) - so opaque preservation is
            # sufficient for round-trip stability.
            start_idx = i  # first child line; used to recover raw text on RawBlock switch
            items: list[str] = []
            raw_lines: list[str] = []
            saw_child = False
            is_raw = False
            while i < len(lines):
                child = lines[i]
                child_stripped = child.strip()
                if not child_stripped:
                    # Blank line inside the block: pass-through in raw mode,
                    # skip in scalar mode (allowed without closing the block).
                    if is_raw:
                        raw_lines.append(child)
                    i += 1
                    continue
                if not (child.startswith(" ") or child.startswith("\t")):
                    # De-indented = the block ended; let the outer loop re-process
                    # this line as a fresh key. This loop must drain every
                    # contiguous indented line before breaking: an indented line
                    # that reaches the outer loop is reported as a runaway
                    # continuation of `last_key`, which is only the right key
                    # because nothing mid-block ever gets there.
                    break
                if child_stripped.startswith("#"):
                    if is_raw:
                        raw_lines.append(child)
                    i += 1
                    continue
                if is_raw:
                    raw_lines.append(child)
                    i += 1
                    continue
                if child_stripped.startswith("- "):
                    # Treat as scalar by default. If this `- ` item is
                    # actually a mapping (`- name: foo`), the next iteration
                    # will see the continuation line (`    predicate: ...`)
                    # which triggers the raw-mode switch below, at which
                    # point we discard accepted scalars and re-collect the
                    # block from start_idx so the on-disk text round-trips
                    # byte-stable. Detecting the mapping proactively via
                    # `":" in inner` would false-trigger on URL items where
                    # the value contains `://` (e.g. block-list of urls).
                    inner = child_stripped[2:].strip()
                    items.append(_parse_scalar(inner))
                    saw_child = True
                    i += 1
                    continue
                # Indented but not `- `: continuation of a previously-accepted
                # mapping item. Switch to raw mode.
                is_raw = True
                raw_lines = list(lines[start_idx:i])
                raw_lines.append(child)
                items = []
                saw_child = False
                i += 1

            if is_raw:
                # Strip trailing blank lines so round-trip doesn't grow the file.
                while raw_lines and not raw_lines[-1].strip():
                    raw_lines.pop()
                fields[key] = RawBlock("\n".join(raw_lines))
            elif saw_child:
                fields[key] = BlockList(items)
            else:
                fields[key] = ""
        else:
            fields[key] = raw_val  # scalar (string)

    return fields, block, rest


def serialize_frontmatter(fields: dict[str, Any]) -> str:
    """Serialize fields back to a YAML frontmatter block (without --- delimiters)."""
    lines = []
    for key, value in fields.items():
        if isinstance(value, RawBlock):
            # Bare key followed by raw indented children, verbatim.
            lines.append(f"{key}:")
            if value.text:
                lines.append(value.text)
        elif isinstance(value, BlockList) and value:
            # An empty block list has no children to carry it and would read
            # back as a bare-key scalar, so it falls through to inline `[]`.
            lines.append(f"{key}:")
            lines.extend(f"  - {_serialize_block_item(item)}" for item in value)
        elif isinstance(value, list):
            lines.append(f"{key}: {_serialize_inline_list(value)}")
        else:
            lines.append(f"{key}: {value}")
    return "\n".join(lines)


def read_plan_file(plan_path: Path) -> tuple[Path, dict[str, Any], str]:
    """Read and parse a plan file. Returns (index_file, fields, rest_content)."""
    # Epic-decomposition group nodes carry plan_path of the form
    # `<doc>#group-<slug>` - the `#group-<slug>` fragment selects a section of
    # the shared design doc and is not part of any real filesystem path. If the
    # literal path is absent but dropping the trailing `#group-<slug>` fragment
    # yields a real file, use that. Scoped to the `#group-` form on purpose: an
    # unrelated typo like `spec#draft.md` must still fail fast rather than
    # silently stamp `spec`. rpartition isolates the LAST `#group-` so a genuine
    # filename containing an earlier `#` survives; the literal path wins first.
    if not plan_path.exists() and "#group-" in plan_path.name:
        stripped_name = plan_path.name.rpartition("#group-")[0]
        if stripped_name:
            stripped = plan_path.with_name(stripped_name)
            if stripped.exists():
                plan_path = stripped

    if plan_path.is_file():
        target = plan_path
    else:
        raise FileNotFoundError(f"Plan path does not exist: {plan_path}")

    content = target.read_text(encoding="utf-8")
    fields, _block, rest = parse_frontmatter(content)
    return target, fields, rest


def _atomic_write(target: Path, content: str) -> None:
    """Write content to target atomically via tmp + os.replace.

    Guards against truncation if the process is interrupted mid-write.
    Preserves the target's existing file mode so os.replace does not
    downgrade a 0644 plan file to the mkstemp default of 0600.
    """
    target.parent.mkdir(parents=True, exist_ok=True)
    # Snapshot the target's mode BEFORE creating the tmp so we can restore it.
    # If the target does not exist yet (first write), fall back to the process
    # umask-driven default that a plain open() would have produced.
    original_mode: int | None = None
    if target.exists():
        try:
            original_mode = target.stat().st_mode & 0o777
        except OSError:
            original_mode = None
    fd, tmp_name = tempfile.mkstemp(
        prefix=f".{target.name}.", suffix=".tmp", dir=str(target.parent)
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        if original_mode is not None:
            try:
                os.chmod(tmp_name, original_mode)
            except OSError:
                pass  # best-effort; atomicity matters more than permissions
        os.replace(tmp_name, target)
    except Exception:
        # Best-effort cleanup of the tmp file; re-raise the original error.
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def write_plan_file(
    target: Path,
    fields: dict[str, Any],
    rest: str,
    dry_run: bool = False,
) -> None:
    """Write the updated frontmatter back to the plan file."""
    fm_block = serialize_frontmatter(fields)
    new_content = f"---\n{fm_block}\n---\n{rest}"
    if dry_run:
        print(f"[dry-run] Would write {target}:")
        print(new_content)
        return
    _atomic_write(target, new_content)


def _plan_node_id(fields: dict[str, Any]) -> str | None:
    claims = fields.get("claims")
    if isinstance(claims, str) and claims.strip():
        return claims.strip()
    if isinstance(claims, list):
        for claim in claims:
            if isinstance(claim, str) and claim.strip():
                return claim.strip()
    return None


def _plan_urls(fields: dict[str, Any]) -> list[str]:
    urls = fields.get("urls", [])
    if isinstance(urls, str):
        return [urls] if urls else []
    return [url for url in urls if isinstance(url, str)] if isinstance(urls, list) else []


def _plan_expected_url_count(fields: dict[str, Any]) -> int | None:
    raw = fields.get("expected_url_count")
    try:
        value = int(str(raw))
    except (TypeError, ValueError):
        return None
    return value if value >= 1 else None


def _latest_plan_session(fields: dict[str, Any]) -> str | None:
    sessions = fields.get("session_ids", [])
    if isinstance(sessions, str):
        return sessions or None
    if isinstance(sessions, list):
        for session in reversed(sessions):
            if isinstance(session, str) and session:
                return session
    return None


def _emit_plan_event(event_type: str, **kwargs: Any) -> None:
    """Report telemetry failure without changing the durable plan outcome."""
    try:
        from fno.events import append_event, plan_graduated, plan_stamped

        builder = plan_stamped if event_type == "plan_stamped" else plan_graduated
        append_event(builder(**kwargs))
    except Exception as exc:  # noqa: BLE001 - the plan write already succeeded
        print(f"warning: failed to emit {event_type}: {exc}", file=sys.stderr)


# ---------------------------------------------------------------------------
# Stamp subcommand
# ---------------------------------------------------------------------------

def cmd_stamp(args: argparse.Namespace) -> int:
    """Stamp a plan with shipping metadata."""
    # Validate expected_url_count before touching the file: a value < 1 would
    # make cmd_graduate's `len(urls) >= expected` always true and graduate the
    # plan after zero URLs. Mirrors cmd_set_expected's guard.
    if args.expected_url_count is not None and args.expected_url_count < 1:
        print(
            f"error: --expected-url-count must be >= 1 (got {args.expected_url_count})",
            file=sys.stderr,
        )
        return 2

    plan_path = Path(args.plan_path).expanduser().resolve()

    # Serialize the full read-modify-write against a concurrent status-fanout
    # progress append on the same doc (both are whole-file rewrites; an atomic
    # write alone loses one side). The append yields on timeout, so the ship-gate
    # stamp is the winner and effectively never waits.
    with plan_doc_lock(plan_path):
        return _do_stamp(args, plan_path)


def _do_stamp(args: argparse.Namespace, plan_path: Path) -> int:
    try:
        target, fields, rest = read_plan_file(plan_path)
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    now_utc = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    status_from = fields.get("status")

    # Idempotency: check if this (session_id, url) pair is already present
    existing_urls: list[str] = fields.get("urls", [])
    if isinstance(existing_urls, str):
        existing_urls = [existing_urls] if existing_urls else []

    existing_sids: list[str] = fields.get("session_ids", [])
    if isinstance(existing_sids, str):
        existing_sids = [existing_sids] if existing_sids else []

    new_urls = list(args.url) if args.url else []
    session_id = args.session_id

    # Check if all new_urls are already present AND session_id is already present
    all_urls_present = all(u in existing_urls for u in new_urls)
    sid_present = session_id in existing_sids

    if all_urls_present and sid_present:
        # Fully idempotent - no-op
        if not args.dry_run:
            _emit_plan_event(
                "plan_stamped",
                plan_path=str(target),
                session_id=session_id,
                node_id=_plan_node_id(fields),
                outcome="idempotent_noop",
                status_from=str(status_from) if status_from is not None else None,
                status_to=str(fields.get("status")) if fields.get("status") is not None else None,
                urls=_plan_urls(fields),
                expected_url_count=_plan_expected_url_count(fields),
                reason="stamp already contains the session and URL",
            )
        return 0

    # Not a full duplicate - merge new data in
    # shipped_at: only set on first ship, never overwritten
    if "shipped_at" not in fields or not fields.get("shipped_at"):
        fields["shipped_at"] = now_utc

    # Accumulate URLs (de-duped)
    for url in new_urls:
        if url not in existing_urls:
            existing_urls.append(url)
    fields["urls"] = existing_urls

    # Accumulate session IDs (de-duped)
    if session_id not in existing_sids:
        existing_sids.append(session_id)
    fields["session_ids"] = existing_sids

    # Status - always set to in_review on stamp (graduate upgrades to done).
    # Compared canonically so a doc already at the retired `shipped` spelling
    # reads as in_review and keeps its bytes: the alias translates on read, it
    # never triggers a migration write.
    if canonical_status(fields.get("status")) not in ("in_review", "done"):
        fields["status"] = "in_review"

    # Store expected_url_count if provided AND no VALID count is already present.
    # First-writer-wins: for a decomposed epic, `set-expected` writes the
    # authoritative group count N onto the shared doc before any group ships,
    # so a per-group ship passing --expected-url-count 1 must not lower it.
    # Matches shipped_at's first-write semantics. A malformed (non-integer)
    # existing value is treated as absent and overwritten, so a corrupted field
    # self-heals rather than leaving graduate to fall back to 1 forever.
    if args.expected_url_count is not None and not _valid_count(fields.get("expected_url_count")):
        fields["expected_url_count"] = str(args.expected_url_count)

    # Write the updated plan file
    write_plan_file(target, fields, rest, dry_run=args.dry_run)

    if not args.dry_run:
        _emit_plan_event(
            "plan_stamped",
            plan_path=str(target),
            session_id=session_id,
            node_id=_plan_node_id(fields),
            outcome="stamped",
            status_from=str(status_from) if status_from is not None else None,
            status_to=str(fields.get("status")) if fields.get("status") is not None else None,
            urls=_plan_urls(fields),
            expected_url_count=_plan_expected_url_count(fields),
        )

    return 0


# ---------------------------------------------------------------------------
# Graduate subcommand
# ---------------------------------------------------------------------------

def cmd_graduate(args: argparse.Namespace) -> int:
    """Flip status: in_review -> done when enough URLs have accumulated."""
    plan_path = Path(args.plan_path).expanduser().resolve()
    with plan_doc_lock(plan_path):  # serialize with a concurrent progress append
        return _do_graduate(args, plan_path)


def _do_graduate(args: argparse.Namespace, plan_path: Path) -> int:
    try:
        target, fields, rest = read_plan_file(plan_path)
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    status = fields.get("status", "")
    status_from = str(status) if status else None
    session_id = _latest_plan_session(fields)
    urls = _plan_urls(fields)
    expected_count = _plan_expected_url_count(fields)
    if canonical_status(status) != "in_review":
        if not args.dry_run:
            _emit_plan_event(
                "plan_graduated",
                plan_path=str(target),
                session_id=session_id,
                node_id=_plan_node_id(fields),
                outcome="idempotent_noop",
                status_from=status_from,
                status_to=status_from,
                urls=urls,
                expected_url_count=expected_count,
                reason=f"status is {status or '(unset)'}",
            )
        return 0

    expected_raw = fields.get("expected_url_count", "1")
    try:
        expected = int(expected_raw)
    except (ValueError, TypeError):
        print(
            f"warning: expected_url_count={expected_raw!r} is not an integer; "
            "defaulting to 1. Cross-project plans may graduate early if this "
            "was set by an earlier writer.",
            file=sys.stderr,
        )
        expected = 1

    if len(urls) >= expected:
        fields["status"] = "done"
        # done_at = the merge/graduation timestamp (first-write-only, mirrors
        # shipped_at). done now means MERGED on both plan and graph (x-f34f).
        if not fields.get("done_at"):
            fields["done_at"] = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        write_plan_file(target, fields, rest, dry_run=args.dry_run)
        if not args.dry_run:
            _emit_plan_event(
                "plan_graduated",
                plan_path=str(target),
                session_id=session_id,
                node_id=_plan_node_id(fields),
                outcome="graduated",
                status_from=status_from,
                status_to="done",
                urls=urls,
                expected_url_count=expected,
            )
    elif not args.dry_run:
        _emit_plan_event(
            "plan_graduated",
            plan_path=str(target),
            session_id=session_id,
            node_id=_plan_node_id(fields),
            outcome="not_met",
            status_from=status_from,
            status_to=status_from,
            urls=urls,
            expected_url_count=expected,
            reason=f"{len(urls)} URL(s) present; {expected} required",
        )

    return 0


# ---------------------------------------------------------------------------
# Set-expected subcommand
# ---------------------------------------------------------------------------

def cmd_set_expected(args: argparse.Namespace) -> int:
    """Authoritatively write expected_url_count into a plan's frontmatter.

    Count-only writer invoked by `fno backlog decompose` to record N (the
    number of group children) on the shared epic doc before any group ships.
    Unlike `stamp`'s first-writer-wins, this OVERWRITES: decompose is the
    authority on the group count, so re-decomposition updates it. Touches no
    other field. Creates a frontmatter block if the doc has none.
    """
    if args.count < 1:
        print(f"error: --count must be >= 1 (got {args.count})", file=sys.stderr)
        return 2

    plan_path = Path(args.plan_path).expanduser().resolve()
    try:
        target, fields, rest = read_plan_file(plan_path)
    except FileNotFoundError as exc:
        # Distinct exit code: a non-existent doc is benign for callers (it
        # cannot graduate early because it cannot be stamped at ship time
        # either), so decompose treats exit 3 as a skip rather than a failure.
        print(f"error: {exc}", file=sys.stderr)
        return 3
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    fields["expected_url_count"] = str(args.count)
    write_plan_file(target, fields, rest, dry_run=args.dry_run)
    return 0


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="stamp-plan.py",
        description="Write completion stamps into plan frontmatter.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    # stamp subcommand
    stamp_p = sub.add_parser("stamp", help="Stamp a plan as in_review.")
    stamp_p.add_argument("--plan-path", required=True, help="Path to plan file.")
    stamp_p.add_argument("--session-id", required=True, help="Claude session ID.")
    stamp_p.add_argument(
        "--url",
        action="append",
        default=[],
        help="PR URL (may repeat for multi-repo).",
    )
    stamp_p.add_argument(
        "--expected-url-count",
        type=int,
        default=None,
        help="Number of URLs expected before graduation (default: 1).",
    )
    stamp_p.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would change without writing.",
    )

    # graduate subcommand
    grad_p = sub.add_parser(
        "graduate", help="Graduate an in_review plan to done when URL count is met."
    )
    grad_p.add_argument("--plan-path", required=True, help="Path to plan file.")
    grad_p.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would change without writing.",
    )

    # set-expected subcommand
    se_p = sub.add_parser(
        "set-expected",
        help="Authoritatively set expected_url_count (count-only; used by decompose).",
    )
    se_p.add_argument("--plan-path", required=True, help="Path to plan file.")
    se_p.add_argument(
        "--count", required=True, type=int, help="Expected URL count (must be >= 1)."
    )
    se_p.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would change without writing.",
    )

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    if args.command == "stamp":
        return cmd_stamp(args)
    elif args.command == "graduate":
        return cmd_graduate(args)
    elif args.command == "set-expected":
        return cmd_set_expected(args)
    else:
        parser.print_help()
        return 1


if __name__ == "__main__":
    sys.exit(main())
