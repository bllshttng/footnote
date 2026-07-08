#!/usr/bin/env python3
"""fno.plan._stamp -- write completion stamps into plan frontmatter.

In-package module (formerly scripts/lib/stamp-plan.py). Invoked by target and
megawalk after a PR ships. Updates the plan's YAML frontmatter with shipping
metadata and writes a human-readable COMPLETION.md at the plan folder root.

Usage:
    python3 -m fno.plan._stamp stamp \\
        --plan-path <folder-or-file> \\
        --session-id <id> \\
        --url <pr-url>              # may repeat
        [--expected-url-count N]   # default 1; used by cross-project graduation
        [--completion-note "text"] # written to COMPLETION.md
        [--dry-run]

    python3 -m fno.plan._stamp graduate \\
        --plan-path <folder-or-file> \\
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


# ---------------------------------------------------------------------------
# Frontmatter parsing / serialization
# ---------------------------------------------------------------------------

_FRONT_RE = re.compile(r"^---\n(.*?)\n---(?:\n|$)", re.DOTALL)
_INLINE_LIST_RE = re.compile(r"^\[(?P<body>.*)\]$")


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
    """Strip surrounding quotes (single or double) from a scalar value."""
    raw = raw.strip()
    if (raw.startswith('"') and raw.endswith('"')) or (
        raw.startswith("'") and raw.endswith("'")
    ):
        return raw[1:-1]
    return raw


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
    for item in body.split(","):
        item = item.strip()
        if item:
            items.append(_parse_scalar(item))
    return items


def _serialize_inline_list(items: list[str]) -> str:
    """Serialize a list back to inline YAML format: [item1, item2]."""
    if not items:
        return "[]"
    formatted = ", ".join(items)
    return f"[{formatted}]"


def parse_frontmatter(content: str) -> tuple[dict[str, Any], str, str]:
    """Parse YAML frontmatter from markdown content.

    Returns (fields, raw_frontmatter_block, rest_of_content).
    fields maps key -> str (scalar) or list[str] (inline list).

    Raises ValueError if:
    - There is no frontmatter
    - A line cannot be parsed (e.g. nested structures)
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
            continue
        # An indented line at this level is leftover from an unclosed parent;
        # the inner block-list reader below consumes its own children, so
        # anything that surfaces here is genuinely nested = error.
        if line.startswith(" ") or line.startswith("\t"):
            raise ValueError(
                f"Malformed frontmatter at line {lineno}: nested structures are not "
                f"supported - offending line: {line!r}"
            )
        if ":" not in line:
            raise ValueError(
                f"Malformed frontmatter at line {lineno}: cannot parse {line!r}"
            )
        key, _, raw_val = line.partition(":")
        key = key.strip()
        raw_val = raw_val.strip()

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
                    # this line as a fresh key.
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
                fields[key] = items
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

    if plan_path.is_dir():
        idx = plan_path / "00-INDEX.md"
        if not idx.exists():
            raise FileNotFoundError(
                f"Folder plan has no 00-INDEX.md: {plan_path}"
            )
        target = idx
    elif plan_path.is_file():
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
    is_folder_plan = plan_path.is_dir()

    try:
        target, fields, rest = read_plan_file(plan_path)
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    now_utc = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

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

    # Status - always set to shipped on stamp (graduate upgrades to done)
    if fields.get("status") not in ("shipped", "done"):
        fields["status"] = "shipped"

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

    # Write COMPLETION.md for folder plans only
    if is_folder_plan:
        # Resolve project artifacts dir: assume cwd is the project root
        # (consistent with how stamp-plan.py is invoked from target). When
        # the artifacts dir is missing, aggregate_deferred_findings is a
        # no-op, so passing it unconditionally is safe.
        artifacts_dir = Path.cwd() / ".fno" / "artifacts"
        _write_completion_md(
            plan_dir=plan_path,
            shipped_at=fields["shipped_at"],
            session_id=session_id,
            urls=new_urls,
            note=args.completion_note,
            dry_run=args.dry_run,
            artifacts_dir=artifacts_dir,
        )

    return 0


# ---------------------------------------------------------------------------
# Graduate subcommand
# ---------------------------------------------------------------------------

def cmd_graduate(args: argparse.Namespace) -> int:
    """Flip status: shipped -> done when enough URLs have accumulated."""
    plan_path = Path(args.plan_path).expanduser().resolve()

    try:
        target, fields, rest = read_plan_file(plan_path)
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    status = fields.get("status", "")
    if status != "shipped":
        # Nothing to do
        return 0

    urls = fields.get("urls", [])
    if isinstance(urls, str):
        urls = [urls] if urls else []

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
        write_plan_file(target, fields, rest, dry_run=args.dry_run)

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
# COMPLETION.md writer
# ---------------------------------------------------------------------------

def aggregate_deferred_findings(artifacts_dir: Path) -> str:
    """Walk gate artifacts and collect deferred_findings from done-with-concerns.

    Returns a markdown section listing each artifact's findings, or an empty
    string when nothing is deferred. The aggregator is invoked at ship time
    by _write_completion_md so concerns recorded by sigma's two-bit
    artifact (phase 04: approved:false + deferred_findings) surface to
    humans rather than rotting in .fno/artifacts/.

    The frontmatter parser here is intentionally minimal (line-based, no
    YAML dependency) - matches the stdlib-only convention enforced
    elsewhere in stamp-plan.py.
    """
    if not artifacts_dir.exists() or not artifacts_dir.is_dir():
        return ""

    sections: list[str] = []
    for artifact_path in sorted(artifacts_dir.glob("*.md")):
        try:
            content = artifact_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if not content.startswith("---"):
            continue
        try:
            fm, _body_with_close, _body = parse_frontmatter(content)
        except Exception:
            continue

        approved = fm.get("approved")
        # Treat approved: true (or absent) as no concerns to surface.
        if str(approved).lower() != "false":
            continue
        findings_raw = fm.get("deferred_findings")
        if not findings_raw:
            continue

        # findings_raw may be a list (parsed inline-list) or a JSON-ish string.
        # We just dump it verbatim into the output - the artifact author
        # owns its shape, and downstream readers can parse it back if needed.
        phase = fm.get("phase", artifact_path.stem)
        sid = fm.get("session_id", "unknown")
        sections.append(f"### {artifact_path.stem} (phase: {phase}, session: {sid})")
        if isinstance(findings_raw, list):
            for entry in findings_raw:
                sections.append(f"- {entry}")
        else:
            sections.append(f"- {findings_raw}")
        sections.append("")

    if not sections:
        return ""
    return "\n## Deferred Findings (from done-with-concerns verdicts)\n\n" + "\n".join(sections)


def _write_completion_md(
    plan_dir: Path,
    shipped_at: str,
    session_id: str,
    urls: list[str],
    note: str | None,
    dry_run: bool = False,
    artifacts_dir: Path | None = None,
) -> None:
    """Write or append a ship section to COMPLETION.md at plan_dir root.

    When artifacts_dir is provided, deferred_findings from gate artifacts
    are aggregated into a separate section so concerns recorded under
    done-with-concerns verdicts (phase 04 two-bit gate model) surface
    here at ship time rather than rotting in .fno/artifacts/.
    """
    comp_path = plan_dir / "COMPLETION.md"

    if comp_path.exists():
        existing = comp_path.read_text(encoding="utf-8")
        # Count existing Ship N headers to determine next number
        ship_count = len(re.findall(r"^## Ship \d+", existing, re.MULTILINE))
        next_ship = ship_count + 1
    else:
        existing = "# Completion Notes\n"
        next_ship = 1

    lines = [
        f"\n## Ship {next_ship} - {shipped_at}",
        f"Session: `{session_id}`",
    ]
    for url in urls:
        lines.append(f"URL: {url}")

    if note:
        lines.append("")
        lines.append(note)

    section = "\n".join(lines) + "\n"

    deferred_section = ""
    if artifacts_dir is not None:
        deferred_section = aggregate_deferred_findings(artifacts_dir)

    new_content = existing + section + deferred_section

    if dry_run:
        print(f"[dry-run] Would write {comp_path}:")
        print(new_content)
        return

    _atomic_write(comp_path, new_content)


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
    stamp_p = sub.add_parser("stamp", help="Stamp a plan as shipped.")
    stamp_p.add_argument("--plan-path", required=True, help="Path to plan folder or file.")
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
        "--completion-note",
        default=None,
        help="Prose note written verbatim to COMPLETION.md.",
    )
    stamp_p.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would change without writing.",
    )

    # graduate subcommand
    grad_p = sub.add_parser(
        "graduate", help="Graduate a shipped plan to done when URL count is met."
    )
    grad_p.add_argument("--plan-path", required=True, help="Path to plan folder or file.")
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
    se_p.add_argument("--plan-path", required=True, help="Path to plan folder or file.")
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
