"""AgentContext + load_agent_context() - state-loading for `fno agent`.

Detects three operating layers independently:

    fleet    glob ~/.fno/fleet/*/00-INDEX.md, find a running mission
             whose projects: map references the current project_root
    walker   .fno/megawalk-state.md
    session  .fno/target-state.md (preferred); .fno/session-state.md (fallback)

YAML parse failure -> one retry after 50ms -> raises MalformedStateError.
The CLI layer catches MalformedStateError and exits rc=2.
"""
from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Literal, Optional

import yaml

Provider = Literal["claude", "gemini", "codex"]
SessionKind = Literal["target", "session", "override"]
FleetStatus = Literal["running", "paused"]


class MissingStateFileOverrideError(Exception):
    """Raised when --state-file points at a path that does not exist."""

    def __init__(self, path: Path) -> None:
        super().__init__(f"--state-file override does not exist: {path}")
        self.path = path

YAML_RETRY_DELAY_SECONDS = 0.05


class MalformedStateError(Exception):
    """Raised when a state file's YAML cannot be parsed after one retry."""

    def __init__(self, path: Path, original: Exception) -> None:
        super().__init__(f"malformed state file: {path}: {original}")
        self.path = path
        self.original = original


@dataclass(frozen=True)
class AgentOptions:
    """Shared options carried via `typer.Context.obj`. Frozen because the
    settings bag is written once in agent_main() and read everywhere else."""

    json_output: bool = False
    state_file: Optional[Path] = None
    no_walker: bool = False
    no_fleet: bool = False


@dataclass
class FleetState:
    mission_id: str
    title: Optional[str]
    status: FleetStatus
    wave_current: Optional[int]
    wave_total: Optional[int]
    path: Path


@dataclass
class WalkerState:
    session_id: Optional[str]
    phase: Optional[str]
    in_flight: int = 0
    done: int = 0
    path: Optional[Path] = None


@dataclass
class SessionState:
    session_id: Optional[str]
    phase: Optional[str]
    status: Optional[str]
    pr_number: Optional[int] = None
    raw: Dict[str, Any] = field(default_factory=dict)
    path: Optional[Path] = None
    kind: SessionKind = "target"

    def gate(self, key: str) -> Any:
        """Read a raw frontmatter field (e.g. a `*_passed` gate). Central
        accessor so cli.py doesn't reach into `raw` directly."""
        return self.raw.get(key)


@dataclass
class AgentContext:
    project_root: Path
    provider: Provider
    fleet: Optional[FleetState] = None
    walker: Optional[WalkerState] = None
    session: Optional[SessionState] = None
    detected_paths: List[Path] = field(default_factory=list)
    warnings: List[str] = field(default_factory=list)


def _detect_project_root(warnings: List[str]) -> Path:
    """git rev-parse --show-toplevel; fall back to cwd with a warning."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            return Path(result.stdout.strip())
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    warnings.append("not a git repo, using cwd")
    return Path.cwd()


def _detect_provider() -> Provider:
    """Mirror init-target-state.sh's detect_provider; default 'claude'."""
    if os.environ.get("CODEX_PLUGIN_ROOT"):
        return "codex"
    if os.environ.get("GEMINI_PROJECT_DIR"):
        return "gemini"
    if os.environ.get("CLAUDE_PLUGIN_ROOT"):
        return "claude"
    return "claude"


def _parse_yaml_with_retry(path: Path) -> Dict[str, Any]:
    """Read + parse the file's YAML frontmatter. One retry on parse failure.

    Returns the parsed frontmatter dict (empty dict if no frontmatter).
    Raises MalformedStateError if both attempts fail.
    """
    last_exc: Optional[Exception] = None
    for attempt in range(2):
        try:
            text = path.read_text(encoding="utf-8")
            return _extract_frontmatter(text)
        except (yaml.YAMLError, ValueError, OSError) as exc:
            # OSError covers: path is a directory, permission denied, file
            # removed between stat() and read_text(). The CLI converts
            # MalformedStateError to rc=2 - a bad-shape filesystem must
            # never escape as an uncaught Python exception.
            last_exc = exc
            if attempt == 0:
                time.sleep(YAML_RETRY_DELAY_SECONDS)
                continue
    assert last_exc is not None
    raise MalformedStateError(path, last_exc)


def _extract_frontmatter(text: str) -> Dict[str, Any]:
    """Parse YAML frontmatter; return {} if no delimiters present.

    Raises ValueError if frontmatter is opened but not closed (unterminated).
    Raises yaml.YAMLError if the YAML inside is invalid.
    """
    if not text.startswith("---"):
        return {}
    rest = text[3:].lstrip("\n")
    end_marker = "\n---"
    idx = rest.find(end_marker)
    if idx == -1:
        raise ValueError("unterminated frontmatter (no closing ---)")
    yaml_block = rest[:idx]
    parsed = yaml.safe_load(yaml_block)
    return parsed if isinstance(parsed, dict) else {}


def _load_fleet_state(
    project_root: Path, warnings: List[str], detected: List[Path]
) -> Optional[FleetState]:
    """Glob ~/.fno/fleet/*/00-INDEX.md; find a mission referencing project_root."""
    from fno import paths as _paths
    fleet_root = _paths.fleet_dir()
    if not fleet_root.exists():
        return None
    abs_root = str(project_root.resolve())
    for index_file in sorted(fleet_root.glob("*/00-INDEX.md")):
        try:
            data = _parse_yaml_with_retry(index_file)
        except MalformedStateError as exc:
            warnings.append(f"fleet: malformed {exc.path.name}, skipping")
            continue
        status = str(data.get("status", "")).lower()
        if status not in {"running", "paused"}:
            continue
        projects = data.get("projects") or {}
        if not isinstance(projects, dict):
            continue
        for proj_value in projects.values():
            proj_path = ""
            if isinstance(proj_value, dict):
                proj_path = str(proj_value.get("cwd") or proj_value.get("path") or "")
            elif isinstance(proj_value, str):
                proj_path = proj_value
            if not proj_path:
                continue
            try:
                if str(Path(proj_path).expanduser().resolve()) == abs_root:
                    detected.append(index_file)
                    return FleetState(
                        mission_id=str(data.get("mission_id") or index_file.parent.name),
                        title=data.get("title"),
                        status=status,
                        wave_current=_int_or_none(data.get("wave_current")),
                        wave_total=_int_or_none(data.get("wave_total")),
                        path=index_file,
                    )
            except (OSError, RuntimeError):
                continue
    return None


def _load_walker_state(
    project_root: Path, warnings: List[str], detected: List[Path]
) -> Optional[WalkerState]:
    path = project_root / ".fno" / "megawalk-state.md"
    if not path.exists() or path.stat().st_size == 0:
        return None
    try:
        data = _parse_yaml_with_retry(path)
    except MalformedStateError as exc:
        warnings.append(f"walker: malformed {exc.path.name}")
        return None
    detected.append(path)
    return WalkerState(
        session_id=_str_or_none(data.get("session_id")),
        phase=_str_or_none(data.get("current_phase") or data.get("phase")),
        in_flight=_int_or_zero(data.get("in_flight")),
        done=_int_or_zero(data.get("done")),
        path=path,
    )


def _load_session_state(
    project_root: Path,
    state_file_override: Optional[Path],
    warnings: List[str],
    detected: List[Path],
) -> Optional[SessionState]:
    if state_file_override is not None:
        if not state_file_override.exists():
            # Explicit user request - the typo deserves a hard error, not a
            # silent degradation to "no session found".
            raise MissingStateFileOverrideError(state_file_override)
        path = state_file_override
        kind = "override"
    else:
        target = project_root / ".fno" / "target-state.md"
        session = project_root / ".fno" / "session-state.md"
        target_present = target.exists() and target.stat().st_size > 0
        session_present = session.exists() and session.stat().st_size > 0
        if target_present and session_present:
            warnings.append(
                "both .fno/target-state.md and .fno/session-state.md present; using target"
            )
            path, kind = target, "target"
        elif target_present:
            path, kind = target, "target"
        elif session_present:
            path, kind = session, "session"
        else:
            return None
    data = _parse_yaml_with_retry(path)  # raises MalformedStateError on failure
    detected.append(path)
    return SessionState(
        session_id=_str_or_none(data.get("session_id")),
        phase=_str_or_none(data.get("current_phase") or data.get("phase")),
        status=_str_or_none(data.get("status")),
        pr_number=_int_or_none(data.get("pr_number")),
        raw=data,
        path=path,
        kind=kind,
    )


def load_agent_context(
    state_file_override: Optional[Path] = None,
    project_root_override: Optional[Path] = None,
) -> AgentContext:
    """Build AgentContext by detecting fleet, walker, session layers independently.

    Layers are independent: failure in one (or absence) does not block the others.
    Session-layer YAML failure raises MalformedStateError; fleet/walker failures
    log a warning and degrade to None.
    """
    warnings: List[str] = []
    detected: List[Path] = []
    project_root = (
        project_root_override
        if project_root_override is not None
        else _detect_project_root(warnings)
    )
    provider = _detect_provider()
    fleet = _load_fleet_state(project_root, warnings, detected)
    walker = _load_walker_state(project_root, warnings, detected)
    session = _load_session_state(project_root, state_file_override, warnings, detected)
    return AgentContext(
        project_root=project_root,
        provider=provider,
        fleet=fleet,
        walker=walker,
        session=session,
        detected_paths=detected,
        warnings=warnings,
    )


def _str_or_none(value: Any) -> Optional[str]:
    if value is None:
        return None
    s = str(value).strip()
    return s if s and s.lower() not in {"null", "none", ""} else None


def _int_or_none(value: Any) -> Optional[int]:
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _int_or_zero(value: Any) -> int:
    n = _int_or_none(value)
    return n if n is not None else 0
