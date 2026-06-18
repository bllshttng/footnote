"""Megatron mission queue verbs (group 3 of ab-ed61946d, node ab-9fd662c6).

``mission_next`` / ``mission_complete`` back the ``fno megatron next`` /
``fno megatron complete`` plumbing verbs that the Rust MegatronQueue
(``crates/fno-agents/src/loop_megatron.rs``) shells. Together they replace
the commander poll loop (``loop.py``, deleted in the same change):

- ``mission_next`` is dispatch-on-demand + "which project is incomplete":
  it reads manifest + state, guards manifest immutability, eagerly
  dispatches every un-dispatched project of the current wave (plan file +
  ``fno backlog intake`` via ``dispatch.dispatch_project``), and returns
  the first incomplete project of that wave as a unit.
- ``mission_complete`` is the journal-evidenced close record: when the
  Rust loop observes a child megawalk's termination event, it records the
  outcome here. ``done`` writes the completion JSON the old loop POLLED
  for (idempotent: a worker-written record wins); ``failed`` pauses the
  mission (the wave partial-failure path).

The completion JSON files under ``{fleet}/{slug}/completions/wave-N/``
remain the mission ledger - written by worker ship gates
(``scripts/lib/mission-emit.sh``) and backfilled here by the commander on
journal evidence. What died is the POLLING, not the files.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Callable, Optional

from fno.megatron._constants import TERMINAL_STATUSES
from fno.megatron.brief import assemble_wave_brief, inject_brief_into_bodies
from fno.megatron.manifest import Manifest, Wave, load_manifest_and_sha
from fno.megatron.state import (
    MissionState,
    append_sent_msg_id,
    read_state,
    stamp_manifest_sha,
    update_status,
)

DispatchFn = Callable[..., str]


def default_dispatch_fn(mission_slug: str) -> DispatchFn:
    """Return the production dispatch function (plan file + ``fno backlog intake``).

    Mirrors the closure the old ``cli._default_dispatcher`` built; returns the
    backlog node id assigned by intake. Tests inject a fake instead.
    """
    from fno.megatron.dispatch import dispatch_project

    def _dispatch(
        *,
        to: str,
        body: str,
        mission_id: str,
        kind: str = "heads-up",
        wave: int = 1,
    ) -> str:
        result = dispatch_project(
            project=to,
            body=body,
            mission_id=mission_id,
            mission_slug=mission_slug,
            wave=wave,
            from_msg_id=None,
        )
        return result.backlog_node_id or ""

    return _dispatch


# ---------------------------------------------------------------------------
# wave predicates (moved from loop.py, slimmed onto state.received_completes)
# ---------------------------------------------------------------------------

def _canonical(name: str) -> str:
    """Resolve a manifest project name to its canonical settings name.

    ProjectNotFound / SettingsNotFound fall back to the raw name (recoverable:
    unknown name or legacy single-workspace setup). DuplicateShortName is NOT
    caught - ambiguity must fail loudly (spec AC1-FR of the resolver).
    """
    from fno.projects.resolve import (
        ProjectNotFound,
        SettingsNotFound,
        resolve_project_name,
    )

    try:
        return resolve_project_name(name)
    except (ProjectNotFound, SettingsNotFound):
        return name


def _completes_for_wave(state: MissionState, wave_num: int) -> list[dict]:
    """Completion records for one wave, via the filesystem-derived property."""
    out = []
    for c in state.received_completes:
        w = c.get("wave")
        if isinstance(w, str) and w.isdigit():
            w = int(w)
        if w == wave_num:
            out.append(c)
    return out


def _wave_complete(state: MissionState, wave: Wave) -> bool:
    if not wave.projects:
        return True
    completed = {c.get("project") for c in _completes_for_wave(state, wave.wave)}
    expected = {_canonical(p.name) for p in wave.projects}
    return expected.issubset(completed)


def _next_wave(manifest: Manifest, state: MissionState) -> Optional[Wave]:
    """Lowest wave whose participants are not all completed; None when done."""
    for wave in manifest.waves:
        if not _wave_complete(state, wave):
            return wave
    return None


def _emit_event(event_type: str, *, mission_id: str, data: dict) -> None:
    """Best-effort telemetry (manifest_baselined / manifest_mutated / wave_advanced).

    Failures are swallowed so a broken events.jsonl cannot block the state
    transition that just succeeded (same contract as state._emit_status_event).
    """
    try:
        from fno.megatron._telemetry import resolve_events_path

        events_path = resolve_events_path()
        if events_path is None:
            return
        from fno import events as _events

        _events.append_event(
            {
                "type": event_type,
                "ts": _events._ts_now(),
                "source": "megatron",
                "data": {"mission_id": mission_id, **data},
            },
            events_path=events_path,
        )
    except Exception:
        pass


def _emit_wave_advanced(mission_id: str, wave: int) -> None:
    """Best-effort wave_advanced via the typed constructor (schema-validated)."""
    try:
        from fno.megatron._telemetry import resolve_events_path

        events_path = resolve_events_path()
        if events_path is None:
            return
        from fno import events as _events

        _events.append_event(
            _events.wave_advanced(
                mission_id=mission_id, wave=wave, child_session_ids=[]
            ),
            events_path=events_path,
        )
    except Exception:
        pass


# ---------------------------------------------------------------------------
# mission_next
# ---------------------------------------------------------------------------

def _pause(policy: str, detail: str) -> dict:
    return {"kind": "pause", "policy": policy, "detail": detail}


def mission_next(
    manifest_path: Path | str,
    state_path: Path | str,
    *,
    fleet_root: Optional[Path] = None,
    dispatch_fn: Optional[DispatchFn] = None,
) -> dict:
    """Return the next incomplete project unit for a mission.

    Returns one of:
    - ``{"kind": "unit", "unit": {...}}`` - dispatch this project
    - ``{"kind": "drained"}``            - mission complete (status stamped)
    - ``{"kind": "pause", "policy": ..., "detail": ...}`` - walk must pause
    """
    manifest_path = Path(manifest_path)
    state_path = Path(state_path)

    state = read_state(state_path, fleet_root=fleet_root)

    if state.status == "paused":
        return _pause("mission_paused", state.paused_reason or "paused")
    if state.status in TERMINAL_STATUSES:
        if state.status == "complete":
            return {"kind": "drained"}
        return _pause("mission_terminal", f"terminal:{state.status}")

    # Atomic load + sha from the same bytes (TOCTOU guard, inherited from loop.py).
    manifest, fresh_sha = load_manifest_and_sha(manifest_path)

    # Manifest immutability guard.
    if state.manifest_sha256 is None:
        from datetime import datetime, timezone

        now_iso = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        stamp_manifest_sha(state_path, fresh_sha, now_iso, fleet_root=fleet_root)
        state.manifest_sha256 = fresh_sha
        _emit_event(
            "manifest_baselined",
            mission_id=manifest.mission_id,
            data={"sha_short": fresh_sha[:12]},
        )
    elif state.manifest_sha256 != fresh_sha:
        detail = (
            f"manifest_mutated:stored_sha={state.manifest_sha256[:12]} "
            f"fresh_sha={fresh_sha[:12]}"
        )
        update_status(state_path, "paused", paused_reason=detail, fleet_root=fleet_root)
        _emit_event(
            "manifest_mutated",
            mission_id=manifest.mission_id,
            data={
                "stored_sha_short": state.manifest_sha256[:12],
                "fresh_sha_short": fresh_sha[:12],
            },
        )
        return _pause("manifest_mutated", detail)

    wave = _next_wave(manifest, state)
    if wave is None:
        # Every wave complete: stamp the mission (artifact auto-writes in
        # update_status) unless a racing close already did.
        if state.status not in TERMINAL_STATUSES:
            update_status(state_path, "complete", fleet_root=fleet_root)
        return {"kind": "drained"}

    # ── dispatch-on-demand: every un-dispatched project of the current wave ──
    already = state.sent_msg_ids.get(f"wave_{wave.wave}", [])
    pending = wave.projects[len(already):]
    if pending:
        if dispatch_fn is None:
            mission_slug = state.slug or state_path.parent.name
            dispatch_fn = default_dispatch_fn(mission_slug)

        pending_bodies = [p.body for p in pending]
        if wave.wave > 1:
            prior = _completes_for_wave(state, wave.wave - 1)
            if prior:
                brief = assemble_wave_brief(
                    completes_for_wave=prior, wave=wave.wave - 1
                )
                pending_bodies = inject_brief_into_bodies(pending_bodies, brief)

        for project, body in zip(pending, pending_bodies):
            try:
                node_id = dispatch_fn(
                    to=project.name,
                    body=body,
                    mission_id=manifest.mission_id,
                    kind=project.kind,
                    wave=wave.wave,
                )
            except Exception as exc:
                detail = (
                    f"dispatch_failure: wave {wave.wave} project {project.name}: {exc}"
                )
                try:
                    update_status(
                        state_path, "paused", paused_reason=detail, fleet_root=fleet_root
                    )
                except Exception as persist_exc:
                    # The pause still reaches the caller (the Rust loop pauses
                    # this run), but the on-disk status was NOT flipped - a
                    # fresh commander would re-attempt dispatch. Surface that
                    # in the detail rather than swallowing it (sigma-review:
                    # state-transition persistence is never telemetry).
                    import sys

                    detail += f"; WARNING: pause not persisted: {persist_exc}"
                    print(
                        f"megatron queue: failed to persist paused status: {persist_exc}",
                        file=sys.stderr,
                    )
                return _pause("dispatch_failure", detail)
            append_sent_msg_id(state_path, wave=wave.wave, msg_id=node_id)

        state = read_state(state_path, fleet_root=fleet_root)

    # ── first incomplete project of the wave, in manifest order ──────────────
    completed = {c.get("project") for c in _completes_for_wave(state, wave.wave)}
    sent = state.sent_msg_ids.get(f"wave_{wave.wave}", [])
    for idx, project in enumerate(wave.projects):
        canonical = _canonical(project.name)
        if canonical in completed:
            continue
        node_id = sent[idx] if idx < len(sent) else None
        return {
            "kind": "unit",
            "unit": {
                "project": canonical,
                "wave": wave.wave,
                "project_path": _project_path(canonical),
                "node_id": node_id,
                "title": f"Mission {manifest.mission_id} wave {wave.wave} - {canonical}",
                "mission_id": manifest.mission_id,
                "slug": state.slug or state_path.parent.name,
            },
        }

    # Race: the wave filled between _next_wave and here. Recurse once -
    # bounded because each recursion either returns or advances a wave.
    return mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=dispatch_fn
    )


def _dispatched_node_id(
    state: MissionState, manifest: Manifest, canonical: str, wave: int
) -> Optional[str]:
    """The backlog node id dispatch recorded for (project, wave), or None.

    ``sent_msg_ids[wave_N]`` is append-ordered by manifest declaration order
    (the same invariant ``mission_next`` uses to pair node ids with units).
    """
    this_wave = next((w for w in manifest.waves if w.wave == wave), None)
    if this_wave is None:
        return None
    sent = state.sent_msg_ids.get(f"wave_{wave}", [])
    for idx, project in enumerate(this_wave.projects):
        if _canonical(project.name) == canonical:
            return sent[idx] if idx < len(sent) else None
    return None


def _node_status(node_id: str) -> Optional[str]:
    """The node's ``_status`` from the backlog graph, or None when the graph
    has no record of it (graphless setups, test fixtures) or cannot be read.

    Read-only, in-process, via the sanctioned graph store - never a direct
    file mutation.
    """
    try:
        from fno import paths as _paths
        from fno.graph.store import read_graph

        for entry in read_graph(_paths.graph_json()):
            if entry.get("id") == node_id:
                return entry.get("_status")
    except Exception:
        return None
    return None


def _project_path(canonical_name: str) -> Optional[str]:
    """Absolute filesystem path for a project, from settings workspaces."""
    import os

    from fno.megatron.dispatch import _find_project_record

    record = _find_project_record(canonical_name)
    if not record:
        return None
    raw = record.get("path")
    if not raw:
        return None
    return str(Path(os.path.expanduser(str(raw))).resolve())


# ---------------------------------------------------------------------------
# mission_complete
# ---------------------------------------------------------------------------

def mission_complete(
    manifest_path: Path | str,
    state_path: Path | str,
    *,
    project: str,
    wave: int,
    outcome: str,
    reason: str,
    fleet_root: Optional[Path] = None,
) -> dict:
    """Record a project walk's outcome against the mission.

    ``outcome="done"``: idempotently write the completion JSON (a
    worker-written record is never clobbered), then stamp the mission
    complete when it was the last record. ``outcome="failed"``: pause the
    mission with a typed reason (the wave partial-failure path).

    Returns ``{"result": "recorded"|"already"|"wave_complete"|
    "mission_complete"|"paused"}``.
    """
    manifest_path = Path(manifest_path)
    state_path = Path(state_path)
    canonical = _canonical(project)

    if outcome not in ("done", "failed"):
        raise ValueError(f"outcome must be 'done' or 'failed', got {outcome!r}")

    manifest, _sha = load_manifest_and_sha(manifest_path)

    if outcome == "failed":
        detail = f"project_failed: wave {wave} project {canonical}: {reason}"
        update_status(state_path, "paused", paused_reason=detail, fleet_root=fleet_root)
        return {"result": "paused", "detail": detail}

    # ── outcome == "done": verify the graph agrees before recording ──────────
    # A drained child walk (NoWork) is NOT proof the project's mission work is
    # done: a restarted commander's fresh child sees `fno backlog next
    # --mission` return null while a PRIOR child's live claim hides the node
    # (codex P1 on PR #458). Cross-check the dispatched node's graph status;
    # an undone node means the work is in flight elsewhere - pause loudly
    # instead of writing a false completion and advancing the wave. Fail-open
    # only when the graph carries no record of the node (graphless setups).
    state = read_state(state_path, fleet_root=fleet_root)
    node_id = _dispatched_node_id(state, manifest, canonical, wave)
    if node_id:
        node_status = _node_status(node_id)
        if node_status is not None and node_status not in ("done", "superseded"):
            detail = (
                f"project_incomplete: wave {wave} project {canonical}: node "
                f"{node_id} is {node_status!r} (a prior child walk may still "
                f"hold its claim; resume after it finishes)"
            )
            update_status(
                state_path, "paused", paused_reason=detail, fleet_root=fleet_root
            )
            return {"result": "incomplete", "detail": detail}

    # ── idempotent completion-record write ────────────────────────────────────
    fleet_dir = state_path.parent
    record_dir = fleet_dir / "completions" / f"wave-{wave}"
    record_path = record_dir / f"{canonical}.json"

    already = record_path.exists()
    if not already:
        import os
        from datetime import datetime, timezone

        record_dir.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "project": canonical,
            "from": canonical,
            "wave": wave,
            "mission_id": manifest.mission_id,
            "pr_url": None,
            "pr_status": "unknown",
            "commit_sha": None,
            "completed_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "reply_to_msg_id": None,
            "discoveries": "",
            "source": "commander",
            "reason": reason,
        }
        # Create-exclusive write: a worker ship gate (mission-emit.sh) may
        # race this commander backfill for the same record. The worker's
        # record is richer (pr_url, commit_sha, discoveries) and must win;
        # an exists-check + rename would be a TOCTOU clobber (sigma-review).
        # os.link onto the final path fails with FileExistsError when the
        # worker landed first, which we treat as the "already" success path.
        tmp = record_dir / f".{canonical}.json.commander.tmp"
        tmp.write_text(json.dumps(payload) + "\n", encoding="utf-8")
        try:
            os.link(tmp, record_path)
        except FileExistsError:
            already = True
        finally:
            tmp.unlink(missing_ok=True)

    # ── advance bookkeeping ───────────────────────────────────────────────────
    state = read_state(state_path, fleet_root=fleet_root)
    this_wave = next((w for w in manifest.waves if w.wave == wave), None)
    wave_done = this_wave is not None and _wave_complete(state, this_wave)
    if wave_done:
        _emit_wave_advanced(manifest.mission_id, wave)

    if _next_wave(manifest, state) is None:
        if state.status not in TERMINAL_STATUSES:
            update_status(state_path, "complete", fleet_root=fleet_root)
        return {"result": "mission_complete"}

    if already:
        return {"result": "already"}
    if wave_done:
        return {"result": "wave_complete"}
    return {"result": "recorded"}
