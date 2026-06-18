"""Filesystem-substrate dispatcher for the megatron commander.

Replaces the ``fno mail send`` approach with a plan-file write + ``fno backlog intake``
pipeline. Each dispatch writes a machine-generated plan file under the child project's
``internal/fno/plans/`` directory and registers it in the global backlog.

Idempotency contract
--------------------
``dispatch_project`` does NOT enforce a second-write guard independently. Callers
rely on ``fno backlog intake``'s own de-dup: when a plan_path is already on the graph,
intake prints "already intaked: ab-XXXX" and exits 0 without creating a duplicate node.
The mission loop's ``_projects_to_dispatch`` function prevents double-dispatch at the
loop level by tracking ``sent_msg_ids``; this module handles the single-call contract.

Public surface
--------------
- ``DispatchError``   -- raised on intake failure (includes project name + stderr)
- ``DispatchResult``  -- dataclass returned on success (plan_path, backlog_node_id: Optional[str])
- ``dispatch_project(project, body, mission_id, mission_slug, wave, from_msg_id)``

``DispatchResult.backlog_node_id`` is typed ``Optional[str]`` to honestly
reflect the historical possibility of a ``None`` from idempotent intakes
that did not surface a parseable id. Production paths now raise
``DispatchError`` before returning a ``None`` (commit 2be10bf), but the
type matches the historical contract so static analysis catches future
regressions if the raise-on-empty guard is ever removed.
"""
from __future__ import annotations

import os
import subprocess
import sys
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Optional

from fno._subprocess_util import propagate_returncode
from fno.graph._constants import extract_node_ids
from fno.projects.resolve import resolve_project_name

# Module-level settings path; monkeypatched in tests.
_SETTINGS_PATH: Path = (
    Path(os.path.expanduser("~")) / ".fno" / "settings.yaml"
)


# ---------------------------------------------------------------------------
# Exceptions + result type
# ---------------------------------------------------------------------------

class DispatchError(Exception):
    """Raised when ``fno backlog intake`` returns a non-zero exit code.

    The message always includes the canonical project name and the underlying
    intake stdout/stderr so the commander can surface it verbatim.
    """


@dataclass
class DispatchResult:
    """Successful dispatch outcome.

    Attributes
    ----------
    plan_path:
        Absolute path of the plan file written under the child project's
        ``internal/fno/plans/`` directory.
    backlog_node_id:
        The ``ab-XXXXXXXX`` id assigned by ``fno backlog intake``.
        ``Optional[str]`` because legacy code paths may surface a ``None``
        from idempotent intakes that did not emit a parseable id; production
        code paths now raise ``DispatchError`` before returning a ``None``
        (commit 2be10bf), but the type honestly reflects the historical
        possibility.
    """
    plan_path: Path
    backlog_node_id: Optional[str]


# ---------------------------------------------------------------------------
# Settings loader (private)
# ---------------------------------------------------------------------------

def _load_workspaces() -> dict:
    """Read ``_SETTINGS_PATH`` and return the ``work.workspaces`` dict.

    Returns an empty dict on every failure mode (missing file, bad YAML,
    wrong schema) so callers only need to handle the empty-result case.
    """
    try:
        import yaml
    except ImportError:
        sys.stderr.write(
            "warning: PyYAML missing - megatron dispatch cannot resolve project paths\n"
        )
        return {}

    path = _SETTINGS_PATH
    if not path.exists():
        return {}

    try:
        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    except Exception:
        return {}

    if not isinstance(data, dict):
        return {}

    work = data.get("work", {})
    if not isinstance(work, dict):
        return {}

    workspaces = work.get("workspaces", {})
    return workspaces if isinstance(workspaces, dict) else {}


def _find_project_record(canonical_name: str) -> Optional[dict]:
    """Walk settings workspaces and return the project record for ``canonical_name``.

    Returns ``None`` if no matching record is found.
    """
    workspaces = _load_workspaces()
    for _ws_name, ws_data in workspaces.items():
        if not isinstance(ws_data, dict):
            continue
        projects = ws_data.get("projects", [])
        if not isinstance(projects, list):
            continue
        for project in projects:
            if not isinstance(project, dict):
                continue
            if project.get("name") == canonical_name:
                return project
    return None


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

def dispatch_project(
    project: str,
    body: str,
    mission_id: str,
    mission_slug: str,
    wave: int,
    from_msg_id: Optional[str],
) -> DispatchResult:
    """Write a mission plan file to the child project and register it in the backlog.

    Idempotency: this function does NOT guard against double-dispatch. The mission
    loop prevents re-dispatch via ``sent_msg_ids``; ``fno backlog intake`` de-dups
    by plan_path if called twice (prints "already intaked: ab-XXXX", exits 0).

    Parameters
    ----------
    project:
        Project ``name`` or ``short_name`` as declared in ``~/.fno/settings.yaml``.
        Resolved to the canonical name via ``resolve_project_name``.
    body:
        The mission brief / heads-up text to include in the plan file body.
    mission_id:
        Full ``ab-XXXXXXXX`` mission id.
    mission_slug:
        Human-readable slug for the mission (e.g. ``"2026-05-13-state-co"``).
    wave:
        Wave number (1-based integer).
    from_msg_id:
        Optional upstream message id for reply-chain provenance; ``None`` if not applicable.

    Returns
    -------
    DispatchResult
        Contains the path to the written plan file and the backlog node id.

    Raises
    ------
    ProjectNotFound
        If ``project`` does not resolve to any known project (propagated from resolver).
    DispatchError
        If ``fno backlog intake`` exits non-zero. The plan file is cleaned up before
        raising so no orphan remains in the child project.
    """
    # Step 1: resolve project name (raises ProjectNotFound on unknown/traversal inputs)
    canonical_name = resolve_project_name(project)

    # Step 2: find the project record to get its filesystem path
    record = _find_project_record(canonical_name)
    if record is None:
        raise DispatchError(
            f"dispatch_project: {canonical_name}: project record not found in "
            f"settings.yaml workspaces (resolver returned canonical name but no "
            f"matching record with a path field)"
        )

    raw_path = record.get("path")
    if not raw_path:
        raise DispatchError(
            f"dispatch_project: {canonical_name}: project record has no 'path' field"
        )

    # Expand and resolve symlinks
    project_path = Path(os.path.expanduser(str(raw_path))).resolve()

    # Step 3: compute plan path.
    # Use mission_slug (which already encodes a stable date prefix at mission
    # authoring time) instead of date.today(). The previous design used
    # `date.today()`, which meant a crash between plan-write and
    # append_sent_msg_id could yield a different filename on retry after
    # midnight - fno backlog intake dedups by path, so the second intake
    # would create a duplicate node and trigger duplicate child runs
    # (Codex round-2 review on PR #254). Keying the filename on
    # mission_slug + mission_id_short + wave + project gives a stable,
    # idempotent path that survives clock boundaries.
    mission_id_short = mission_id.removeprefix("ab-")
    safe_slug = mission_slug if mission_slug else date.today().strftime("%Y-%m-%d")
    plan_filename = f"{safe_slug}-mission-{mission_id_short}-wave-{wave}-{canonical_name}.md"
    plans_dir = project_path / "internal" / "fno" / "plans"
    plans_dir.mkdir(parents=True, exist_ok=True)
    plan_path = plans_dir / plan_filename

    # Step 4: build frontmatter + body, guarding against double-frontmatter
    from_msg_id_yaml = str(from_msg_id) if from_msg_id is not None else "null"
    frontmatter = (
        f"---\n"
        f'title: "Mission {mission_id} wave {wave} - {canonical_name}"\n'
        f"mission_id: {mission_id}\n"
        f"mission_slug: {mission_slug}\n"
        f"mission_wave: {wave}\n"
        f"mission_from_msg_id: {from_msg_id_yaml}\n"
        f"priority: p1\n"
        f"size: M\n"
        f"---\n"
    )

    # If body itself starts with a frontmatter block, strip the embedded
    # block so we don't end up with two `---` fences back-to-back (which
    # `fno backlog intake` parses as a single frontmatter ending at the
    # first `---`, leaking the second frontmatter's keys as body content).
    if body.startswith("---\n"):
        # Find the closing fence after the opening one.
        rest = body[len("---\n"):]
        close_idx = rest.find("\n---\n")
        if close_idx >= 0:
            body_block = rest[close_idx + len("\n---\n"):]
        else:
            # Malformed: no closing fence. Strip the leading `---\n` so
            # the file is at least parseable; lose the unterminated block.
            body_block = rest
    else:
        body_block = body
    plan_content = frontmatter + body_block

    plan_path.write_text(plan_content, encoding="utf-8")

    # Step 5: call fno backlog intake from the child project's cwd
    cmd = ["fno", "backlog", "intake", str(plan_path)]
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            cwd=str(project_path),
        )
    except Exception as exc:
        # Subprocess itself failed to launch (e.g. fno not on PATH)
        plan_path.unlink(missing_ok=True)
        raise DispatchError(
            f"dispatch_project: {canonical_name}: failed to launch intake: {exc}"
        ) from exc

    rc = propagate_returncode(result.returncode)

    if rc != 0:
        # Clean up orphan plan file before raising
        plan_path.unlink(missing_ok=True)
        error_detail = (result.stderr or result.stdout or "").strip()
        raise DispatchError(
            f"dispatch_project: {canonical_name}: intake failed (rc={rc}): {error_detail}"
        )

    # Step 6: parse the assigned node id from intake output. Liberal extraction
    # (legacy ab- or any configured prefix/width) over the trusted intake
    # stdout, which prints the new id (or "already intaked: <id>"); the first
    # node-id-shaped token is the assigned node.
    output_text = result.stdout or ""
    candidates = extract_node_ids(output_text)
    node_id = candidates[0] if candidates else ""

    if not node_id:
        # Treat unparseable success as a hard dispatch failure rather than
        # returning an empty backlog_node_id. The loop's append_sent_msg_id
        # de-duplicates identical ids, so multiple "" returns collapse into
        # a single sent_msg_ids entry; subsequent iterations then redispatch
        # the missing projects forever (Codex review on PR #254). Clean up
        # the orphan plan file so a retry can re-intake cleanly.
        plan_path.unlink(missing_ok=True)
        raise DispatchError(
            f"dispatch_project: {canonical_name}: intake succeeded (rc=0) "
            f"but no ab-XXXXXXXX node id found in stdout. stdout={output_text!r}"
        )

    return DispatchResult(plan_path=plan_path, backlog_node_id=node_id)
