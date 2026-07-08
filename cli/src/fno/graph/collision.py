"""Plan collision detection: file-overlap as a proxy for plan overlap.

Pure-Python primitive consumed by:
  * ``fno backlog collisions check`` (cli.py)
  * ``fno backlog triage health`` (triage.py)
  * ``/spec`` skill at intake time (steps 3a / 11a)

Public API:
    Collision (dataclass)            - one collision record between two plans
    parse_files_to_modify(plan_path) - normalize files from a plan's table
    find_collisions(...)             - compare a candidate against all pending
    find_acknowledged_collisions(...) - reconcile acknowledged + shipped pairs
    _load_thresholds()               - resolve severity thresholds from settings

Severity model (configurable; see ``_load_thresholds``):
    high   -> shared count >= high_count OR shared/min(set_sizes) >= high_ratio
    medium -> shared count == medium_count OR shared/max(set_sizes) >= medium_ratio
    low    -> shared count == 1 (no further overlap signals)

Action inference (deterministic):
    candidate_files subset other_files -> absorb (older plan covers everything)
    other_files     subset candidate    -> supersede (new plan covers more)
    shared >= 50% of both sides         -> absorb if other is older + has more, else coordinate
    severity == low                     -> coordinate with split rationale appended
    everything else                     -> coordinate (both ship; rebase second to land)
"""
from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Literal, TypedDict

Severity = Literal["low", "medium", "high"]
Action = Literal["coordinate", "absorb", "supersede"]
ResolvedStatus = Literal["done", "merged"]


class CollisionThresholds(TypedDict):
    """Severity scoring thresholds. Keys are stringly-typed elsewhere; this
    TypedDict catches typos at the call site instead of producing a KeyError
    or silent miss inside ``_classify``."""

    high_count: float
    high_ratio: float
    medium_count: float
    medium_ratio: float


# Default thresholds (v1 heuristics; tunable via settings.yaml). Derived from
# the Pydantic CollisionThresholdsBlock so the model is the single source of
# truth: this is the model's defaults, not an independent copy.
def _default_thresholds() -> CollisionThresholds:
    from fno.config import CollisionThresholdsBlock

    return CollisionThresholdsBlock().model_dump()  # type: ignore[return-value]


DEFAULT_THRESHOLDS: CollisionThresholds = _default_thresholds()


@dataclass(frozen=True)
class Collision:
    with_node_id: str
    with_node_title: str
    with_plan_path: str
    shared_files: list[str]
    candidate_only_files: list[str]
    other_only_files: list[str]
    severity: Severity
    recommended_action: Action
    rationale: str
    # Hidden tie-breaker for sorting; populated when find_collisions builds
    # the record. Not part of the user-visible payload.
    _other_created_at: str = field(default="", repr=False, compare=False)


# ---------------------------------------------------------------------------
# File-table parser
# ---------------------------------------------------------------------------

# Recognized headings under which the file list lives. Plans in this repo
# settled on "Files to Modify"; we accept a couple of synonyms in case a
# templating tool ever drifts. Folder plans (00-INDEX.md) sometimes use
# "Files Touched" instead.
_FILE_HEADINGS = (
    "files to modify",
    "files to change",
    "files touched",
    "files",
)

# Trailing line suffix on a code reference: `path/to/file.py:42` should
# normalize to `path/to/file.py`.
_LINE_SUFFIX_RE = re.compile(r":\d+(-\d+)?$")


def _strip_path(raw: str) -> str:
    """Normalize a file cell from a markdown table.

    Strips backticks, leading/trailing whitespace, tilde paths
    (``~/.fno/settings.yaml`` -> ``~/.fno/settings.yaml``,
    deliberately preserved), parenthetical annotations like ``(template)``,
    and trailing line suffixes.
    """
    s = raw.strip()
    # Drop trailing parenthetical annotations: `path (template)` -> `path`.
    s = re.sub(r"\s*\([^)]*\)\s*$", "", s)
    # Strip backticks and surrounding whitespace.
    s = s.replace("`", "").strip()
    # Strip line suffix.
    s = _LINE_SUFFIX_RE.sub("", s)
    return s


def _is_file_table_row(line: str) -> bool:
    """True if the line looks like a markdown table row with a file in col 1."""
    if not line.startswith("|"):
        return False
    # The header separator row: `| --- | --- |` or `|------|------|`
    if re.match(r"^\|[\s:|-]+\|[\s:|-]*$", line):
        return False
    return True


def _extract_files_from_section(text: str, heading_index: int) -> set[str]:
    """Walk lines after a recognized heading until the next heading or EOF.

    Returns the set of normalized file paths from column 1 of any markdown
    tables encountered. A non-table line is allowed (prose between heading
    and table) but a new ``##`` heading terminates the search.
    """
    lines = text.splitlines()
    files: set[str] = set()
    saw_header = False
    saw_separator = False
    for line in lines[heading_index + 1:]:
        stripped = line.strip()
        if stripped.startswith("##"):
            break
        if not _is_file_table_row(stripped):
            # Reset table parsing once we leave a table; another table can
            # still appear later under the same heading.
            if saw_header and saw_separator and not stripped:
                # Blank line after a table - keep going; another table may
                # follow under the same heading.
                saw_header = False
                saw_separator = False
            continue
        # First valid table row is the header.
        if not saw_header:
            saw_header = True
            continue
        # Second row should be the separator.
        if saw_header and not saw_separator:
            # Tolerate plans that omit the separator (rare but cheap to allow).
            if re.match(r"^\|[\s:|-]+\|", stripped):
                saw_separator = True
                continue
            saw_separator = True  # treat as separator and fall through
        # Data row: split, take column 1.
        parts = [p for p in stripped.strip("|").split("|")]
        if not parts:
            continue
        cell = _strip_path(parts[0])
        if cell:
            files.add(cell)
    return files


def parse_files_to_modify(plan_path: Path) -> set[str]:
    """Extract file paths from the 'Files to Modify' table of a plan.

    Supports:
      - Single-file quick plans: read the plan file directly.
      - Folder plans: read 00-INDEX.md plus every NN-*.md phase file.

    Returns a normalized set of repo-relative paths. Strips backticks, line
    suffixes (``:123``), parenthetical annotations, and folder slashes.
    Missing or unreadable plan files return an empty set; the caller treats
    "no parseable files" as "cannot collide" so a malformed plan never
    spurious-blocks a new spec.
    """
    if not plan_path.exists():
        return set()

    files: set[str] = set()
    if plan_path.is_dir():
        # Folder plan: read 00-INDEX.md + every NN-*.md file.
        index = plan_path / "00-INDEX.md"
        if index.exists():
            files |= _scan_one(index)
        for child in sorted(plan_path.iterdir()):
            if not child.is_file() or not child.suffix == ".md":
                continue
            if child.name == "00-INDEX.md":
                continue
            if not re.match(r"^\d", child.name):
                continue
            files |= _scan_one(child)
    else:
        files |= _scan_one(plan_path)
    return files


def _scan_one(path: Path) -> set[str]:
    """Read a single markdown file and pull out its files-to-modify set."""
    try:
        text = path.read_text()
    except OSError as exc:
        print(f"Warning: collision parser cannot read {path}: {exc}", file=sys.stderr)
        return set()

    found: set[str] = set()
    lines = text.splitlines()
    for idx, line in enumerate(lines):
        stripped = line.strip()
        if not stripped.startswith("##"):
            continue
        heading_text = stripped.lstrip("#").strip().lower()
        if any(heading_text.startswith(h) for h in _FILE_HEADINGS):
            found |= _extract_files_from_section(text, idx)
    return found


# ---------------------------------------------------------------------------
# Threshold loading
# ---------------------------------------------------------------------------


def _load_thresholds(
    project_settings: Path | None = None,
    user_settings: Path | None = None,
) -> dict[str, float]:
    """Resolve severity thresholds from the model.

    Single source of truth is the Pydantic ``CollisionThresholdsBlock`` read via
    ``load_settings().collision.severity_thresholds``; a malformed or
    negative per-key value degrades to the model default (with a WARNING) inside
    the model's own sanitizer, so this can never break the load. Returns a fresh
    dict each call so callers can mutate freely.

    ``project_settings`` / ``user_settings`` are honored for callers (tests)
    pointing at explicit temp files: they are merged through the SAME model via
    ``config.settings_from_files`` (project beats user). No private hand-parser
    or per-module default set remains.
    """
    from fno.config import load_settings, settings_from_files

    try:
        if project_settings is None and user_settings is None:
            block = load_settings().collision.severity_thresholds
        else:
            explicit = [p for p in (project_settings, user_settings) if p is not None]
            block = settings_from_files(explicit).collision.severity_thresholds
        return dict(block.model_dump())
    except Exception as exc:
        # Fail-open: a malformed UNRELATED setting must not abort collision
        # detection; fall back to default thresholds. The old loader only read
        # config.collision.severity_thresholds.
        print(
            f"Warning: collision thresholds: settings validation failed ({exc}); "
            "using defaults",
            file=sys.stderr,
        )
        return dict(DEFAULT_THRESHOLDS)


# ---------------------------------------------------------------------------
# Collision detection
# ---------------------------------------------------------------------------


_SEVERITY_ORDER = {"high": 0, "medium": 1, "low": 2}


_repo_root_cache: tuple[bool, Path] | None = None


def _find_repo_root() -> Path:
    """Best-effort repo root: ``git rev-parse --show-toplevel`` then CWD.

    Used to resolve relative ``plan_path`` entries that were stored as
    repo-relative strings (the canonical shape on intake). Falls back to
    the current working directory when not in a git checkout - tests
    inject a tmp_path-rooted shape so the fallback is fine there.

    Memoized for the lifetime of the process. ``cmd_health`` calls
    ``find_collisions`` once per pending node; without the cache that is N
    subprocess invocations of ``git rev-parse``. The fallback path emits a
    one-shot stderr warning so a misconfigured environment is visible.
    """
    global _repo_root_cache
    if _repo_root_cache is not None:
        _, cached = _repo_root_cache
        return cached

    import subprocess

    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if out:
            root = Path(out)
            _repo_root_cache = (True, root)
            return root
    except (subprocess.CalledProcessError, FileNotFoundError):
        pass

    fallback = Path.cwd()
    _repo_root_cache = (False, fallback)
    print(
        "Warning: git rev-parse failed; resolving plan paths against cwd "
        f"({fallback}). Cross-collision detection may produce false negatives.",
        file=sys.stderr,
    )
    return fallback


def _resolve_plan_path(plan_path: str, repo_root: Path) -> Path:
    """Resolve a stored ``plan_path`` to an absolute Path.

    Handles three shapes seen on the live graph:
      1. Absolute (`/abs/path/to/plan.md`) - returned as-is.
      2. ``~`` expanded (`~/vault/plans/...`) - tilde-expanded.
      3. Repo-relative (`internal/fno/plans/...`) - joined to repo_root.
    """
    if not plan_path:
        return Path("")
    if plan_path.startswith("~"):
        return Path(plan_path).expanduser()
    p = Path(plan_path)
    if p.is_absolute():
        return p
    return repo_root / plan_path


def _is_pending_for_collision(entry: dict) -> bool:
    """A node is collision-eligible if it has a plan and is not done/deferred/superseded."""
    if entry.get("type") == "roadmap":
        return False
    if entry.get("completed_at"):
        return False
    status = entry.get("_status", "ready")
    if status in ("done", "deferred", "superseded"):
        return False
    if not entry.get("plan_path"):
        return False
    return True


def _classify(
    candidate: set[str],
    other: set[str],
    thresholds: dict[str, float],
) -> tuple[Severity, list[str]]:
    """Return (severity, sorted-shared-files) for two file sets."""
    shared = sorted(candidate & other)
    if not shared:
        return ("low", [])  # caller filters empty
    n_shared = len(shared)
    min_set = min(len(candidate), len(other)) or 1
    max_set = max(len(candidate), len(other)) or 1
    high_count = thresholds["high_count"]
    high_ratio = thresholds["high_ratio"]
    medium_count = thresholds["medium_count"]
    medium_ratio = thresholds["medium_ratio"]

    if n_shared >= high_count or (n_shared / min_set) >= high_ratio:
        return ("high", shared)
    if n_shared >= medium_count or (n_shared / max_set) >= medium_ratio:
        return ("medium", shared)
    return ("low", shared)


def _infer_action(
    candidate: set[str],
    other: set[str],
    other_created_at: str,
    candidate_created_at: str,
    severity: Severity,
) -> Action:
    """Infer the recommended action from set relationships and ages."""
    if candidate and candidate < other:  # strict subset
        return "absorb"
    if other and other < candidate:
        return "supersede"
    shared = candidate & other
    min_set = min(len(candidate), len(other)) or 1
    if shared and (len(shared) / min_set) >= 0.5:
        # Both sides share half-or-more of their smaller side. Prefer
        # absorption into the older plan unless they were created in the
        # same hour (in which case coordination is the safer call - we
        # don't have enough signal to pick which one wins).
        if other_created_at and candidate_created_at:
            if other_created_at < candidate_created_at:
                return "absorb"
        return "coordinate"
    if severity == "low":
        # Low severity gets a coordinate recommendation; the rationale
        # appended to the message points at "split into a shared dependency"
        # as the cleaner long-term move.
        return "coordinate"
    return "coordinate"


def _build_rationale(
    candidate: set[str],
    other: set[str],
    shared: list[str],
    other_id: str,
    severity: Severity,
    action: Action,
) -> str:
    """Produce a human-readable rationale string."""
    files_preview = ", ".join(shared[:3])
    if len(shared) > 3:
        files_preview += f", ... ({len(shared)} total)"
    base = (
        f"{len(shared)} shared files ({files_preview}) "
        f"of {len(candidate)} in this plan and {len(other)} in {other_id}"
    )
    if action == "absorb":
        return base + f"; {other_id} has wider scope, so absorbing your changes into {other_id} is the cleanest path."
    if action == "supersede":
        return base + f"; this plan covers everything {other_id} touches plus more, so superseding {other_id} is the cleanest path."
    if severity == "low":
        return base + "; consider splitting the overlap into a shared dependency rather than two parallel touches."
    return base + "; both plans can ship if the second one rebases on the first."


def find_collisions(
    candidate_plan_path: Path,
    graph: Iterable[dict],
    *,
    self_id: str | None = None,
    thresholds: dict[str, float] | None = None,
) -> list[Collision]:
    """Compare candidate plan against all pending plans on the graph.

    self_id: optional node ID of the candidate plan itself, to exclude
    self-collisions when the candidate has already been intaked.

    thresholds: optional override; when None, ``_load_thresholds()`` resolves
    from project + user + default layers. Tests inject a fixed dict to avoid
    touching the real settings.

    Returns collisions sorted by severity descending (high first), then by
    node id ASC for stable output.
    """
    if thresholds is None:
        thresholds = _load_thresholds()

    out: list[Collision] = []
    candidate_files = parse_files_to_modify(candidate_plan_path)
    if not candidate_files:
        return out

    # Resolve candidate created_at if it's already on the graph (for action
    # inference's age tie-breaker). Default to empty string when absent.
    candidate_created = ""
    if self_id:
        for entry in graph:
            if entry.get("id") == self_id:
                candidate_created = entry.get("created_at") or ""
                break

    repo_root = _find_repo_root()
    for entry in graph:
        if not _is_pending_for_collision(entry):
            continue
        if self_id and entry.get("id") == self_id:
            continue
        other_plan = entry.get("plan_path")
        if not other_plan:
            continue
        other_path = _resolve_plan_path(other_plan, repo_root)
        other_files = parse_files_to_modify(other_path)
        if not other_files:
            continue
        shared = candidate_files & other_files
        if not shared:
            continue
        severity, shared_sorted = _classify(candidate_files, other_files, thresholds)
        action = _infer_action(
            candidate_files,
            other_files,
            entry.get("created_at") or "",
            candidate_created,
            severity,
        )
        rationale = _build_rationale(
            candidate_files,
            other_files,
            shared_sorted,
            entry.get("id", "<unknown>"),
            severity,
            action,
        )
        out.append(
            Collision(
                with_node_id=entry.get("id", "<unknown>"),
                with_node_title=entry.get("title", ""),
                with_plan_path=str(other_plan),
                shared_files=shared_sorted,
                candidate_only_files=sorted(candidate_files - other_files),
                other_only_files=sorted(other_files - candidate_files),
                severity=severity,
                recommended_action=action,
                rationale=rationale,
                _other_created_at=entry.get("created_at") or "",
            )
        )

    out.sort(key=lambda c: (_SEVERITY_ORDER[c.severity], c.with_node_id))
    return out


# ---------------------------------------------------------------------------
# Acknowledged-collision reconciliation
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class AcknowledgedReconciliation:
    """A node that acknowledged a collision which has now resolved.

    The user accepted the collision at spec time; the colliding plan has
    since shipped. Triage health surfaces this so the user can verify the
    conflict actually resolved cleanly, or notice that it didn't.
    """

    node_id: str
    node_title: str
    resolved_via: str  # the colliding node's id
    resolved_via_title: str
    resolved_via_status: ResolvedStatus


def find_acknowledged_collisions(graph: Iterable[dict]) -> list[AcknowledgedReconciliation]:
    """Find nodes whose acknowledged collisions have since shipped.

    A node carries ``collisions_acknowledged: list[str]`` of ab-IDs the user
    accepted at spec time. When any of those referenced nodes have
    ``_status == "done"`` (or a ``merge_status == "merged"``), surface a
    reconciliation entry so the user can verify the conflict resolved.

    The ``__skipped_check__`` sentinel (written by ``--no-collision-check``)
    is ignored because there is no specific other node to reconcile against.
    """
    entries = list(graph)
    by_id = {e["id"]: e for e in entries if isinstance(e.get("id"), str)}
    out: list[AcknowledgedReconciliation] = []
    for e in entries:
        ack = e.get("collisions_acknowledged") or []
        if not isinstance(ack, list):
            continue
        for ref_id in ack:
            if not isinstance(ref_id, str) or ref_id == "__skipped_check__":
                continue
            other = by_id.get(ref_id)
            if other is None:
                continue
            status = other.get("_status", "")
            merge_status = other.get("merge_status", "")
            if status == "done" or merge_status == "merged":
                out.append(
                    AcknowledgedReconciliation(
                        node_id=e["id"],
                        node_title=e.get("title", ""),
                        resolved_via=ref_id,
                        resolved_via_title=other.get("title", ""),
                        resolved_via_status="merged" if merge_status == "merged" else status,
                    )
                )
    out.sort(key=lambda r: (r.node_id, r.resolved_via))
    return out
