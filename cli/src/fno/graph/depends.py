"""Dependency resolution for graph intake.

Public API:
    _parse_frontmatter(plan_path) -> _FrontmatterData | None
    _collect_frontmatter_depends(plan_path) -> (list[str], Path)
    _resolve_depends_on(raw_values, entries, plan_dir) -> (list[str], list[str])
    _sequence_token(basename) -> str | None
    _first_h1(plan_path) -> str | None
    _derive_title(plan_path, override) -> str
    _parse_inline_yaml_list(src) -> list[str] | None
"""
from __future__ import annotations

import os
import re
import sys
from pathlib import Path
from typing import TypedDict, Union, cast

from fno.graph._constants import has_node_id_prefix


class _FrontmatterData(TypedDict, total=False):
    """Shape of a parsed frontmatter block.

    `total=False` because frontmatter fields are optional - missing keys
    simply don't appear. Callers use `.get()` / `isinstance()` guards.
    String-valued keys land as `str`; the `depends_on` key, when present
    as a list, lands as `list[str]`.
    """
    title: str
    depends_on: Union[list[str], str, None]


def _parse_frontmatter(plan_path: Path) -> _FrontmatterData | None:
    """Parse a minimal YAML frontmatter block from a markdown file.

    Handles scalar string values and list-of-strings values written with
    `- ` prefixes. Sufficient for the `title:` and `depends_on:` fields
    intake cares about. Returns None if no fenced frontmatter is present
    or the file cannot be read; file-read failures log a stderr warning
    so callers can distinguish "no frontmatter" from "unreadable file."
    """
    try:
        text = plan_path.read_text()
    except OSError as e:
        print(
            f"Warning: could not read frontmatter from {plan_path}: {e}",
            file=sys.stderr,
        )
        return None
    if not text.startswith("---\n"):
        return None
    end = text.find("\n---", 4)
    if end == -1:
        return None
    body = text[4:end]
    result: dict = {}
    current_key: str | None = None
    for raw_line in body.splitlines():
        stripped = raw_line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if raw_line.lstrip().startswith("- ") and current_key is not None:
            value = raw_line.lstrip()[2:].strip()
            if " #" in value:
                value = value.split(" #", 1)[0].rstrip()
            value = value.strip('"').strip("'")
            if not isinstance(result.get(current_key), list):
                result[current_key] = []
            if value:
                result[current_key].append(value)
            continue
        if ":" in raw_line and not raw_line.startswith(" "):
            key, _, val = raw_line.partition(":")
            key = key.strip()
            val = val.strip()
            if " #" in val:
                val = val.split(" #", 1)[0].rstrip()
            val = val.strip('"').strip("'")
            current_key = key
            result[key] = val if val else None
    return cast(_FrontmatterData, result)


def _first_h1(plan_path: Path) -> str | None:
    """Return the first H1 heading in a markdown file (skipping frontmatter).

    File-read failures log a stderr warning rather than silently falling
    through to a filename-derived title, so unreadable plan files are
    visible in the intake output.
    """
    try:
        text = plan_path.read_text()
    except OSError as e:
        print(f"Warning: could not read {plan_path} for H1: {e}", file=sys.stderr)
        return None
    lines = text.splitlines()
    i = 0
    if lines and lines[0] == "---":
        i = 1
        while i < len(lines) and lines[i] != "---":
            i += 1
        i += 1
    while i < len(lines):
        line = lines[i]
        if line.startswith("# "):
            return line[2:].strip()
        i += 1
    return None


# Batch-intake file pattern. Matches `NN-name.md` and `NNa-name.md` forms
# so parallel-lane plans (02a-, 02b-, 04a-, 04b-, 04c-, ...) that are
# common in /spec fork folders are included in --batch. `00-*` remains
# reserved for the INDEX and is excluded by filename check.
_PLAN_FILE_RE = re.compile(r"^\d{2}[a-zA-Z]?-.+\.md$")
_TITLE_PREFIX_RE = re.compile(r"^\d+[a-zA-Z]?-")
_SEQUENCE_PREFIX_RE = re.compile(r"^(\d{1,2}[a-zA-Z]?)-")


def _sequence_token(plan_path_basename: str) -> str | None:
    """Extract the NN / NNa sequence token from a plan filename, or None.

    `01-foo.md` -> `01`, `02a-bar.md` -> `02a`. Used to resolve bare
    sequence references like `depends_on: 02a` within a batch.
    """
    m = _SEQUENCE_PREFIX_RE.match(plan_path_basename)
    return m.group(1) if m else None


def _derive_title(plan_path: Path, override: str | None) -> str:
    """Derive a display title for a plan file.

    Order: explicit override, frontmatter title, first H1 heading, filename
    slug with numeric prefix stripped.
    """
    if override:
        return override
    fm = _parse_frontmatter(plan_path)
    if fm and isinstance(fm.get("title"), str) and fm["title"]:
        return fm["title"]
    h1 = _first_h1(plan_path)
    if h1:
        return h1
    stem = _TITLE_PREFIX_RE.sub("", plan_path.stem)
    return stem.replace("-", " ").strip().title() or plan_path.name


def _parse_inline_yaml_list(src: str) -> list[str] | None:
    """Parse a simple `[a, b, c]` inline YAML list. Returns None on failure.

    Supports bare tokens, quoted strings (single or double), and empty
    lists (`[]`). Does NOT support nested brackets or escaped commas -
    those cases return None so the caller can warn and skip. Covers the
    common /spec output shapes (`depends_on: []`, `depends_on: [ab-123,
    ab-456]`, `depends_on: ["path/a", "path/b"]`).
    """
    s = src.strip()
    if not (s.startswith("[") and s.endswith("]")):
        return None
    inner = s[1:-1].strip()
    if not inner:
        return []
    if "[" in inner or "]" in inner:
        return None
    parts: list[str] = []
    for token in inner.split(","):
        t = token.strip()
        if not t:
            continue
        if (t.startswith('"') and t.endswith('"')) or (t.startswith("'") and t.endswith("'")):
            t = t[1:-1]
        if t:
            parts.append(t)
    return parts


def _collect_frontmatter_depends(plan_path: str) -> tuple[list[str], Path]:
    """Read depends_on from a plan file's frontmatter.

    Returns (raw_values, plan_dir) where plan_dir is the directory used as
    the base for resolving relative dependency paths.

    Value forms accepted in the frontmatter:
    - Block list (preferred): `depends_on:` followed by `- entry` lines.
    - Inline YAML list: `depends_on: [a, b]` or `depends_on: []`.
    - Scalar string: `depends_on: ab-xxxxxxxx` or a single slug. Coerced
      to a one-element list.

    Complex inline forms (nested brackets, escaped commas) fall back to
    a stderr warning + empty list rather than silently dropping edges.
    """
    p = Path(plan_path)
    if p.is_file():
        fm = _parse_frontmatter(p)
        plan_dir = p.parent
    else:
        return [], Path(plan_path).parent
    if not fm:
        return [], plan_dir
    raw = fm.get("depends_on")
    if isinstance(raw, list):
        return [str(x).strip() for x in raw if str(x).strip()], plan_dir
    if isinstance(raw, str) and raw.strip():
        s = raw.strip()
        if s.startswith("[") and s.endswith("]"):
            parsed = _parse_inline_yaml_list(s)
            if parsed is not None:
                return parsed, plan_dir
            print(
                f"Warning: depends_on in {plan_path} uses an inline-list form "
                "this parser can't read (nested brackets or escaped commas). "
                "Switch to block form (`depends_on:` then `- entry` lines).",
                file=sys.stderr,
            )
            return [], plan_dir
        return [s], plan_dir
    return [], plan_dir


def _resolve_depends_on(
    raw_values: list[str],
    entries: list[dict],
    plan_dir: Path,
) -> tuple[list[str], list[str]]:
    """Resolve a list of depends_on references to ab-IDs.

    Each raw value may be:
    - An `ab-XXXXXXXX` ID (matched against the graph)
    - A slug or path (resolved against `entries[].plan_path`)
    - A bare sequence token (`01`, `02a`, `3`) - resolved against graph
      entries that live in the SAME directory as the current plan.
      Batch-scoped so sequence tokens don't collide across projects.

    Numeric tokens are normalized: `3` matches `03-foo.md`, `01` matches
    a user-written `1`. Letter-suffix tokens are exact (`02a` only).
    Returns `(resolved_ids, unresolved_raw)`.
    """
    id_set = {e.get("id") for e in entries if isinstance(e.get("id"), str)}
    by_path: dict[str, str] = {}
    by_slug: dict[str, str] = {}
    # Batch-scoped: sequence-token -> ab-ID, populated only from graph
    # entries whose plan_path directory matches the current plan_dir.
    #
    # Use `abspath` (not `.resolve()`) so we don't follow symlinks - stored
    # plan_paths keep whatever form the caller originally passed (symlinked
    # or not), and resolving here would break the match on macOS where
    # /var -> /private/var.
    plan_dir_abs = (
        os.path.normpath(os.path.abspath(str(plan_dir))) if plan_dir else None
    )
    by_sequence_in_dir: dict[str, str] = {}

    for e in entries:
        eid = e.get("id")
        pp = e.get("plan_path")
        if not isinstance(eid, str) or not pp:
            continue
        norm = os.path.normpath(pp)
        by_path[norm] = eid
        # Graph entries can store plan_path as either absolute or relative
        # (whatever the caller passed to `intake`). If the stored form is
        # relative and the entry's cwd is known, also index by the
        # absolute path so dependents whose candidate is a resolved abs
        # path can still match. Without this, graphs built from mixed
        # absolute/relative intakes silently drop edges on resolution.
        cwd = e.get("cwd")
        abs_form: str | None = None
        if cwd and not os.path.isabs(norm):
            abs_form = os.path.normpath(os.path.join(cwd, norm))
            by_path.setdefault(abs_form, eid)
        slug = os.path.basename(norm.rstrip(os.sep))
        if slug and slug not in by_slug:
            by_slug[slug] = eid
        # Parent-folder indexing lets users write `depends_on: 2026-04-19-foo`
        # and have it resolve against a plan nested under `plans/2026-04-19-foo/`.
        # But for flat-file layouts the parent is often a generic container name
        # like `plans/` or `specs/`, which would collide across every node
        # and make `depends_on: plans` spuriously match the first intaked
        # plan. Require a hyphen in the parent slug so the heuristic fires
        # only on meaningful per-feature folder names (dated slugs, multi-
        # word kebab names) and never on generic containers.
        parent = os.path.basename(os.path.dirname(norm))
        if parent and "-" in parent and parent not in by_slug:
            by_slug[parent] = eid

        # Sequence-token index: only include entries whose directory
        # matches plan_dir. Honors both the stored-path-absolute case and
        # the stored-relative-with-cwd case.
        if plan_dir_abs:
            entry_dirs = {os.path.normpath(os.path.dirname(norm))}
            if abs_form:
                entry_dirs.add(os.path.normpath(os.path.dirname(abs_form)))
            if plan_dir_abs in entry_dirs:
                basename = os.path.basename(norm)
                token = _sequence_token(basename)
                if token:
                    by_sequence_in_dir.setdefault(token, eid)
                    # Normalize numeric-only tokens so `1` and `01` both work.
                    if token.isdigit():
                        by_sequence_in_dir.setdefault(token.lstrip("0") or "0", eid)
                        by_sequence_in_dir.setdefault(token.zfill(2), eid)

    resolved: list[str] = []
    unresolved: list[str] = []
    for raw in raw_values:
        if has_node_id_prefix(raw):
            if raw in id_set:
                resolved.append(raw)
            else:
                unresolved.append(raw)
            continue
        matched: str | None = None
        candidate_paths = [raw]
        if not os.path.isabs(raw):
            try:
                candidate_paths.append(str((plan_dir / raw).resolve()))
            except (OSError, RuntimeError):
                pass
        for c in candidate_paths:
            norm = os.path.normpath(c)
            if norm in by_path:
                matched = by_path[norm]
                break
        if not matched:
            slug = os.path.basename(raw.rstrip("/").rstrip(os.sep))
            if slug and slug in by_slug:
                matched = by_slug[slug]
        if not matched:
            # Final fallback: batch-local sequence-token lookup. The index
            # was populated with both the raw token (`01`) AND its
            # lstrip-0 form (`1`) for numeric tokens, so a single direct
            # lookup here covers `1`, `01`, and letter-suffix forms like
            # `02a` without a zfill fallback branch.
            cleaned = raw.strip()
            if cleaned in by_sequence_in_dir:
                matched = by_sequence_in_dir[cleaned]
        if matched:
            resolved.append(matched)
        else:
            unresolved.append(raw)
    return resolved, unresolved
