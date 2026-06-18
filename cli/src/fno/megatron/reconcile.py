"""fno megatron reconcile: detect filesystem-vs-PR completion drift.

Pure-logic helpers split from I/O. The CLI in cli.py wires them together.
The GitHub query is the only I/O dependency; tests stub ``query_pr_state``
via the ``query_pr`` parameter on ``scan_drift``.
"""
from __future__ import annotations

import glob as _glob
import json
import os
import re
import secrets
import shutil
import subprocess
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

from fno.megatron.manifest import Manifest
from fno.projects.resolve import (
    SETTINGS_PATH,
    ProjectNotFound,
    SettingsNotFound,
    resolve_project_name,
)

# `gh pr list` default timeout; covers network hangs and stuck auth prompts.
GH_QUERY_TIMEOUT_S = 30.0

# A minimal completion record is valid when it's a JSON object with a
# string `project` field. Loose by design: completion records evolve
# additively, and reconcile only cares that the file IS a completion
# record rather than a placeholder or stub. The runtime completion
# predicate in loop.py applies stricter checks for advance gating.
_REQUIRED_COMPLETION_KEYS = ("project",)


class ReconcileError(Exception):
    """Raised on gh failure or other I/O errors during reconcile."""


def _completion_payload_valid(path: Path) -> tuple[bool, Optional[str]]:
    """Return (ok, reason). A completion file is valid when it parses as a
    JSON object with the documented required keys present and non-empty.
    Missing keys, non-object top-level, null values, or non-string types
    are all treated as drift."""
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as exc:
        return False, f"unreadable: {exc}"
    try:
        obj = json.loads(raw)
    except json.JSONDecodeError as exc:
        return False, f"not JSON: {exc.msg}"
    if not isinstance(obj, dict):
        return False, f"not a JSON object (got {type(obj).__name__})"
    for key in _REQUIRED_COMPLETION_KEYS:
        if key not in obj:
            return False, f"missing required key '{key}'"
        val = obj[key]
        if not isinstance(val, str) or not val:
            return False, (
                f"required key '{key}' must be a non-empty string "
                f"(got {type(val).__name__}: {val!r})"
            )
    return True, None


# `owner/repo` cache keyed by canonical project name. Cleared from tests via
# `_clear_repo_slug_cache()`.
_REPO_SLUG_CACHE: dict[str, Optional[str]] = {}


def _clear_repo_slug_cache() -> None:
    _REPO_SLUG_CACHE.clear()


# Matches the repo segment of a GitHub origin URL. Allows '.' inside repo
# names (e.g. `org/my.project`) but strips a trailing `.git` so the slug is
# `org/my.project` not `org/my.project.git`. Stops on `/`, whitespace, or
# end-of-string.
_GITHUB_ORIGIN_RE = re.compile(r"github\.com[:/]+([^/\s]+)/([^/\s]+?)(?:\.git)?\s*$")


def _project_repo_slug(canonical_name: str) -> Optional[str]:
    """Return 'owner/name' for the given canonical project, or None if the
    project's workspace path can't be located or has no GitHub remote.

    Result is cached per process; tests clear via _clear_repo_slug_cache().
    The settings.yaml schema treats `work.workspaces.<ws>.projects` as a
    LIST of project dicts (not a dict keyed by short_name).
    """
    if canonical_name in _REPO_SLUG_CACHE:
        return _REPO_SLUG_CACHE[canonical_name]

    def _store_and_return(value: Optional[str]) -> Optional[str]:
        _REPO_SLUG_CACHE[canonical_name] = value
        return value

    if not SETTINGS_PATH.exists():
        return _store_and_return(None)
    try:
        import yaml as _yaml
    except ImportError:
        return _store_and_return(None)
    try:
        with SETTINGS_PATH.open() as fh:
            data = _yaml.safe_load(fh)
    except (OSError, _yaml.YAMLError):
        return _store_and_return(None)
    # Tolerate malformed settings.yaml without crashing: the previous
    # `(x or {}).get(...)` chain relied on falsiness to short-circuit, but a
    # truthy non-dict scalar (e.g. user wrote `work: foo` instead of a
    # mapping) flows through and crashes with AttributeError on .get(). Use
    # explicit isinstance guards at each level so a malformed config is
    # treated the same as a missing key.
    if not isinstance(data, dict):
        return _store_and_return(None)
    # config.work is canonical; fall back to legacy top-level work.
    work = (data.get("config") or {}).get("work") or data.get("work")
    if not isinstance(work, dict):
        return _store_and_return(None)
    workspaces = work.get("workspaces")
    if not isinstance(workspaces, dict):
        return _store_and_return(None)
    for ws_def in workspaces.values():
        if not isinstance(ws_def, dict):
            continue
        projects = ws_def.get("projects") or []
        # Schema is a list of dicts; tolerate a dict-keyed variant defensively
        # but never default to current directory.
        if isinstance(projects, dict):
            iter_projects = list(projects.values())
        elif isinstance(projects, list):
            iter_projects = projects
        else:
            continue

        for proj in iter_projects:
            if not isinstance(proj, dict) or proj.get("name") != canonical_name:
                continue
            path_raw = proj.get("path")
            if not isinstance(path_raw, str) or not path_raw.strip():
                # No project path declared - refuse to fall back to cwd.
                return _store_and_return(None)
            proj_path = Path(os.path.expanduser(path_raw))
            if not proj_path.is_dir():
                return _store_and_return(None)
            try:
                res = subprocess.run(
                    ["git", "-C", str(proj_path), "remote", "get-url", "origin"],
                    capture_output=True, text=True, check=False, timeout=5.0,
                )
            except (OSError, subprocess.SubprocessError):
                return _store_and_return(None)
            if res.returncode != 0:
                return _store_and_return(None)
            m = _GITHUB_ORIGIN_RE.search(res.stdout.strip())
            if not m:
                return _store_and_return(None)
            return _store_and_return(f"{m.group(1)}/{m.group(2)}")
    return _store_and_return(None)


@dataclass
class PrState:
    number: int
    url: str
    state: str  # "OPEN" | "CLOSED" | "MERGED"
    merged_at: Optional[str]
    merge_commit_sha: Optional[str]


@dataclass
class DriftRecord:
    wave: int
    project: str
    completion_exists: bool
    completion_path: Path
    branch_pattern: str
    pr_candidates: list[PrState] = field(default_factory=list)
    state: str = ""
    backfill_attempted: bool = False
    backfill_written: bool = False
    backfill_skipped_reason: Optional[str] = None


@dataclass
class DriftReport:
    mission_id: str
    fleet_dir: Path
    drift: list[DriftRecord] = field(default_factory=list)

    @property
    def has_drift(self) -> bool:
        return any(not d.completion_exists for d in self.drift)


def _expected_branch_pattern(
    mission_slug: str, mission_id: str, wave: int, project: str
) -> str:
    mission_id_short = mission_id.removeprefix("ab-")
    return (
        f"feature/{mission_slug}-mission-{mission_id_short}-wave-{wave}-{project}"
    )


def _classify_drift_state(candidates: list[PrState]) -> str:
    if not candidates:
        return "missing-no-pr"
    if len(candidates) > 1:
        merged = [c for c in candidates if c.state == "MERGED"]
        if len(merged) == 1:
            return "missing-pr-merged"
        if len(merged) > 1:
            return "ambiguous"
        opens = [c for c in candidates if c.state == "OPEN"]
        if len(opens) == 1:
            return "missing-pr-open"
        if len(opens) > 1:
            return "ambiguous"
        return "missing-pr-closed-unmerged"
    only = candidates[0]
    if only.state == "MERGED":
        return "missing-pr-merged"
    if only.state == "OPEN":
        return "missing-pr-open"
    return "missing-pr-closed-unmerged"


def query_pr_state(
    project: str,
    branch_pattern: str,
    *,
    repo_slug: Optional[str] = None,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> list[PrState]:
    """Shell out to ``gh pr list --search head:<pattern>`` and parse JSON.

    Returns a (possibly empty) list of PrState. Raises ``ReconcileError``
    on gh failure (auth missing, network down, parse error, timeout).

    When ``repo_slug`` is provided (e.g. "owner/name"), the gh call is
    scoped to that repo via --repo; otherwise gh uses the current
    directory's repo, which is rarely correct for cross-project missions.
    """
    if shutil.which("gh") is None:
        raise ReconcileError("gh CLI not found on PATH")

    cmd = ["gh", "pr", "list"]
    if repo_slug:
        cmd += ["--repo", repo_slug]
    cmd += [
        "--search", f"head:{branch_pattern}",
        "--state", "all",
        "--json", "number,url,state,mergedAt,mergeCommit",
        "--limit", "10",
    ]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, check=False, timeout=timeout_s,
        )
    except subprocess.TimeoutExpired as exc:
        raise ReconcileError(
            f"gh pr list timed out after {timeout_s}s"
        ) from exc
    except OSError as exc:
        raise ReconcileError(f"gh subprocess failed to launch: {exc}") from exc

    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr list failed (rc={result.returncode}): {result.stderr.strip()}"
        )

    try:
        rows = json.loads(result.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc

    return [
        PrState(
            number=row["number"],
            url=row["url"],
            state=row.get("state", "UNKNOWN"),
            merged_at=row.get("mergedAt"),
            merge_commit_sha=(row.get("mergeCommit") or {}).get("oid"),
        )
        for row in rows
    ]


def scan_drift(
    manifest: Manifest,
    fleet_dir: Path,
    mission_slug: str,
    *,
    query_pr: Optional[Callable[..., list[PrState]]] = None,
) -> DriftReport:
    """Walk manifest waves, detect missing/corrupt completion files, attach PR candidates.

    A completion file is treated as "no-drift" only when it parses as a JSON
    object carrying the documented required keys. Corrupt or stub files
    count as drift and trigger a PR query just like a missing file does.

    Tests inject a stub ``query_pr`` to avoid shelling out to ``gh``.
    Stubs may accept the optional ``repo_slug`` and ``timeout_s`` kwargs.
    """
    if query_pr is None:
        query_pr = query_pr_state

    report = DriftReport(mission_id=manifest.mission_id, fleet_dir=fleet_dir)
    completions_root = fleet_dir / "completions"

    for wave_def in manifest.waves:
        for project_def in wave_def.projects:
            try:
                canonical = resolve_project_name(project_def.name)
            except (ProjectNotFound, SettingsNotFound):
                canonical = project_def.name

            completion_path = (
                completions_root / f"wave-{wave_def.wave}" / f"{canonical}.json"
            )
            present = completion_path.exists()
            valid = False
            invalid_reason: Optional[str] = None
            if present:
                valid, invalid_reason = _completion_payload_valid(completion_path)

            record = DriftRecord(
                wave=wave_def.wave,
                project=canonical,
                completion_exists=valid,
                completion_path=completion_path,
                branch_pattern=_expected_branch_pattern(
                    mission_slug, manifest.mission_id, wave_def.wave, canonical
                ),
            )

            if valid:
                record.state = "no-drift"
            else:
                # Missing or corrupt -> drift. Query GitHub for candidate PRs.
                repo_slug = _project_repo_slug(canonical)
                try:
                    try:
                        candidates = query_pr(
                            canonical, record.branch_pattern, repo_slug=repo_slug
                        )
                    except TypeError:
                        # Tolerate stubs/older callers that lack repo_slug.
                        candidates = query_pr(canonical, record.branch_pattern)
                    record.pr_candidates = candidates
                    record.state = _classify_drift_state(record.pr_candidates)
                except ReconcileError as exc:
                    record.state = "query-failed"
                    record.backfill_skipped_reason = str(exc)
                if present and invalid_reason and not record.backfill_skipped_reason:
                    record.backfill_skipped_reason = (
                        f"corrupt completion file present ({invalid_reason}); "
                        f"remove or fix before --backfill"
                    )

            report.drift.append(record)

    return report


def backfill_completion(
    record: DriftRecord,
    *,
    mission_id: str,
    pr_choice_index: int = 0,
) -> bool:
    """Write a backfill completion JSON for a single drift record.

    Returns True iff the file was written; False if skipped.
    Atomic create-if-not-exists via tempfile + ``os.link``; the link call
    raises FileExistsError if a concurrent producer wrote the destination
    between our initial check and the final rename.
    """
    record.backfill_attempted = True

    if record.completion_path.exists():
        try:
            mtime = datetime.fromtimestamp(
                record.completion_path.stat().st_mtime, tz=timezone.utc
            ).isoformat()
        except OSError:
            mtime = "unknown"
        record.backfill_skipped_reason = f"already present (mtime: {mtime})"
        return False
    if record.state == "query-failed":
        return False
    if record.state in (
        "missing-no-pr",
        "missing-pr-open",
        "missing-pr-closed-unmerged",
    ):
        record.backfill_skipped_reason = (
            f"PR state '{record.state}' not safe to backfill (need merged PR)"
        )
        return False

    if pr_choice_index < 0 or pr_choice_index >= len(record.pr_candidates):
        record.backfill_skipped_reason = (
            f"pr_choice_index {pr_choice_index} out of range "
            f"(got {len(record.pr_candidates)} candidates)"
        )
        return False

    pr = record.pr_candidates[pr_choice_index]
    # The selected candidate itself must be MERGED. State 'ambiguous' can hold
    # multiple opens too, so verify per-candidate, not per-record.
    if pr.state != "MERGED":
        record.backfill_skipped_reason = (
            f"selected candidate PR #{pr.number} is {pr.state}, not MERGED"
        )
        return False
    if pr.merge_commit_sha is None:
        record.backfill_skipped_reason = (
            "merged PR has null merge_commit_sha; refusing to backfill"
        )
        return False

    if not pr.merged_at:
        record.backfill_skipped_reason = (
            "merged PR has null merged_at timestamp; refusing to fabricate completed_at"
        )
        return False

    payload = {
        "schema_version": 1,
        "project": record.project,
        "wave": record.wave,
        "mission_id": mission_id,
        "pr_url": pr.url,
        "pr_status": "merged",
        "commit_sha": pr.merge_commit_sha,
        "completed_at": pr.merged_at,
        "reply_to_msg_id": None,
        "discoveries": "(no discoveries reported)",
        "source": "reconcile-backfill",
    }

    record.completion_path.parent.mkdir(parents=True, exist_ok=True)

    # Reap any orphan tmp files left behind by SIGKILL'd prior runs.
    # Pattern: ".{completion_name}.{pid}-{hex}.tmp". The completion file
    # name is escaped because project names can carry glob metacharacters
    # (square brackets, asterisks, question marks).
    safe_name = _glob.escape(record.completion_path.name)
    for orphan in record.completion_path.parent.glob(f".{safe_name}.*.tmp"):
        try:
            orphan.unlink()
        except OSError:
            pass

    # Per-process tmp suffix prevents two concurrent --backfill runs from
    # racing on a shared tmp path. os.link() atomically promotes tmp -> final
    # and raises FileExistsError if the destination is already present, so
    # the "never clobbers" contract holds across the exists()-check-to-write
    # race window.
    tmp_suffix = f".{os.getpid()}-{secrets.token_hex(4)}.tmp"
    tmp_path = record.completion_path.with_name(
        f".{record.completion_path.name}{tmp_suffix}"
    )
    try:
        tmp_path.write_text(json.dumps(payload), encoding="utf-8")
        try:
            os.link(tmp_path, record.completion_path)
        except FileExistsError:
            tmp_path.unlink(missing_ok=True)
            record.backfill_skipped_reason = (
                "completion file appeared mid-write (race with concurrent producer); "
                "refusing to clobber"
            )
            return False
        # link succeeded; remove the tmp so we don't leave a duplicate inode.
        tmp_path.unlink(missing_ok=True)
    except OSError as exc:
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError:
            pass
        record.backfill_skipped_reason = f"OSError writing backfill: {exc}"
        return False

    record.backfill_written = True
    record.completion_exists = True

    # Best-effort forensic event; never raises.
    try:
        events_dir = Path(".fno")
        if events_dir.is_dir():
            from fno import events as _events

            _events.append_event(
                {
                    "ts": _events._ts_now(),
                    "type": "reconcile_backfill_wrote",
                    "source": "megatron",
                    "data": {
                        "mission_id": mission_id,
                        "wave": record.wave,
                        "project": record.project,
                        "pr_url": pr.url,
                        "commit_sha": pr.merge_commit_sha,
                    },
                }
            )
    except Exception:
        pass

    return True


def render_drift_report(
    report: DriftReport, *, as_json: bool = False, verbose: bool = False
) -> str:
    """Format a DriftReport as markdown or JSON."""
    if as_json:
        return json.dumps(
            {
                "mission_id": report.mission_id,
                "fleet_dir": str(report.fleet_dir),
                "drift": [
                    {
                        "wave": d.wave,
                        "project": d.project,
                        "completion_exists": d.completion_exists,
                        "state": d.state,
                        "branch_pattern": d.branch_pattern,
                        "pr_candidates": [
                            {
                                "number": p.number,
                                "url": p.url,
                                "state": p.state,
                                "merged_at": p.merged_at,
                                "merge_commit_sha": p.merge_commit_sha,
                            }
                            for p in d.pr_candidates
                        ],
                        "backfill_attempted": d.backfill_attempted,
                        "backfill_written": d.backfill_written,
                        "backfill_skipped_reason": d.backfill_skipped_reason,
                    }
                    for d in report.drift
                ],
            },
            indent=2,
        )

    lines: list[str] = [
        f"# Reconcile: {report.mission_id}",
        f"Fleet dir: `{report.fleet_dir}`",
        "",
    ]
    rows_shown = 0
    for d in report.drift:
        if d.state == "no-drift" and not verbose:
            continue
        rows_shown += 1
        lines.append(f"## Wave {d.wave} / `{d.project}`")
        lines.append(f"- State: `{d.state}`")
        lines.append(f"- Branch pattern: `{d.branch_pattern}`")
        if d.pr_candidates:
            for p in d.pr_candidates:
                suffix = f"; merged at {p.merged_at}" if p.merged_at else ""
                lines.append(
                    f"- PR #{p.number}: {p.state}; {p.url}{suffix}"
                )
        if d.backfill_attempted:
            if d.backfill_written:
                lines.append(f"- Backfill: WROTE `{d.completion_path}`")
            else:
                lines.append(
                    f"- Backfill: SKIPPED - {d.backfill_skipped_reason}"
                )
        lines.append("")
    if rows_shown == 0:
        lines.append("No drift detected.")
    return "\n".join(lines)
