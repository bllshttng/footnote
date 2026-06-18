"""Deterministic blast-radius classifier (x-518f).

`classify(paths, cfg)` reads a plan's touched-surface path list against a blast
map and returns ``{"verdict": "high"|"low"|"unknown", "matched_paths": [...],
"reason": "..."}``. The verdict drives the `/target` init size modulation
(floor-up for high, cautious-down for low, fail-safe to unchanged on unknown).

The blast map has two parts, each with its own glob dialect:

* **In-footnote-repo control-plane** - the include entries of
  ``scripts/ci/loc-ratchet-manifest.yaml`` (reused so the blast map tracks the
  LOC-ratchet curation, one source of truth). Manifest entries use the
  ratchet's own prefix/star/exact semantics, NOT recursive globs:
  trailing ``/`` = directory prefix, trailing ``*`` = path-prefix glob,
  otherwise an exact path.
* **General (any repo)** - a small, high-precision default list plus the
  per-project ``high_blast_globs`` extension. These use true ``**``-recursive
  globs (``match_recursive`` from skills/blueprint, with the leading-``**/``
  fix so ``**/*.sql`` also matches a root-level ``x.sql``).

Everything is fail-safe: a missing/unparseable manifest contributes no globs, a
single bad glob is skipped (never raised), and an empty path list classifies
``unknown`` so a classifier hiccup can never block target init.
"""
from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any, Iterable

HIGH = "high"
LOW = "low"
UNKNOWN = "unknown"

# General high-blast default list. The four CATEGORIES are locked (auth /
# migrations+sql / infra+secrets / billing); the exact patterns are tunable for
# precision (Claude's Discretion 1 in the design doc).
GENERAL_HIGH_BLAST_GLOBS: tuple[str, ...] = (
    # auth
    "**/auth/**",
    "**/*auth*",
    # migrations / sql
    "**/migrations/**",
    "**/*.sql",
    "supabase/migrations/**",
    "prisma/migrations/**",
    # infra / secrets
    "**/.env*",
    "**/secrets/**",
    "**/Dockerfile",
    "**/*.tf",
    "terraform/**",
    # billing / payment
    "**/billing/**",
    "**/payment*/**",
)

_LOC_MANIFEST_RELPATH = "scripts/ci/loc-ratchet-manifest.yaml"


# --------------------------------------------------------------------------- #
# Glob matchers (two dialects)
# --------------------------------------------------------------------------- #
def _glob_to_regex(pat: str) -> str:
    """Translate an rsync-style glob (with ``**`` recursive) to a regex string.

    ``**/`` becomes ``(?:.*/)?`` so it matches zero or more leading directories
    (the blueprint ``match_recursive`` translated ``**`` -> ``.*`` which left a
    mandatory ``/`` and missed root-level files). A standalone ``**`` is
    ``.*`` (cross-segment), ``*`` is ``[^/]*`` (single segment), ``?`` is one
    non-slash char. Everything else is escaped literally.
    """
    out: list[str] = []
    i, n = 0, len(pat)
    while i < n:
        if pat.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pat.startswith("**", i):
            out.append(".*")
            i += 2
        elif pat[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pat[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(pat[i]))
            i += 1
    return "".join(out)


def match_recursive(file_path: str, pat: str) -> bool:
    """True when ``file_path`` matches the recursive glob ``pat``.

    Fail-safe: a pattern that cannot compile to a regex (a malformed glob) is
    treated as a non-match rather than raising, so one bad entry in
    ``high_blast_globs`` never aborts a classification.
    """
    try:
        return re.fullmatch(_glob_to_regex(pat), file_path) is not None
    except re.error:
        return False


def match_manifest_entry(file_path: str, entry: str) -> bool:
    """Match a loc-ratchet manifest include entry (prefix/star/exact semantics).

    Mirrors scripts/ci/loc-ratchet.sh include handling:
      trailing ``/`` -> directory prefix (path starts with the prefix)
      trailing ``*`` -> path-prefix glob (path starts with the part before ``*``)
      otherwise      -> exact path match
    """
    entry = entry.strip()
    if not entry:
        return False
    if entry.endswith("/"):
        return file_path == entry.rstrip("/") or file_path.startswith(entry)
    if entry.endswith("*"):
        return file_path.startswith(entry[:-1])
    return file_path == entry


# --------------------------------------------------------------------------- #
# Path normalization
# --------------------------------------------------------------------------- #
def normalize_path(raw: str, repo_root: str | os.PathLike[str] | None = None) -> str:
    """Normalize one ownership-map path to a repo-relative POSIX string.

    Handles ``~`` (expanduser), ``.``/``..`` segments (normpath), and absolute
    or symlinked paths: when ``repo_root`` is given, an absolute or
    repo-anchored path is resolved (following symlinks best-effort) and
    relativized to the repo root, so a path that physically resolves into a
    blast glob is not missed. Returns "" for an empty input.
    """
    s = os.path.expanduser((raw or "").strip())
    if not s:
        return ""
    p = Path(s)
    if repo_root is not None:
        root = Path(os.path.expanduser(str(repo_root)))
        anchored = p if p.is_absolute() else (root / p)
        try:
            resolved = anchored.resolve()
            root_resolved = root.resolve()
            return resolved.relative_to(root_resolved).as_posix()
        except (ValueError, OSError):
            # Resolves outside the repo (or resolve failed) - fall through to a
            # logical normalization of the original input.
            pass
    # Fallback (no repo_root, or resolve failed): strip a leading slash so an
    # absolute-looking "/hooks/..." can still match a repo-relative manifest
    # entry like "hooks/".
    return os.path.normpath(s).replace(os.sep, "/").lstrip("/")


# --------------------------------------------------------------------------- #
# Blast-map assembly
# --------------------------------------------------------------------------- #
def _load_manifest_globs(manifest_path: str | os.PathLike[str] | None) -> list[str]:
    """Read the include entries from the loc-ratchet manifest, fail-safe to [].

    Uses a tiny line-oriented parse of the ``include:`` section (the same YAML
    subset loc-ratchet.sh parses) rather than a YAML dependency, so a comment
    or a sibling section can never change behavior. Any read/parse error
    contributes no globs (the general list still carries the safety weight).
    """
    if not manifest_path:
        return []
    try:
        text = Path(manifest_path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return []
    globs: list[str] = []
    in_include = False
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        # A top-level key (no leading whitespace, ends in ':') opens a section.
        if not line.startswith((" ", "\t")) and stripped.endswith(":"):
            in_include = stripped == "include:"
            continue
        if in_include and stripped.startswith("- "):
            val = stripped[2:].strip()
            # Strip only a MATCHED surrounding quote pair (do not mangle a value
            # that legitimately starts or ends with a lone quote).
            if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                val = val[1:-1]
            globs.append(val)
    return globs


def default_manifest_path() -> str | None:
    """Best-effort resolve scripts/ci/loc-ratchet-manifest.yaml from the plugin.

    The manifest is footnote-specific; for another target repo it is read from
    the footnote plugin install. Returns None when it cannot be located (the
    classifier then leans on the general globs alone).
    """
    try:
        from fno.paths import resolve_plugin_script

        candidate = resolve_plugin_script(_LOC_MANIFEST_RELPATH)
        return str(candidate) if candidate and Path(candidate).is_file() else None
    except Exception:
        return None


def _build_glob_sets(
    cfg: Any, manifest_path: str | os.PathLike[str] | None
) -> tuple[list[str], list[str]]:
    """Return (manifest_entries, recursive_globs) for the configured map."""
    reuse = bool(getattr(cfg, "reuse_loc_manifest", True))
    extra = getattr(cfg, "high_blast_globs", None) or []
    manifest_entries: list[str] = []
    if reuse:
        path = manifest_path if manifest_path is not None else default_manifest_path()
        manifest_entries = _load_manifest_globs(path)
    recursive = list(GENERAL_HIGH_BLAST_GLOBS) + [str(g) for g in extra]
    return manifest_entries, recursive


# --------------------------------------------------------------------------- #
# Classification
# --------------------------------------------------------------------------- #
def classify(
    paths: Iterable[str],
    cfg: Any = None,
    *,
    repo_root: str | os.PathLike[str] | None = None,
    manifest_path: str | os.PathLike[str] | None = None,
) -> dict[str, Any]:
    """Classify a touched-surface path list as high / low / unknown blast.

    Verdict rules (design "The classifier"):
      * any normalized path matches any blast glob -> ``high``
      * all paths known and none match              -> ``low``
      * empty / no usable paths                     -> ``unknown``

    ``cfg`` is read via getattr (``reuse_loc_manifest``, ``high_blast_globs``)
    so a ``BlastConfig`` or any duck-typed object works; ``None`` uses defaults.
    """
    raw = [p for p in (paths or []) if str(p).strip()]
    normalized = [normalize_path(p, repo_root) for p in raw]
    pairs = [(orig, norm) for orig, norm in zip(raw, normalized) if norm]
    if not pairs:
        return {
            "verdict": UNKNOWN,
            "matched_paths": [],
            "reason": "no usable paths in the File Ownership Map",
        }

    manifest_entries, recursive = _build_glob_sets(cfg, manifest_path)

    matched: list[str] = []
    matched_reasons: list[str] = []
    for orig, norm in pairs:
        hit = next((e for e in manifest_entries if match_manifest_entry(norm, e)), None)
        if hit is None:
            hit = next((g for g in recursive if match_recursive(norm, g)), None)
        if hit is not None:
            matched.append(orig)
            matched_reasons.append(f"{orig} ~ {hit}")

    if matched:
        return {
            "verdict": HIGH,
            "matched_paths": matched,
            "reason": "high-blast surface: " + "; ".join(matched_reasons),
        }
    return {
        "verdict": LOW,
        "matched_paths": [],
        "reason": f"all {len(pairs)} path(s) outside the blast map",
    }


# --------------------------------------------------------------------------- #
# File Ownership Map parsing
# --------------------------------------------------------------------------- #
def resolve_plan_index(plan_path: str | os.PathLike[str]) -> Path:
    """Return the file whose ``## File Ownership Map`` to parse.

    A folder plan keeps its map in ``00-INDEX.md``; a single-file plan is the
    file itself. (ab-d... lean-blueprint single-doc plans are files.)
    """
    p = Path(plan_path)
    return p / "00-INDEX.md" if p.is_dir() else p


# Allow `@` so npm scoped-package paths (node_modules/@types/..., packages/@scope/...)
# are not dropped as prose.
_PATH_LIKE = re.compile(r"^[\w./*+@-]+$")


def _looks_like_path(cell: str) -> bool:
    """A first-column cell is a path candidate (not a prose description)."""
    if not cell or not _PATH_LIKE.match(cell):
        return False
    return "/" in cell or "." in cell


def parse_ownership_map(plan_text: str) -> list[str]:
    """Extract path candidates from a plan's ``## File Ownership Map`` table.

    Reads the first table column under the heading, strips backticks, and keeps
    only path-like cells (drops prose rows such as ``init-modulation test`` or
    ``fno target init writer (locate ...)``). Returns [] when the section is
    absent or holds no path-like rows (-> classify returns ``unknown``).
    """
    lines = plan_text.splitlines()
    out: list[str] = []
    in_section = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("#"):
            # Any heading toggles the section: enter on the ownership map,
            # leave on the next heading of equal-or-higher level.
            heading = stripped.lstrip("#").strip().lower()
            in_section = heading.startswith("file ownership map")
            continue
        if not in_section:
            continue
        if not stripped.startswith("|"):
            continue
        cells = [c.strip() for c in stripped.strip("|").split("|")]
        if not cells:
            continue
        first = cells[0].strip().strip("`").strip()
        low = first.lower()
        if low in ("file", "files", "") or set(first) <= set("-: "):
            continue  # header or separator row
        if _looks_like_path(first):
            out.append(first)
    return out
