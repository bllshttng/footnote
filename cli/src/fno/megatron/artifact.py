"""Mission-completion artifact writer.

Forensic attestation written to ``~/.fno/fleet/{slug}/mission-complete-{mission_id}.md``
when a mission transitions to a terminal status (complete, cancelled,
failed). Written from inside ``update_status`` while the state filelock
is held; failure is logged at WARN and swallowed so state-flip remains
the source of truth.

The artifact is **forensic, not gating**. Megatron has no in-project
stop hook; state.md is the source of truth and the artifact is a derived
attestation that aggregates wave-level identity (sent_msg_ids,
received_completes, project list) for postmortem and tooling. Failure
to write the artifact does not break state-flip.
"""
from __future__ import annotations

import json
import logging
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import yaml

from fno.megatron._constants import TERMINAL_STATUSES
from fno.megatron.manifest import Manifest, ManifestError, load_manifest
from fno.megatron.state import MissionState

_LOG = logging.getLogger(__name__)

__all__ = [
    "TERMINAL_STATUSES",
    "build_mission_artifact",
    "mission_artifact_path",
    "write_mission_artifact",
    "write_mission_complete",
]


def mission_artifact_path(fleet_dir: Path, mission_id: str) -> Path:
    """Deterministic path constructor.

    Stable across calls; safe to inspect for existence checks.
    """
    return fleet_dir / f"mission-complete-{mission_id}.md"


def build_mission_artifact(
    state: MissionState,
    manifest: Optional[Manifest],
    completed_at: Optional[str] = None,
) -> str:
    """Render markdown content (frontmatter + body) for a terminal mission.

    Pure function. No I/O. Manifest is optional: when None or unparseable,
    manifest-derived fields (projects list, total_waves_planned) are set
    to None; state-derived fields (sent_msg_ids, received_completes) come
    from state regardless. Locked decision 6 from the design doc: state
    is the runtime source, manifest is the structural source.

    completed_at defaults to ``datetime.now(timezone.utc)`` rendered as
    ISO-8601 with ``Z`` suffix.
    """
    completed_at = completed_at or datetime.now(timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )

    waves = _build_waves(state)
    total_dispatched = sum(len(w["sent_msg_ids"]) for w in waves)
    total_received = sum(len(w["received_completes"]) for w in waves)

    projects: Optional[list[str]] = None
    total_waves_planned: Optional[int] = None
    if manifest is not None:
        projects = sorted({p.name for w in manifest.waves for p in w.projects})
        total_waves_planned = len(manifest.waves)

    frontmatter: dict = {
        "type": "mission-complete",
        "mission_id": state.mission_id,
        "slug": _slug_from_state(state),
        "status": state.status,
        "created_at": state.created_at,
        "completed_at": completed_at,
        "total_waves_planned": total_waves_planned,
        "total_waves_advanced": len(waves),
        "projects": projects,
        "waves": waves,
        "total_dispatched": total_dispatched,
        "total_received": total_received,
        "paused_reason": state.paused_reason,
    }

    yaml_block = yaml.safe_dump(
        frontmatter, sort_keys=False, default_flow_style=False
    )
    body = _render_body(state, manifest, completed_at)
    return f"---\n{yaml_block}---\n\n{body}"


def write_mission_artifact(
    state: MissionState,
    fleet_dir: Path,
    manifest: Optional[Manifest] = None,
) -> None:
    """Write the artifact to disk via atomic-rename.

    Called from ``update_status`` inside the filelock scope when the new
    status is terminal. Manifest is resolved via sibling lookup if not
    passed: ``fleet_dir / "00-INDEX.md"``. On parse failure, manifest is
    None and manifest-derived fields are elided rather than blocking the
    artifact.

    Forensic-only: any I/O failure is logged at WARN, mirrored to stderr
    for operator visibility, and swallowed. State.md is the truth;
    artifact failure must not break state-flip.
    """
    if state.status not in TERMINAL_STATUSES:
        # Defensive guard; the primary check is in the update_status caller.
        return

    # Defensive slug stamp for callers constructing MissionState in-process
    # (tests, future repair tools) that bypass read_state. Read-state-loaded
    # missions already carry the same value via state.slug; setting it again
    # here is a no-op when consistent.
    if state.slug is None:
        state.slug = fleet_dir.name

    if manifest is None:
        manifest = _resolve_manifest(fleet_dir)

    target = mission_artifact_path(fleet_dir, state.mission_id)
    tmp = target.with_suffix(target.suffix + ".tmp")
    try:
        content = build_mission_artifact(state, manifest)
        # Atomic rename: tmp sibling avoids EXDEV across mount points.
        tmp.write_text(content, encoding="utf-8")
        os.replace(tmp, target)
    except (OSError, yaml.YAMLError) as exc:
        # Catches disk-full, permission errors, and the (very rare) case
        # of yaml.safe_dump raising on an unrepresentable value. The
        # docstring promises stderr mirroring on any failure; broadening
        # the catch keeps that contract for builder failures too.
        _LOG.warning(
            "mission artifact write failed: mission_id=%s fleet_dir=%s error=%s",
            state.mission_id,
            fleet_dir,
            exc,
        )
        # Best-effort cleanup of stale .tmp; ignore further errors.
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        # Stderr WARN for operator visibility (mirrors hook conventions).
        print(
            f"megatron: WARNING: mission artifact write failed for "
            f"{state.mission_id}: {exc}",
            file=sys.stderr,
        )


def _build_waves(state: MissionState) -> list[dict]:
    """Group sent_msg_ids and received_completes per wave.

    Wave keys in state.sent_msg_ids are ``wave_{N}``. Sort numerically so
    wave 10 doesn't sort before wave 2. ``received_completes`` is filtered
    by the integer wave number.
    """
    waves: list[dict] = []
    seen_keys = sorted(
        state.sent_msg_ids.keys(),
        key=lambda k: _wave_num_or_default(k),
    )
    for wave_key in seen_keys:
        wave_num = _wave_num_or_default(wave_key)
        sent = list(state.sent_msg_ids.get(wave_key, []) or [])
        completes = [c for c in state.received_completes if c.get("wave") == wave_num]
        waves.append(
            {
                "wave": wave_num,
                "sent_msg_ids": sent,
                "received_completes": [
                    {
                        "from": c.get("from") or c.get("project"),
                        # str() before slice: a corrupted record with a non-string
                        # commit_sha (int, list, etc.) would raise TypeError on
                        # `[:12]` and crash _build_waves; the defensive coercion
                        # keeps the fallback chain crash-free for any input shape.
                        "msg_id": c.get("msg_id") or str(c.get("commit_sha") or "")[:12] or c.get("project"),
                        "reply_to": c.get("reply_to") or c.get("reply_to_msg_id"),
                        "ts": c.get("ts") or c.get("completed_at"),
                    }
                    for c in completes
                ],
                "advanced_at": _wave_advanced_at(state, wave_num),
            }
        )
    return waves


def _wave_num_or_default(key: str) -> int:
    """Parse ``wave_{N}`` -> int. Returns a large fallback on malformed keys
    so they sort to the end without crashing the builder."""
    if not key.startswith("wave_"):
        return 1 << 30
    try:
        return int(key.removeprefix("wave_"))
    except ValueError:
        return 1 << 30


def _resolve_manifest(fleet_dir: Path) -> Optional[Manifest]:
    """Sibling-lookup helper. Returns None on missing/unparseable manifest.

    The artifact tolerates manifest-absent missions: state.md carries
    enough runtime data to render a complete forensic record. Manifest
    provides the project list and total_waves_planned only.
    """
    manifest_path = fleet_dir / "00-INDEX.md"
    if not manifest_path.exists():
        return None
    try:
        return load_manifest(manifest_path)
    except (ManifestError, OSError, yaml.YAMLError) as exc:
        # Narrow catch: only swallow expected manifest-load failures.
        # Programmer errors (AttributeError, TypeError) propagate to the
        # writer's outer except and surface via the same WARN path.
        _LOG.warning("manifest unparseable at %s: %s", manifest_path, exc)
        return None


def _wave_advanced_at(state: MissionState, wave_num: int) -> Optional[str]:
    """Best-effort timestamp recovery from received_completes.

    State doesn't track per-wave advance timestamps explicitly. When
    ``ts`` is present on completes, the latest ts for the wave is used as
    a proxy. Today ``append_received_complete`` does not record ``ts``,
    so this returns None until the state schema grows that field.
    """
    completes = [c for c in state.received_completes if c.get("wave") == wave_num]
    timestamps = [c.get("ts") for c in completes if c.get("ts")]
    return max(timestamps) if timestamps else None


def _slug_from_state(state: MissionState) -> Optional[str]:
    """Read slug from state.slug; populated by read_state at load time.

    Filesystem-derived (path.parent.name); never written to state.md.
    Returns None when the state was constructed in-memory without a
    backing file.
    """
    return state.slug


def write_mission_complete(
    manifest_path: Path,
    state_path: Path,
    fleet_dir: Path,
    ledger_path: Optional[Path] = None,
) -> Path:
    """Aggregate completion JSONs and ledger costs into a mission-complete artifact.

    Walks ``fleet_dir/completions/wave-*/*.json`` grouping by wave number.
    Cross-references each completion's ``commit_sha`` against ``ledger.json``
    to retrieve cost per session (best-effort; renders ``unknown`` on miss).

    Writes to ``{fleet_dir}/mission-complete-{mission_id}.md`` via atomic
    tempfile + os.replace. Returns the written path.

    Robust to:
    - Missing completion JSON for one project (renders as "(completion file not found)").
    - Missing/malformed ledger.json (renders cost as "unknown").
    - Re-invocation with same inputs (idempotent overwrite, no partial file).
    """
    # Load manifest (structural source: project list, wave names)
    manifest: Optional[Manifest] = None
    try:
        manifest = load_manifest(manifest_path)
    except (ManifestError, OSError, yaml.YAMLError) as exc:
        _LOG.warning("write_mission_complete: manifest unparseable at %s: %s", manifest_path, exc)

    # Load state (runtime source: mission_id, status, created_at, slug,
    # sent_msg_ids, received_completes). fleet_dir IS the slug directory,
    # so fleet_root for read_state is fleet_dir.parent (which triggers
    # filesystem completion-file rebuild under fleet_root/slug/completions/).
    from fno.megatron.state import MissionState, read_state

    state = read_state(state_path, fleet_root=fleet_dir.parent)
    if state.slug is None:
        state.slug = fleet_dir.name

    # Load ledger for cost lookup (optional)
    ledger_by_sha: dict[str, float] = _load_ledger_by_sha(ledger_path)

    # Walk completion files grouped by wave number
    wave_completions: dict[int, list[dict]] = _gather_completions(fleet_dir)

    # Build the artifact content
    content = _build_aggregated_artifact(state, manifest, wave_completions, ledger_by_sha)

    # Write atomically
    target = mission_artifact_path(fleet_dir, state.mission_id)
    tmp = target.with_suffix(target.suffix + ".tmp")
    try:
        tmp.write_text(content, encoding="utf-8")
        os.replace(tmp, target)
    except OSError:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise

    return target


def _load_ledger_by_sha(ledger_path: Optional[Path]) -> dict[str, float]:
    """Return mapping of commit_sha -> cost_usd from ledger.json.

    Returns an empty dict when the ledger is absent or unreadable. The
    completion JSON shape does not currently include session_id, so we
    index on commit_sha only (best-effort; misses are rendered as unknown).
    """
    if ledger_path is None:
        return {}
    try:
        raw = ledger_path.read_text(encoding="utf-8")
        # ledger.json is JSON; use json.loads (stricter parser, surfaces
        # malformed-ledger as JSONDecodeError instead of silently coercing
        # via YAML's looser grammar).
        try:
            data = json.loads(raw)
        except json.JSONDecodeError as exc:
            _LOG.warning("write_mission_complete: ledger malformed at %s: %s", ledger_path, exc)
            return {}
        if not isinstance(data, dict):
            return {}
        entries = data.get("entries") or []
        result: dict[str, float] = {}
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            # Ledger entries do not directly carry commit_sha for now;
            # attempt a best-effort match via sessions list if present.
            # For now we index by any sha-like key we find.
            sha = entry.get("commit_sha") or entry.get("sha")
            cost = entry.get("cost_usd")
            if sha and isinstance(cost, (int, float)):
                result[sha] = float(cost)
        return result
    except (OSError, yaml.YAMLError, ValueError):
        return {}


def _gather_completions(fleet_dir: Path) -> dict[int, list[dict]]:
    """Walk fleet_dir/completions/wave-*/*.json and group records by wave number.

    Wave directories are expected to be named ``wave-{N}`` where N is a
    positive integer. Non-matching directories are skipped. Malformed JSON
    files are skipped silently (the caller renders the absence as
    '(completion file not found)').
    """
    comp_root = fleet_dir / "completions"
    if not comp_root.is_dir():
        return {}

    result: dict[int, list[dict]] = {}
    for wave_dir in sorted(comp_root.iterdir()):
        if not wave_dir.is_dir():
            continue
        name = wave_dir.name
        if not name.startswith("wave-"):
            continue
        try:
            wave_num = int(name.removeprefix("wave-"))
        except ValueError:
            continue

        records: list[dict] = []
        for json_path in sorted(wave_dir.glob("*.json")):
            if json_path.name.startswith("."):
                continue  # skip tmp files like .project.json.tmp
            try:
                records.append(json.loads(json_path.read_text(encoding="utf-8")))
            except (OSError, ValueError):
                _LOG.warning("write_mission_complete: could not read %s; skipping", json_path)
        result[wave_num] = records
    return result


def _build_aggregated_artifact(
    state: MissionState,
    manifest: Optional[Manifest],
    wave_completions: dict[int, list[dict]],
    ledger_by_sha: dict[str, float],
) -> str:
    """Compose the full markdown content (YAML frontmatter + body)."""
    # Compute elapsed seconds: manifest.created_at -> max(completed_at)
    all_completed_ats = [
        rec.get("completed_at", "")
        for recs in wave_completions.values()
        for rec in recs
        if rec.get("completed_at")
    ]
    elapsed_seconds: Optional[int] = None
    if state.created_at and all_completed_ats:
        try:
            start_dt = datetime.fromisoformat(state.created_at.rstrip("Z")).replace(
                tzinfo=timezone.utc
            )
            end_dt = datetime.fromisoformat(max(all_completed_ats).rstrip("Z")).replace(
                tzinfo=timezone.utc
            )
            elapsed_seconds = int((end_dt - start_dt).total_seconds())
        except (ValueError, AttributeError):
            pass

    # Total cost: sum all matched ledger entries across completions
    total_cost: Optional[float] = None
    for recs in wave_completions.values():
        for rec in recs:
            sha = rec.get("commit_sha")
            if sha and sha in ledger_by_sha:
                if total_cost is None:
                    total_cost = 0.0
                total_cost += ledger_by_sha[sha]

    # project_count: from manifest if available, else count completion records
    if manifest is not None:
        project_count = sum(len(w.projects) for w in manifest.waves)
        waves_completed = len(manifest.waves)
    else:
        waves_completed = len(wave_completions)
        project_count = sum(len(recs) for recs in wave_completions.values())

    mission_slug: Optional[str] = state.slug

    # State-based metrics (preserved for back-compat with consumers of
    # the original write_mission_artifact output).
    state_waves = _build_waves(state)
    total_dispatched = sum(len(w["sent_msg_ids"]) for w in state_waves)
    total_received = sum(len(w["received_completes"]) for w in state_waves)
    projects_list: Optional[list[str]] = None
    if manifest is not None:
        projects_list = sorted({p.name for w in manifest.waves for p in w.projects})

    frontmatter: dict = {
        "type": "mission-complete",
        "mission_id": state.mission_id,
        "mission_slug": mission_slug,
        # "status" is the back-compat field name from write_mission_artifact;
        # "mission_status" is the new name from the aggregated spec. Both
        # are included so existing consumers of the original artifact keep
        # working after the overwrite.
        "status": state.status,
        "mission_status": state.status,
        "created_at": state.created_at,
        # Back-compat fields from the state-based artifact.
        "total_waves_advanced": len(state_waves),
        "total_dispatched": total_dispatched,
        "total_received": total_received,
        "projects": projects_list,
        # New aggregated fields.
        "waves_completed": waves_completed,
        "project_count": project_count,
        "elapsed_seconds": elapsed_seconds,
        "total_cost_usd": total_cost,
    }
    yaml_block = yaml.safe_dump(frontmatter, sort_keys=False, default_flow_style=False)

    # Build body: per-wave headings with per-project rows
    body_lines: list[str] = ["# Mission complete\n"]
    if manifest is not None:
        body_lines.append(f"Mission **{state.mission_id}** completed with status `{state.status}`.\n")
        for wave_def in manifest.waves:
            wave_num = wave_def.wave
            wave_title = wave_def.extra.get("name") or f"Wave {wave_num}"
            body_lines.append(f"\n## Wave {wave_num} - {wave_title}\n")
            recs_by_project = {
                rec["project"]: rec
                for rec in wave_completions.get(wave_num, [])
                if isinstance(rec.get("project"), str)
            }
            # Canonicalize manifest project names before the lookup so a
            # manifest declaring projects via short_name still matches
            # completion records (which are written under canonical names).
            # The loop's _wave_complete already canonicalizes; the retro
            # aggregator used to skip that step and render
            # "(completion file not found)" for short_name projects even
            # though the mission completed cleanly (Codex round-2 review
            # on PR #254). Fall back to the raw name when the resolver
            # cannot run (settings.yaml absent etc.).
            from fno.projects.resolve import (
                ProjectNotFound,
                SettingsNotFound,
                resolve_project_name,
            )

            def _canonical(name: str) -> str:
                try:
                    return resolve_project_name(name)
                except (ProjectNotFound, SettingsNotFound):
                    return name

            for proj in wave_def.projects:
                canonical = _canonical(proj.name)
                rec = recs_by_project.get(canonical) or recs_by_project.get(proj.name)
                if rec is None:
                    body_lines.append(f"- **{proj.name}**: (completion file not found)\n")
                    continue
                pr_url = rec.get("pr_url") or "(no PR URL)"
                sha = rec.get("commit_sha") or "unknown"
                completed = rec.get("completed_at") or "unknown"
                cost_usd = ledger_by_sha.get(sha)
                cost_str = f"${cost_usd:.2f}" if cost_usd is not None else "unknown"
                body_lines.append(
                    f"- **{proj.name}**: PR [{pr_url}]({pr_url}) | "
                    f"commit: `{sha}` | completed: {completed} | cost: {cost_str}\n"
                )
    else:
        body_lines.append(f"Mission **{state.mission_id}** completed with status `{state.status}`.\n")
        for wave_num in sorted(wave_completions):
            body_lines.append(f"\n## Wave {wave_num}\n")
            for rec in wave_completions[wave_num]:
                proj_name = rec.get("project", "unknown")
                pr_url = rec.get("pr_url") or "(no PR URL)"
                sha = rec.get("commit_sha") or "unknown"
                completed = rec.get("completed_at") or "unknown"
                cost_usd = ledger_by_sha.get(sha)
                cost_str = f"${cost_usd:.2f}" if cost_usd is not None else "unknown"
                body_lines.append(
                    f"- **{proj_name}**: PR [{pr_url}]({pr_url}) | "
                    f"commit: `{sha}` | completed: {completed} | cost: {cost_str}\n"
                )

    body = "".join(body_lines)
    return f"---\n{yaml_block}---\n\n{body}"


def _render_body(
    state: MissionState,
    manifest: Optional[Manifest],
    completed_at: str,
) -> str:
    """Short prose summary. Implementer's discretion (locked decision 1)."""
    title = state.mission_id
    if manifest is not None:
        # Manifest dataclass has no `title` field; titles land in `extra`.
        extra_title = manifest.extra.get("title")
        if isinstance(extra_title, str) and extra_title.strip():
            title = extra_title
    waves_advanced = sum(1 for v in state.sent_msg_ids.values() if v)
    msgs_sent = sum(len(v) for v in state.sent_msg_ids.values())
    completes_received = len(state.received_completes)
    return (
        f"# Mission complete: {title}\n\n"
        f"Mission **{state.mission_id}** terminated with status "
        f"`{state.status}` at {completed_at}.\n\n"
        f"Total waves dispatched: {waves_advanced}. "
        f"Total messages sent: {msgs_sent}. "
        f"Total completes received: {completes_received}.\n\n"
        f"See `events.jsonl` for the full event stream "
        f"(`grep '\"mission_id\":\"{state.mission_id}\"' "
        f".fno/events.jsonl`).\n"
    )
