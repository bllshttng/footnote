"""Megatron mission manifest schema + parser.

Manifests live at ``~/.fno/fleet/{slug}/00-INDEX.md`` and follow a
markdown-with-YAML-frontmatter shape. The parser only consumes the
frontmatter; the markdown body is the human-facing prose that explains
the mission to the operator.

Locked Decision (per design doc): convention-not-strict-parser. We
accept conformant manifests, raise targeted ``ManifestError``s on
known boundary failures, and let the validator (Task 2.3) handle the
semantic checks. Unknown frontmatter keys are preserved on the dataclass
under ``extra`` so future fields don't require parser changes.
"""
from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

import yaml


class ManifestError(Exception):
    """Raised when a manifest cannot be parsed into a typed structure."""


@dataclass
class Project:
    name: str
    body: str
    kind: str = "heads-up"
    extra: dict = field(default_factory=dict)


@dataclass
class Wave:
    wave: int
    projects: list[Project] = field(default_factory=list)
    tasks: list[str] = field(default_factory=list)
    mode: str = "sequential"
    wave_type: Optional[str] = None
    failure_policy: str = "block"
    extra: dict = field(default_factory=dict)


@dataclass
class Budget:
    cost_cap_usd_per_mission: Optional[float] = None
    cost_cap_usd_per_project: Optional[float] = None


@dataclass
class Manifest:
    mission_id: str
    mission_type: str
    waves: list[Wave]
    budget: Budget = field(default_factory=Budget)
    # Plan B (Spec 4, ab-0e5a921e): optional combo: key in frontmatter sets
    # routing for spawned megawalk subprocesses. CLI --combo wins over this
    # at runtime (see cli.py::cmd_run resolution). None means "no combo;
    # downstream resolver falls back to per-agent pin / settings active /
    # active provider".
    combo: Optional[str] = None
    extra: dict = field(default_factory=dict)


_FRONTMATTER_DELIM = "---"


def _split_frontmatter(text: str) -> tuple[str, int]:
    """Return (frontmatter_yaml_text, start_line_in_file) or raise.

    The start line is 1-indexed and points at the line AFTER the opening
    ``---`` delimiter so YAML errors can be reported with file-relative
    line numbers.
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != _FRONTMATTER_DELIM:
        raise ManifestError("manifest must start with '---' frontmatter delimiter")
    end_index = None
    for i in range(1, len(lines)):
        if lines[i].strip() == _FRONTMATTER_DELIM:
            end_index = i
            break
    if end_index is None:
        raise ManifestError("manifest frontmatter has no closing '---' delimiter")
    return "\n".join(lines[1:end_index]), 2


def _coerce_project(raw: Any, wave_index: int, project_index: int) -> Project:
    if not isinstance(raw, dict):
        raise ManifestError(
            f"wave {wave_index} project {project_index}: expected dict, got {type(raw).__name__}"
        )
    name = raw.get("name")
    if not isinstance(name, str) or not name:
        raise ManifestError(
            f"wave {wave_index} project {project_index}: missing 'name'"
        )
    body = raw.get("body", "")
    if body is None:
        body = ""
    if not isinstance(body, str):
        raise ManifestError(
            f"wave {wave_index} project {name}: 'body' must be a string"
        )
    kind = raw.get("kind", "heads-up")
    if not isinstance(kind, str):
        raise ManifestError(
            f"wave {wave_index} project {name}: 'kind' must be a string"
        )
    extra = {k: v for k, v in raw.items() if k not in ("name", "body", "kind")}
    return Project(name=name, body=body, kind=kind, extra=extra)


def _coerce_wave(raw: Any, wave_index: int) -> Wave:
    if not isinstance(raw, dict):
        raise ManifestError(f"wave {wave_index}: expected dict, got {type(raw).__name__}")
    wave_num = raw.get("wave", wave_index + 1)
    if not isinstance(wave_num, int):
        raise ManifestError(f"wave {wave_index}: 'wave' must be an int")
    projects_raw = raw.get("projects") or []
    tasks_raw = raw.get("tasks") or []
    if not isinstance(projects_raw, list):
        raise ManifestError(f"wave {wave_num}: 'projects' must be a list")
    if not isinstance(tasks_raw, list):
        raise ManifestError(f"wave {wave_num}: 'tasks' must be a list")
    projects = [
        _coerce_project(p, wave_num, i) for i, p in enumerate(projects_raw)
    ]
    tasks = [str(t) for t in tasks_raw]
    mode = raw.get("mode", "sequential")
    if mode not in ("sequential", "parallel"):
        raise ManifestError(
            f"wave {wave_num}: 'mode' must be 'sequential' or 'parallel' (got {mode!r})"
        )
    failure_policy = raw.get("failure_policy", "block")
    if failure_policy != "block":
        # Spec scope locks failure_policy to 'block' for v0; surface a
        # targeted error so misconfigured manifests don't run silently.
        raise ManifestError(
            f"wave {wave_num}: only failure_policy: block is supported in v0 "
            f"(got {failure_policy!r})"
        )
    extra = {
        k: v for k, v in raw.items()
        if k not in ("wave", "projects", "tasks", "mode", "wave_type", "failure_policy")
    }
    return Wave(
        wave=wave_num,
        projects=projects,
        tasks=tasks,
        mode=mode,
        wave_type=raw.get("wave_type"),
        failure_policy=failure_policy,
        extra=extra,
    )


def _coerce_budget(raw: Any) -> Budget:
    if raw is None:
        return Budget()
    if not isinstance(raw, dict):
        raise ManifestError(f"'budget' must be a dict (got {type(raw).__name__})")
    cap_mission = raw.get("cost_cap_usd_per_mission")
    cap_project = raw.get("cost_cap_usd_per_project")
    return Budget(
        cost_cap_usd_per_mission=float(cap_mission) if cap_mission is not None else None,
        cost_cap_usd_per_project=float(cap_project) if cap_project is not None else None,
    )


def load_manifest(path: Path | str) -> Manifest:
    """Parse a manifest file into a typed ``Manifest`` dataclass.

    Raises ``ManifestError`` on missing required fields, malformed YAML,
    shape errors, OR a non-UTF-8 file. The validator (validate_manifest)
    handles semantic checks like wave caps and chain-of-research detection.
    """
    path = Path(path)
    if not path.exists():
        raise ManifestError(f"manifest not found: {path}")
    try:
        raw_text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise ManifestError(f"could not read manifest {path}: {exc}") from exc
    return _parse_manifest_text(raw_text)


def load_manifest_and_sha(path: Path | str) -> tuple[Manifest, str]:
    """Atomic load + raw-bytes sha256 from a single file read.

    Solves the TOCTOU window between ``load_manifest`` (text decode) and
    ``manifest_sha256`` (bytes hash) - both derive from the same bytes
    snapshot so a mutation between two separate reads cannot baseline a
    different version than the one being dispatched.
    """
    path = Path(path)
    if not path.exists():
        raise ManifestError(f"manifest not found: {path}")
    try:
        raw_bytes = path.read_bytes()
    except OSError as exc:
        raise ManifestError(f"could not read manifest {path}: {exc}") from exc
    sha = hashlib.sha256(raw_bytes).hexdigest()
    try:
        raw_text = raw_bytes.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ManifestError(f"manifest {path} is not valid UTF-8: {exc}") from exc
    return _parse_manifest_text(raw_text), sha


def _parse_manifest_text(raw_text: str) -> Manifest:
    fm_text, fm_start = _split_frontmatter(raw_text)
    try:
        data = yaml.safe_load(fm_text) or {}
    except yaml.YAMLError as exc:
        line_hint = ""
        mark = getattr(exc, "problem_mark", None)
        if mark is not None:
            line_hint = f" at line {mark.line + fm_start}"
        raise ManifestError(f"YAML parse error{line_hint}: {exc}") from exc

    if not isinstance(data, dict):
        raise ManifestError("manifest frontmatter must be a YAML mapping")

    mission_type = data.get("mission_type")
    if mission_type is None:
        raise ManifestError("missing required frontmatter field: mission_type")
    mission_id = data.get("mission_id")
    if mission_id is None:
        raise ManifestError("missing required frontmatter field: mission_id")

    waves_raw = data.get("waves") or []
    if not isinstance(waves_raw, list):
        raise ManifestError("'waves' must be a list")
    if not waves_raw:
        raise ManifestError("manifest must declare at least one wave")
    waves = [_coerce_wave(w, i) for i, w in enumerate(waves_raw)]

    budget = _coerce_budget(data.get("budget"))

    # Plan B (Spec 4): optional combo: key in manifest frontmatter.
    # Only validates type here; existence-of-combo check happens in cmd_run
    # (so a manifest can be authored before the combo is created).
    combo_raw = data.get("combo")
    combo: Optional[str] = None
    if combo_raw is not None:
        if not isinstance(combo_raw, str) or not combo_raw.strip():
            raise ManifestError(
                f"'combo' must be a non-empty string when present "
                f"(got {type(combo_raw).__name__})"
            )
        combo = combo_raw.strip()

    extra = {
        k: v for k, v in data.items()
        if k not in ("mission_type", "mission_id", "waves", "budget", "combo")
    }
    return Manifest(
        mission_id=str(mission_id),
        mission_type=str(mission_type),
        waves=waves,
        budget=budget,
        combo=combo,
        extra=extra,
    )


def manifest_sha256(path: Path | str) -> str:
    """Compute the raw-bytes sha256 of a manifest file.

    Raw bytes (not YAML-canonicalized): the operator's contract is "the
    file is immutable," so reordering keys or adding whitespace counts as
    mutation.

    Raises ManifestError when the file is unreadable; never returns empty.
    """
    path = Path(path)
    if not path.exists():
        raise ManifestError(f"manifest_sha256: file not found: {path}")
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError as exc:
        raise ManifestError(f"manifest_sha256: could not read {path}: {exc}") from exc
