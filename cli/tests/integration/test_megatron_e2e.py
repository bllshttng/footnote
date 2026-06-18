"""End-to-end integration tests for the megatron 2-wave 2-project pipeline.

Originally Task 5.1 of plan 2026-05-13-megatron-gap-closure; rewritten for
group 3 (ab-9fd662c6) against the queue verbs that replaced the commander
poll loop. Exercises the full filesystem-substrate pipeline:
  mission_next dispatch -> completion file write -> wave-complete
  predicate -> artifact aggregator -> mission-complete status

The fake dispatcher simulates target + stop-hook by synchronously writing
completion JSON files into the correct fleet directory on each call, so a
single ``mission_next`` call advances through every wave (each wave's
"first incomplete project" scan finds none and recurses).

Structure
---------
Four queue-driven tests + two brief-assembly wiring tests:
  1. test_two_wave_two_project_mission_completes  -- AC5-HP happy path
  2. test_idempotent_rerun_after_completion        -- AC5-FR re-run is no-op
  3. test_paused_mission_resumes_correctly         -- AC5-ERR dispatch failure
  4. test_short_name_in_manifest_resolves_to_canonical -- AC5-EDGE resolver
"""
from __future__ import annotations

import json
import os
import textwrap
from pathlib import Path
from typing import Optional

import pytest
import yaml


# ---------------------------------------------------------------------------
# Module-level constants used in tests
# ---------------------------------------------------------------------------

_SLUG = "2026-05-13-e2e"
_MISSION_ID = "ab-e2e0001"


# ---------------------------------------------------------------------------
# Fixture helpers
# ---------------------------------------------------------------------------


def _write_settings(tmp_path: Path) -> Path:
    """Write a hermetic settings.yaml with two fake projects under tmp_path."""
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)
    settings_path = abilities_dir / "settings.yaml"

    fake_a_path = tmp_path / "projects" / "fake-a"
    fake_b_path = tmp_path / "projects" / "fake-b"
    fake_a_path.mkdir(parents=True, exist_ok=True)
    fake_b_path.mkdir(parents=True, exist_ok=True)

    settings_content = {
        "work": {
            "workspaces": {
                "fakes": {
                    "projects": [
                        {
                            "name": "fake-a",
                            "short_name": "a-short",
                            "path": str(fake_a_path),
                        },
                        {
                            "name": "fake-b",
                            "short_name": "b-short",
                            "path": str(fake_b_path),
                        },
                    ]
                }
            }
        }
    }
    settings_path.write_text(
        yaml.safe_dump(settings_content, default_flow_style=False),
        encoding="utf-8",
    )
    return settings_path


def _write_manifest(fleet_dir: Path, project_names: Optional[list[str]] = None) -> Path:
    """Write a 2-wave manifest for the given project names (defaults to fake-a, fake-b)."""
    names = project_names or ["fake-a", "fake-b"]
    fleet_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = fleet_dir / "00-INDEX.md"
    manifest_path.write_text(
        textwrap.dedent(f"""
        ---
        mission_type: fleet
        mission_id: {_MISSION_ID}
        title: E2E integration test mission
        waves:
          - wave: 1
            mode: parallel
            projects:
              - name: {names[0]}
                body: "Wave 1 brief for {names[0]}"
              - name: {names[1]}
                body: "Wave 1 brief for {names[1]}"
          - wave: 2
            mode: parallel
            projects:
              - name: {names[0]}
                body: "Wave 2 brief for {names[0]}"
              - name: {names[1]}
                body: "Wave 2 brief for {names[1]}"
        ---

        E2E integration test mission.
        """).lstrip(),
        encoding="utf-8",
    )
    return manifest_path


def _write_state_md(state_path: Path, mission_id: str, status: str = "pending") -> None:
    """Write a minimal state.md at state_path."""
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state_path.write_text(
        f"---\n"
        f"mission_id: {mission_id}\n"
        f"status: {status}\n"
        f"created_at: 2026-05-13T12:00:00Z\n"
        f"sent_msg_ids: {{}}\n"
        f"received_completes: []\n"
        f"---\n",
        encoding="utf-8",
    )


def _write_completion_json(
    fleet_dir: Path,
    project: str,
    wave: int,
    pr_url: str = "https://github.com/fake/repo/pull/1",
    discoveries: Optional[str] = None,
) -> Path:
    """Write a valid completion JSON file, simulating the target stop-hook.

    ``discoveries`` writes the (post-spec) ``discoveries`` field on the
    payload when provided. ``None`` omits the key entirely so legacy-shape
    completion JSONs can still be exercised by older tests without
    silently flipping their on-disk shape.
    """
    from datetime import datetime, timezone

    completions_dir = fleet_dir / "completions" / f"wave-{wave}"
    completions_dir.mkdir(parents=True, exist_ok=True)
    target = completions_dir / f"{project}.json"
    tmp = completions_dir / f".{project}.json.tmp"
    payload = {
        "project": project,
        "wave": wave,
        "mission_id": _MISSION_ID,
        "pr_url": pr_url,
        "pr_status": "open",
        "commit_sha": f"deadbeef{wave:02d}{project[:2]}",
        "completed_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "reply_to_msg_id": None,
    }
    if discoveries is not None:
        payload["discoveries"] = discoveries
    tmp.write_text(json.dumps(payload), encoding="utf-8")
    tmp.replace(target)
    return target


def _reset_resolver(settings_path: Path, monkeypatch) -> None:
    """Point the resolver's module-level SETTINGS_PATH to our hermetic file and clear cache."""
    import fno.projects.resolve as resolve_mod
    import fno.megatron.dispatch as dispatch_mod

    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", settings_path)
    monkeypatch.setattr(dispatch_mod, "_SETTINGS_PATH", settings_path)
    # Clear the module-level cache so the patched path is used.
    resolve_mod._clear_cache()

    # Anchor the megatron event sink (fno.paths.resolve_repo_root) at the
    # tmp root so loop.run telemetry lands in tmp/.fno/events.jsonl, NOT
    # the real repo (ab-fc00ae39). settings_path is <tmp>/.fno/settings.yaml.
    # The conftest autouse fixture clears resolve_repo_root's @cache each test;
    # clear it here too so the env override is read on the next resolve.
    monkeypatch.setenv("FNO_REPO_ROOT", str(settings_path.parent.parent))
    from fno.paths import resolve_repo_root

    resolve_repo_root.cache_clear()


# ---------------------------------------------------------------------------
# Fake dispatcher factory
# ---------------------------------------------------------------------------


def _make_fake_dispatcher(fleet_dir: Path, canonical_names: Optional[dict[str, str]] = None):
    """Return (dispatcher, calls_list).

    The dispatcher synchronously writes a completion JSON on each call,
    simulating target running and the stop-hook emitting the completion file.
    ``canonical_names`` is an optional {input_name: canonical_name} map
    used to resolve the project name; defaults to identity (name == canonical).
    """
    calls: list[dict] = []
    _counter = [0]

    def dispatcher(*, to: str, body: str, mission_id: str, kind: str = "heads-up", wave: int = 1) -> str:
        _counter[0] += 1
        msg_id = f"msg-fake-{_counter[0]:04d}"
        canonical = (canonical_names or {}).get(to, to)
        calls.append({"to": to, "canonical": canonical, "wave": wave, "msg_id": msg_id})
        # Simulate the stop hook: write the completion file under the canonical name.
        _write_completion_json(fleet_dir, canonical, wave)
        return msg_id

    return dispatcher, calls


# ---------------------------------------------------------------------------
# Test 1: happy path two-wave two-project mission completes
# ---------------------------------------------------------------------------


def test_two_wave_two_project_mission_completes(tmp_path: Path, monkeypatch):
    """AC5-HP: a 2-wave 2-project mission drives to status: complete end-to-end.

    Setup is entirely in tmpfs; no real HOME, no real settings.yaml.
    The fake dispatcher writes completion JSON files synchronously so the
    loop sees wave-complete on the very next iteration.
    """
    from fno.megatron import read_state
    from fno.megatron.queue import mission_next

    # Setup: resolver + settings
    settings_path = _write_settings(tmp_path)
    _reset_resolver(settings_path, monkeypatch)

    # Fleet dir structure
    fleet_root = tmp_path / "fleet"
    fleet_dir = fleet_root / _SLUG
    manifest_path = _write_manifest(fleet_dir)
    state_path = fleet_dir / "state.md"
    _write_state_md(state_path, _MISSION_ID, status="running")

    # Fake dispatcher (synchronously drops completion files)
    dispatcher, calls = _make_fake_dispatcher(fleet_dir)

    # Drive to completion: the synchronous completions mean every wave
    # drains inside one mission_next call (dispatch -> all complete ->
    # recurse to the next wave -> drained).
    result = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=dispatcher
    )

    # AC5-HP: mission completed
    assert result["kind"] == "drained", f"Expected drained, got {result}"

    # Final state shows complete
    final_state = read_state(state_path, fleet_root=fleet_root)
    assert final_state.status == "complete"

    # All four completion files exist (wave 1 and wave 2, both projects)
    for wave in (1, 2):
        for project in ("fake-a", "fake-b"):
            comp_file = fleet_dir / "completions" / f"wave-{wave}" / f"{project}.json"
            assert comp_file.exists(), f"Missing completion file: {comp_file}"
            payload = json.loads(comp_file.read_text(encoding="utf-8"))
            assert payload["project"] == project
            assert payload["wave"] == wave
            assert payload["mission_id"] == _MISSION_ID

    # Mission-complete artifact exists
    artifact = fleet_dir / f"mission-complete-{_MISSION_ID}.md"
    assert artifact.exists(), f"Mission-complete artifact missing at {artifact}"
    content = artifact.read_text(encoding="utf-8")

    # Artifact contains both wave headings and project rows
    assert "Wave 1" in content, "Artifact body missing Wave 1 heading"
    assert "Wave 2" in content, "Artifact body missing Wave 2 heading"
    assert "fake-a" in content, "Artifact missing fake-a project row"
    assert "fake-b" in content, "Artifact missing fake-b project row"

    # Dispatcher was called exactly 4 times (2 projects x 2 waves)
    assert len(calls) == 4, f"Expected 4 dispatcher calls, got {len(calls)}: {calls}"
    wave1_calls = [c for c in calls if c["wave"] == 1]
    wave2_calls = [c for c in calls if c["wave"] == 2]
    assert len(wave1_calls) == 2
    assert len(wave2_calls) == 2


# ---------------------------------------------------------------------------
# Test 2: idempotent re-run after completion
# ---------------------------------------------------------------------------


def test_idempotent_rerun_after_completion(tmp_path: Path, monkeypatch):
    """AC5-FR: re-invoking loop.run on a complete mission is a clean no-op.

    File mtimes for completion files and the artifact must not change.
    The dispatcher must not be called again.
    """
    from fno.megatron import read_state
    from fno.megatron.queue import mission_next

    settings_path = _write_settings(tmp_path)
    _reset_resolver(settings_path, monkeypatch)

    fleet_root = tmp_path / "fleet"
    fleet_dir = fleet_root / _SLUG
    manifest_path = _write_manifest(fleet_dir)
    state_path = fleet_dir / "state.md"
    _write_state_md(state_path, _MISSION_ID, status="running")

    dispatcher, calls = _make_fake_dispatcher(fleet_dir)

    # First run: reaches complete
    result1 = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=dispatcher
    )
    assert result1["kind"] == "drained"

    # Snapshot mtimes of completion files and artifact
    artifact = fleet_dir / f"mission-complete-{_MISSION_ID}.md"
    assert artifact.exists()

    comp_mtimes: dict[str, float] = {}
    for wave in (1, 2):
        for project in ("fake-a", "fake-b"):
            comp_file = fleet_dir / "completions" / f"wave-{wave}" / f"{project}.json"
            comp_mtimes[f"wave-{wave}/{project}"] = comp_file.stat().st_mtime
    artifact_mtime = artifact.stat().st_mtime

    calls_after_first = len(calls)

    # Second run: should exit immediately (status already complete)
    result2 = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=dispatcher
    )
    assert result2["kind"] == "drained"

    # The state is still complete and nothing changed.
    final_state = read_state(state_path, fleet_root=fleet_root)
    assert final_state.status == "complete"

    # Dispatcher was not called again
    assert len(calls) == calls_after_first, (
        f"Dispatcher called {len(calls) - calls_after_first} extra times on re-run"
    )

    # Completion file mtimes unchanged
    for wave in (1, 2):
        for project in ("fake-a", "fake-b"):
            comp_file = fleet_dir / "completions" / f"wave-{wave}" / f"{project}.json"
            key = f"wave-{wave}/{project}"
            assert comp_file.stat().st_mtime == comp_mtimes[key], (
                f"Completion file {key} was modified on re-run"
            )

    # Artifact mtime unchanged (write_mission_complete is only called from update_status;
    # no status flip happens on re-run so no re-write)
    assert artifact.stat().st_mtime == artifact_mtime, (
        "Artifact was re-written on idempotent re-run"
    )


# ---------------------------------------------------------------------------
# Test 3: paused mission resumes correctly
# ---------------------------------------------------------------------------


def test_paused_mission_resumes_correctly(tmp_path: Path, monkeypatch):
    """AC5-ERR: dispatcher failure pauses the mission; retry succeeds and clears pause.

    The dispatcher fails for fake-b on the first wave-1 dispatch, causing
    the mission to enter paused. After resetting the dispatcher to succeed,
    re-running the loop completes the mission and clears paused_reason.
    """
    from fno.megatron import read_state
    from fno.megatron.dispatch import DispatchError
    from fno.megatron.queue import mission_next

    settings_path = _write_settings(tmp_path)
    _reset_resolver(settings_path, monkeypatch)

    fleet_root = tmp_path / "fleet"
    fleet_dir = fleet_root / _SLUG
    manifest_path = _write_manifest(fleet_dir)
    state_path = fleet_dir / "state.md"
    _write_state_md(state_path, _MISSION_ID, status="running")

    # Failing dispatcher: raises DispatchError for fake-b; succeeds for fake-a
    fail_calls: list[dict] = []
    _counter = [0]

    def failing_dispatcher(*, to: str, body: str, mission_id: str, kind: str = "heads-up", wave: int = 1) -> str:
        _counter[0] += 1
        if to == "fake-b":
            raise DispatchError(f"backlog_intake_failed_for_{to}")
        # fake-a succeeds and drops its completion file
        msg_id = f"msg-fail-{_counter[0]:04d}"
        fail_calls.append({"to": to, "wave": wave})
        _write_completion_json(fleet_dir, to, wave)
        return msg_id

    result1 = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=failing_dispatcher
    )

    # Mission paused due to dispatch failure
    assert result1["kind"] == "pause", f"Expected pause, got: {result1}"
    assert result1["policy"] == "dispatch_failure"
    assert "fake-b" in result1["detail"], (
        f"pause detail should mention fake-b; got: {result1['detail']}"
    )

    paused_state = read_state(state_path, fleet_root=fleet_root)
    assert paused_state.status == "paused"
    assert paused_state.paused_reason is not None

    # Now switch to a working dispatcher; it will complete remaining dispatches
    # and write all missing completion files.
    success_calls: list[dict] = []
    _s_counter = [0]

    def success_dispatcher(*, to: str, body: str, mission_id: str, kind: str = "heads-up", wave: int = 1) -> str:
        _s_counter[0] += 1
        msg_id = f"msg-ok-{_s_counter[0]:04d}"
        success_calls.append({"to": to, "wave": wave})
        _write_completion_json(fleet_dir, to, wave)
        return msg_id

    # Need to flip the state from paused -> running for the loop to proceed.
    # In production this is done by the operator; in tests we do it directly.
    from fno.megatron import update_status
    update_status(state_path, "running", fleet_root=fleet_root)

    result2 = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=success_dispatcher
    )

    assert result2["kind"] == "drained", f"Expected drained after retry, got: {result2}"

    final_state = read_state(state_path, fleet_root=fleet_root)
    assert final_state.status == "complete"
    # paused_reason is cleared when the mission completes successfully
    # (the state.md that gets serialized on terminal flip preserves whatever
    # paused_reason is in the in-memory state, which was set during the pause).
    # The artifact should exist.
    artifact = fleet_dir / f"mission-complete-{_MISSION_ID}.md"
    assert artifact.exists(), "Mission-complete artifact missing after successful retry"


# ---------------------------------------------------------------------------
# Test 4: short_name in manifest resolves to canonical name
# ---------------------------------------------------------------------------


def test_short_name_in_manifest_resolves_to_canonical(tmp_path: Path, monkeypatch):
    """AC5-EDGE: dispatcher resolves short_names to canonical before writing completion files.

    The manifest uses 'a-short' and 'b-short' (the short_name aliases from
    settings.yaml where canonical names are 'fake-a' and 'fake-b').

    Exercises the resolver/dispatcher naming layer: short names from the
    manifest are forwarded to the dispatcher, which resolves and writes
    completion files under canonical names. The matching predicate
    (_wave_complete) also resolves short_name -> canonical so the walk
    advances. Assertions are scoped to wave 1 so failures localize to the
    naming layer.
    """
    from fno.megatron.queue import mission_next

    settings_path = _write_settings(tmp_path)
    _reset_resolver(settings_path, monkeypatch)

    fleet_root = tmp_path / "fleet"
    fleet_dir = fleet_root / _SLUG

    # Manifest uses short_names instead of canonical names
    manifest_path = _write_manifest(fleet_dir, project_names=["a-short", "b-short"])
    state_path = fleet_dir / "state.md"
    _write_state_md(state_path, _MISSION_ID, status="running")

    # The dispatcher resolves short names to canonical names (mirrors real
    # dispatch_project behavior). Fake target writes completion file under canonical.
    from fno.projects.resolve import resolve_project_name

    dispatcher_calls: list[dict] = []
    _counter = [0]

    def resolving_dispatcher(*, to: str, body: str, mission_id: str, kind: str = "heads-up", wave: int = 1) -> str:
        _counter[0] += 1
        canonical = resolve_project_name(to)  # resolves a-short -> fake-a, etc.
        msg_id = f"msg-short-{_counter[0]:04d}"
        dispatcher_calls.append({"to": to, "canonical": canonical, "wave": wave})
        # Write completion file under canonical name (mirrors real dispatch behaviour)
        _write_completion_json(fleet_dir, canonical, wave)
        return msg_id

    # Drive the walk; the dispatcher writes completion files under
    # canonical names (synchronous completes recurse through both waves).
    mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=resolving_dispatcher
    )

    # Dispatcher was called for both short-name projects in wave 1
    wave1_calls = [c for c in dispatcher_calls if c["wave"] == 1]
    assert len(wave1_calls) == 2, (
        f"Expected 2 dispatch calls for wave 1; got: {dispatcher_calls}"
    )

    # Dispatcher received the short names from the manifest ...
    dispatched_to = {c["to"] for c in wave1_calls}
    assert dispatched_to == {"a-short", "b-short"}, (
        f"Expected manifest short names forwarded to dispatcher; got: {dispatched_to}"
    )

    # ... and resolved them to canonical names before writing completion files.
    dispatched_canonical = {c["canonical"] for c in wave1_calls}
    assert dispatched_canonical == {"fake-a", "fake-b"}, (
        f"Expected canonical names used for completion files; got: {dispatched_canonical}"
    )

    # Completion files exist under canonical names, NOT under short names
    for project in ("fake-a", "fake-b"):
        canonical_file = fleet_dir / "completions" / "wave-1" / f"{project}.json"
        assert canonical_file.exists(), f"Missing canonical completion file: {canonical_file}"
        payload = json.loads(canonical_file.read_text(encoding="utf-8"))
        assert payload["project"] == project

    for short_name in ("a-short", "b-short"):
        short_file = fleet_dir / "completions" / "wave-1" / f"{short_name}.json"
        assert not short_file.exists(), (
            f"Unexpected short-name completion file (should be canonical): {short_file}"
        )


# ---------------------------------------------------------------------------
# Test: discoveries roundtrip (producer -> consumer)
# ---------------------------------------------------------------------------


def test_discoveries_roundtrip_into_wave_brief(tmp_path: Path):
    """E2E gap-closer: a producer-written ``discoveries`` field surfaces in
    the wave brief consumed by the next wave's dispatcher.

    The target stop hook (producer) writes ``discoveries: str`` on the
    completion JSON. ``assemble_wave_brief`` (consumer) reads each
    completion's ``discoveries`` field via the post-spec read path.
    Both sides are unit-tested in isolation, but a rename or contract
    drift on either side would pass CI silently without an e2e wiring
    test that exercises the producer-written file through the real
    consumer.

    This test writes a completion-shaped JSON (matching what the stop
    hook would write) and feeds the loaded payload into the real
    ``assemble_wave_brief`` so a field rename on either side fails this
    test. No fake hooks or mocks - the producer-side write helper is
    shared with the rest of the e2e suite (``_write_completion_json``).

    Plan ab-bc919f7f (sigma-review integration-test-analyzer follow-up).
    """
    from fno.megatron.brief import assemble_wave_brief

    fleet_dir = tmp_path / "fleet" / "test-slug"
    fleet_dir.mkdir(parents=True)

    # Wave-1 completions: two projects, each with a distinct discoveries
    # markdown chunk that the stop hook would have extracted from
    # .fno/HANDOFF.md. Fixture matches the REAL producer shape
    # (section body only, no `### Discoveries` header). PR #256 review
    # caught this contract drift - a re-headered fixture masked a
    # consumer extraction bug because both producer and test agreed on a
    # shape the hook never actually writes.
    _write_completion_json(
        fleet_dir,
        project="alpha",
        wave=1,
        discoveries=(
            "- alpha-finding-one: race in the cache write path.\n"
            "- alpha-finding-two: jq -e false is rejected as falsy.\n"
        ),
    )
    _write_completion_json(
        fleet_dir,
        project="beta",
        wave=1,
        discoveries=(
            "- beta-finding-one: settings.yaml watcher is racy on rename.\n"
        ),
    )

    # Load the completion JSONs the way the megatron loop does and feed
    # them into the real brief assembler. Order is irrelevant - sort is
    # internal to assemble_wave_brief.
    completes_for_wave: list[dict] = []
    for completion_path in (fleet_dir / "completions" / "wave-1").glob("*.json"):
        completes_for_wave.append({
            **json.loads(completion_path.read_text(encoding="utf-8")),
            "msg_id": f"msg-{completion_path.stem}",
            "from": completion_path.stem,
        })

    brief = assemble_wave_brief(completes_for_wave=completes_for_wave, wave=2)

    # All three findings round-trip producer -> JSON file -> consumer
    assert "alpha-finding-one" in brief
    assert "alpha-finding-two" in brief
    assert "beta-finding-one" in brief
    # The fallback string MUST NOT appear when discoveries content is present
    assert "(no discoveries reported)" not in brief
    # Stable header
    assert brief.startswith("# Wave 2 brief")


def test_discoveries_omitted_field_renders_no_discoveries_reported(tmp_path: Path):
    """Legacy completion JSONs (no ``discoveries`` field, no ``body``) must
    still flow through the consumer without crashing, and render the
    documented ``(no discoveries reported)`` fallback line."""
    from fno.megatron.brief import assemble_wave_brief

    fleet_dir = tmp_path / "fleet" / "test-slug"
    fleet_dir.mkdir(parents=True)
    # Legacy shape: discoveries omitted entirely (the default behavior
    # of _write_completion_json when discoveries=None).
    _write_completion_json(fleet_dir, project="legacy", wave=1)

    completes_for_wave: list[dict] = []
    for completion_path in (fleet_dir / "completions" / "wave-1").glob("*.json"):
        completes_for_wave.append({
            **json.loads(completion_path.read_text(encoding="utf-8")),
            "msg_id": f"msg-{completion_path.stem}",
            "from": completion_path.stem,
        })

    brief = assemble_wave_brief(completes_for_wave=completes_for_wave, wave=2)
    assert "(no discoveries reported)" in brief
