"""Group-3 (ab-9fd662c6) megatron queue-verb tests.

``mission_next`` / ``mission_complete`` back the ``fno megatron next`` /
``fno megatron complete`` plumbing verbs the Rust MegatronQueue shells.
They replace ``loop.run_iteration``'s poll cycle: next() is dispatch-on-demand
+ "which project is incomplete", complete() is the journal-evidenced close
record (the commander writes the completion JSON the old loop POLLED for).
"""
from __future__ import annotations

import json
import textwrap
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _isolate_project_resolver(tmp_path, monkeypatch):
    """Point the project-name resolver at a nonexistent settings file.

    ``queue._canonical`` resolves manifest names via the REAL
    ``~/.fno/settings.yaml`` otherwise, so a developer machine whose
    settings define a project named (or short-named) ``backend`` would make
    raw-seeded completion records mismatch their canonicalized expectations.
    SettingsNotFound -> raw-name fallback keeps these tests hermetic.
    """
    import fno.megatron.dispatch as dispatch_mod
    import fno.projects.resolve as resolve_mod

    missing = tmp_path / "no-such-settings.yaml"
    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", missing)
    monkeypatch.setattr(dispatch_mod, "_SETTINGS_PATH", missing)
    resolve_mod._clear_cache()
    yield
    resolve_mod._clear_cache()


def _write_manifest(path: Path, content: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(content).lstrip(), encoding="utf-8")
    return path


def _make_state(path: Path, mission_id: str, status: str = "running") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"---\n"
        f"mission_id: {mission_id}\n"
        f"status: {status}\n"
        f"slug: {path.parent.name}\n"
        f"created_at: 2026-06-07T13:00:00Z\n"
        f"sent_msg_ids: {{}}\n"
        f"---\n",
        encoding="utf-8",
    )


TWO_PROJECT_MANIFEST = """
---
mission_type: fleet
mission_id: ab-mq0001
waves:
  - wave: 1
    mode: sequential
    projects:
      - name: backend
        body: "ship the region feature"
      - name: frontend
        body: "render the region picker"
---
"""

TWO_WAVE_MANIFEST = """
---
mission_type: fleet
mission_id: ab-mq0002
waves:
  - wave: 1
    mode: sequential
    projects:
      - name: backend
        body: "ship the api"
  - wave: 2
    mode: sequential
    projects:
      - name: frontend
        body: "consume the api"
---
"""


class FakeDispatch:
    """Records dispatch calls; returns deterministic node ids."""

    def __init__(self, fail_for: str | None = None):
        self.calls: list[dict] = []
        self._counter = 0
        self._fail_for = fail_for

    def __call__(
        self,
        *,
        to: str,
        body: str,
        mission_id: str,
        kind: str = "heads-up",
        wave: int = 1,
    ) -> str:
        if self._fail_for == to:
            raise RuntimeError(f"intake exploded for {to}")
        self._counter += 1
        node_id = f"ab-fk{self._counter:06x}"
        self.calls.append(
            {"to": to, "body": body, "mission_id": mission_id, "kind": kind, "wave": wave}
        )
        return node_id


def _fleet(tmp_path: Path, manifest: str, mission_id: str) -> tuple[Path, Path, Path]:
    """Build a fleet dir; return (fleet_root, manifest_path, state_path)."""
    fleet_root = tmp_path / "fleet"
    fleet_dir = fleet_root / "2026-06-07-test-mission"
    manifest_path = _write_manifest(fleet_dir / "00-INDEX.md", manifest)
    state_path = fleet_dir / "state.md"
    _make_state(state_path, mission_id)
    return fleet_root, manifest_path, state_path


def _seed_complete(fleet_dir: Path, wave: int, project: str, **extra) -> None:
    d = fleet_dir / "completions" / f"wave-{wave}"
    d.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema_version": 1,
        "project": project,
        "from": project,
        "wave": wave,
        "completed_at": "2026-06-07T14:00:00Z",
        **extra,
    }
    (d / f"{project}.json").write_text(json.dumps(payload), encoding="utf-8")


# ---------------------------------------------------------------------------
# mission_next
# ---------------------------------------------------------------------------

def test_next_dispatches_wave_and_returns_first_unit(tmp_path):
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()

    out = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake
    )

    assert out["kind"] == "unit"
    unit = out["unit"]
    assert unit["project"] == "backend"
    assert unit["wave"] == 1
    assert unit["mission_id"] == "ab-mq0001"
    assert unit["node_id"] == "ab-fk000001"
    # Both wave-1 projects dispatched eagerly (wave visibility preserved).
    assert [c["to"] for c in fake.calls] == ["backend", "frontend"]


def test_next_is_idempotent_across_restarts(tmp_path):
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()

    first = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)
    second = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    # Restart: no re-dispatch (sent_msg_ids bookkeeping), same incomplete unit.
    assert len(fake.calls) == 2
    assert first["unit"]["project"] == second["unit"]["project"] == "backend"
    assert first["unit"]["node_id"] == second["unit"]["node_id"]


def test_next_skips_completed_project(tmp_path):
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)
    _seed_complete(state_path.parent, 1, "backend")

    out = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    assert out["kind"] == "unit"
    assert out["unit"]["project"] == "frontend"
    assert out["unit"]["node_id"] == "ab-fk000002"


def test_next_advances_wave_with_brief(tmp_path):
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_WAVE_MANIFEST, "ab-mq0002"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)
    _seed_complete(
        state_path.parent, 1, "backend", discoveries="api lives at /v2/regions"
    )

    out = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    assert out["kind"] == "unit"
    assert out["unit"]["project"] == "frontend"
    assert out["unit"]["wave"] == 2
    # Wave-2 dispatch body carries the wave-1 brief.
    wave2_calls = [c for c in fake.calls if c["wave"] == 2]
    assert len(wave2_calls) == 1
    assert "api lives at /v2/regions" in wave2_calls[0]["body"]


def test_next_drained_when_all_waves_complete(tmp_path):
    from fno.megatron.queue import mission_next
    from fno.megatron.state import read_state

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    _seed_complete(state_path.parent, 1, "backend")
    _seed_complete(state_path.parent, 1, "frontend")

    out = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=FakeDispatch()
    )

    assert out["kind"] == "drained"
    assert read_state(state_path, fleet_root=fleet_root).status == "complete"


def test_next_paused_mission_returns_pause(tmp_path):
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    from fno.megatron.state import update_status

    update_status(state_path, "paused", paused_reason="operator hold", fleet_root=fleet_root)

    out = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=FakeDispatch()
    )

    assert out["kind"] == "pause"
    assert out["policy"] == "mission_paused"
    assert "operator hold" in out["detail"]


def test_next_manifest_mutation_pauses(tmp_path):
    from fno.megatron.queue import mission_next
    from fno.megatron.state import read_state

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    manifest_path.write_text(
        manifest_path.read_text(encoding="utf-8").replace("region feature", "MUTATED"),
        encoding="utf-8",
    )
    out = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    assert out["kind"] == "pause"
    assert out["policy"] == "manifest_mutated"
    assert read_state(state_path, fleet_root=fleet_root).status == "paused"


def test_next_dispatch_failure_pauses(tmp_path):
    from fno.megatron.queue import mission_next
    from fno.megatron.state import read_state

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )

    out = mission_next(
        manifest_path,
        state_path,
        fleet_root=fleet_root,
        dispatch_fn=FakeDispatch(fail_for="frontend"),
    )

    assert out["kind"] == "pause"
    assert out["policy"] == "dispatch_failure"
    assert "frontend" in out["detail"]
    assert read_state(state_path, fleet_root=fleet_root).status == "paused"


def test_next_corrupt_completion_does_not_advance(tmp_path):
    """AC5-ERR analog: a corrupted wave-1 completion blocks wave-2 dispatch.

    A wave is "complete" only when EVERY project's completion JSON parses;
    a corrupt file is skipped by the filesystem rebuild, so next() keeps
    returning the not-actually-complete project instead of advancing.
    """
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_WAVE_MANIFEST, "ab-mq0002"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    # Corrupt completion for wave-1 backend.
    d = state_path.parent / "completions" / "wave-1"
    d.mkdir(parents=True, exist_ok=True)
    (d / "backend.json").write_text("{not json", encoding="utf-8")

    out = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    assert out["kind"] == "unit"
    assert out["unit"]["wave"] == 1, "corrupt completion must not advance the wave"
    assert out["unit"]["project"] == "backend"
    # Wave 2 was never dispatched.
    assert all(c["wave"] == 1 for c in fake.calls)


# ---------------------------------------------------------------------------
# mission_complete
# ---------------------------------------------------------------------------

def test_complete_done_writes_record(tmp_path):
    from fno.megatron.queue import mission_complete, mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )

    out = mission_complete(
        manifest_path,
        state_path,
        project="backend",
        wave=1,
        outcome="done",
        reason="NoWork",
        fleet_root=fleet_root,
    )

    assert out["result"] == "recorded"
    record_path = state_path.parent / "completions" / "wave-1" / "backend.json"
    data = json.loads(record_path.read_text(encoding="utf-8"))
    assert data["project"] == "backend"
    assert data["wave"] == 1
    assert data["source"] == "commander"
    assert data["reason"] == "NoWork"
    # next() now sees backend complete.
    nxt = mission_next(
        manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=FakeDispatch()
    )
    assert nxt["unit"]["project"] == "frontend"


def test_complete_done_is_idempotent(tmp_path):
    from fno.megatron.queue import mission_complete

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    _seed_complete(state_path.parent, 1, "backend", pr_url="https://example.com/pr/7")

    out = mission_complete(
        manifest_path,
        state_path,
        project="backend",
        wave=1,
        outcome="done",
        reason="NoWork",
        fleet_root=fleet_root,
    )

    assert out["result"] == "already"
    # Worker-written record is NOT clobbered.
    record_path = state_path.parent / "completions" / "wave-1" / "backend.json"
    data = json.loads(record_path.read_text(encoding="utf-8"))
    assert data["pr_url"] == "https://example.com/pr/7"


def test_complete_failed_pauses_mission(tmp_path):
    from fno.megatron.queue import mission_complete
    from fno.megatron.state import read_state

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )

    out = mission_complete(
        manifest_path,
        state_path,
        project="backend",
        wave=1,
        outcome="failed",
        reason="Budget",
        fleet_root=fleet_root,
    )

    assert out["result"] == "paused"
    state = read_state(state_path, fleet_root=fleet_root)
    assert state.status == "paused"
    assert "backend" in (state.paused_reason or "")
    # No completion record was written for a failed project.
    assert not (state_path.parent / "completions" / "wave-1" / "backend.json").exists()


def test_complete_done_refuses_when_node_not_done(tmp_path, monkeypatch):
    """codex P1 (PR #458): a drained child walk is not proof of completion.
    When the dispatched node's graph status is not done, the done outcome is
    REFUSED: no record written, mission paused, result 'incomplete'."""
    import json as _json

    from fno import paths as paths_mod
    from fno.megatron.queue import mission_complete, mission_next
    from fno.megatron.state import read_state

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    # The dispatched backend node (ab-fk000001) exists and is still ready -
    # e.g. a prior child walk's live claim hid it from `backlog next`.
    graph = tmp_path / "graph.json"
    graph.write_text(
        _json.dumps({"entries": [{"id": "ab-fk000001", "_status": "ready"}]}),
        encoding="utf-8",
    )
    monkeypatch.setattr(paths_mod, "graph_json", lambda: graph)

    out = mission_complete(
        manifest_path, state_path,
        project="backend", wave=1, outcome="done", reason="NoWork",
        fleet_root=fleet_root,
    )

    assert out["result"] == "incomplete"
    assert "ab-fk000001" in out["detail"]
    assert not (state_path.parent / "completions" / "wave-1" / "backend.json").exists()
    state = read_state(state_path, fleet_root=fleet_root)
    assert state.status == "paused"
    assert "project_incomplete" in (state.paused_reason or "")


def test_complete_done_proceeds_when_node_done(tmp_path, monkeypatch):
    """Graph agreement: a done node lets the completion record land."""
    import json as _json

    from fno import paths as paths_mod
    from fno.megatron.queue import mission_complete, mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    graph = tmp_path / "graph.json"
    graph.write_text(
        _json.dumps({"entries": [{"id": "ab-fk000001", "_status": "done"}]}),
        encoding="utf-8",
    )
    monkeypatch.setattr(paths_mod, "graph_json", lambda: graph)

    out = mission_complete(
        manifest_path, state_path,
        project="backend", wave=1, outcome="done", reason="NoWork",
        fleet_root=fleet_root,
    )

    assert out["result"] == "recorded"
    assert (state_path.parent / "completions" / "wave-1" / "backend.json").exists()


def test_complete_last_record_completes_mission(tmp_path):
    from fno.megatron.queue import mission_complete
    from fno.megatron.state import read_state

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    _seed_complete(state_path.parent, 1, "backend")

    out = mission_complete(
        manifest_path,
        state_path,
        project="frontend",
        wave=1,
        outcome="done",
        reason="NoWork",
        fleet_root=fleet_root,
    )

    assert out["result"] == "mission_complete"
    assert read_state(state_path, fleet_root=fleet_root).status == "complete"


def test_next_manifest_revert_resumes_without_repause(tmp_path):
    """Operator recovery: mutate -> pause -> revert bytes + unpause -> next
    proceeds (sha-equality branch) and the stored sha is unchanged."""
    from fno.megatron.queue import mission_next
    from fno.megatron.state import read_state, update_status

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)
    original_bytes = manifest_path.read_text(encoding="utf-8")
    original_sha = read_state(state_path, fleet_root=fleet_root).manifest_sha256

    manifest_path.write_text(original_bytes.replace("region feature", "MUTATED"), encoding="utf-8")
    assert mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)["kind"] == "pause"

    # Operator reverts the manifest and un-pauses.
    manifest_path.write_text(original_bytes, encoding="utf-8")
    update_status(state_path, "running", fleet_root=fleet_root)

    out = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    assert out["kind"] == "unit", f"revert must resume dispatch, got {out}"
    state = read_state(state_path, fleet_root=fleet_root)
    assert state.manifest_sha256 == original_sha, "revert must not re-baseline the sha"


def test_next_non_mapping_completion_is_skipped(tmp_path):
    """A completion file that parses to a non-mapping (list/scalar) must be
    skipped by the rebuild - wave does not advance, nothing crashes."""
    from fno.megatron.queue import mission_next

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_WAVE_MANIFEST, "ab-mq0002"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    d = state_path.parent / "completions" / "wave-1"
    d.mkdir(parents=True, exist_ok=True)
    (d / "backend.json").write_text("[1, 2, 3]", encoding="utf-8")

    out = mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    assert out["kind"] == "unit"
    assert out["unit"]["wave"] == 1
    assert out["unit"]["project"] == "backend"


def test_complete_does_not_clobber_concurrent_worker_record(tmp_path, monkeypatch):
    """TOCTOU guard: a worker record landing AFTER the exists-check but
    BEFORE the commander's write must win (create-exclusive link)."""
    import json as _json

    from fno.megatron import queue as queue_mod

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    record_dir = state_path.parent / "completions" / "wave-1"
    record_path = record_dir / "backend.json"

    # Simulate the race: the worker's record appears during the commander's
    # tmp write (i.e. after the exists() check). Patch write_text on Path is
    # invasive; instead pre-create the record between check and link by
    # hooking os.link's first argument writer - simplest faithful simulation:
    # create the record, then call mission_complete with exists() forced
    # False so the code path proceeds to the link.
    real_exists = queue_mod.Path.exists

    def fake_exists(self):
        if self == record_path:
            return False  # commander believes no record yet
        return real_exists(self)

    record_dir.mkdir(parents=True, exist_ok=True)
    record_path.write_text(
        _json.dumps({"project": "backend", "wave": 1, "pr_url": "https://x/pr/9"}),
        encoding="utf-8",
    )
    monkeypatch.setattr(queue_mod.Path, "exists", fake_exists)

    out = queue_mod.mission_complete(
        manifest_path, state_path,
        project="backend", wave=1, outcome="done", reason="NoWork",
        fleet_root=fleet_root,
    )

    # The worker's record survives untouched; the commander treats it as already.
    data = _json.loads(record_path.read_text(encoding="utf-8"))
    assert data["pr_url"] == "https://x/pr/9", "worker record must never be clobbered"
    assert out["result"] in ("already", "recorded", "wave_complete")


def test_dispatch_failure_pause_persistence_failure_is_surfaced(tmp_path, monkeypatch, capsys):
    """If persisting the paused status fails, the pause detail says so
    (reported-but-not-persisted is never silent)."""
    from fno.megatron import queue as queue_mod

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )

    def exploding_update_status(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(queue_mod, "update_status", exploding_update_status)

    out = queue_mod.mission_next(
        manifest_path,
        state_path,
        fleet_root=fleet_root,
        dispatch_fn=FakeDispatch(fail_for="backend"),
    )

    assert out["kind"] == "pause"
    assert "pause not persisted" in out["detail"]
    assert "disk full" in capsys.readouterr().err


def test_telemetry_events_emitted(tmp_path, monkeypatch):
    """manifest_baselined fires exactly once (lazy sha init); wave_advanced
    fires with the completed wave number on the last record of a wave."""
    import json as _json

    from fno.megatron import _telemetry
    from fno.megatron.queue import mission_complete, mission_next

    events_path = tmp_path / "events.jsonl"
    monkeypatch.setattr(_telemetry, "resolve_events_path", lambda: events_path)

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    fake = FakeDispatch()
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)
    mission_next(manifest_path, state_path, fleet_root=fleet_root, dispatch_fn=fake)

    events = [
        _json.loads(line)
        for line in events_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    baselined = [e for e in events if e["type"] == "manifest_baselined"]
    assert len(baselined) == 1, f"expected exactly one manifest_baselined, got {events}"

    mission_complete(
        manifest_path, state_path,
        project="backend", wave=1, outcome="done", reason="NoWork",
        fleet_root=fleet_root,
    )
    mission_complete(
        manifest_path, state_path,
        project="frontend", wave=1, outcome="done", reason="NoWork",
        fleet_root=fleet_root,
    )

    events = [
        _json.loads(line)
        for line in events_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    advanced = [e for e in events if e["type"] == "wave_advanced"]
    assert len(advanced) == 1, f"expected one wave_advanced, got {events}"
    assert advanced[0]["data"]["wave"] == 1


# ---------------------------------------------------------------------------
# CLI wiring (fno megatron next / complete)
# ---------------------------------------------------------------------------

def test_cli_next_emits_unit_json(tmp_path, monkeypatch):
    from typer.testing import CliRunner
    from fno.megatron import cli as megatron_cli

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    monkeypatch.setattr(megatron_cli, "_fleet_root", lambda: fleet_root)

    fake = FakeDispatch()
    monkeypatch.setattr(
        "fno.megatron.queue.default_dispatch_fn",
        lambda mission_slug: fake,
    )

    result = CliRunner().invoke(megatron_cli.app, ["next", "ab-mq0001", "--json"])

    assert result.exit_code == 0, result.output
    unit = json.loads(result.output.strip())
    assert unit["project"] == "backend"
    assert unit["wave"] == 1


def test_cli_next_unit_wire_shape_matches_rust_parser(tmp_path, monkeypatch):
    """The emitted unit JSON carries the exact fields the Rust MegatronQueue
    reads (project, wave as int, project_path, title) - pins the
    cross-language contract from the Python side."""
    from typer.testing import CliRunner
    from fno.megatron import cli as megatron_cli

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    monkeypatch.setattr(megatron_cli, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(
        "fno.megatron.queue.default_dispatch_fn",
        lambda mission_slug: FakeDispatch(),
    )

    result = CliRunner().invoke(megatron_cli.app, ["next", "ab-mq0001", "--json"])

    assert result.exit_code == 0, result.output
    unit = json.loads(result.output.strip())
    # Fields the Rust parser requires (loop_megatron.rs::next).
    assert isinstance(unit["project"], str) and unit["project"]
    assert isinstance(unit["wave"], int)
    assert "project_path" in unit  # may be null; Rust errors loudly on null
    assert isinstance(unit["title"], str) and unit["title"]
    # Contract fields carried for operators / the Python side.
    for key in ("node_id", "mission_id", "slug"):
        assert key in unit, f"missing contract field {key}"


def test_cli_next_drained_emits_literal_null(tmp_path, monkeypatch):
    """Mission complete -> stdout is the literal `null` the Rust queue
    matches for the Drained branch."""
    from typer.testing import CliRunner
    from fno.megatron import cli as megatron_cli

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    _seed_complete(state_path.parent, 1, "backend")
    _seed_complete(state_path.parent, 1, "frontend")
    monkeypatch.setattr(megatron_cli, "_fleet_root", lambda: fleet_root)

    result = CliRunner().invoke(megatron_cli.app, ["next", "ab-mq0001", "--json"])

    assert result.exit_code == 0, result.output
    assert result.output.strip() == "null"


def test_cli_next_pause_wire_shape(tmp_path, monkeypatch):
    """Paused mission -> stdout is the {"pause": {policy, detail}} envelope
    the Rust queue maps to a walk pause."""
    from typer.testing import CliRunner
    from fno.megatron import cli as megatron_cli
    from fno.megatron.state import update_status

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    update_status(state_path, "paused", paused_reason="operator hold", fleet_root=fleet_root)
    monkeypatch.setattr(megatron_cli, "_fleet_root", lambda: fleet_root)

    result = CliRunner().invoke(megatron_cli.app, ["next", "ab-mq0001", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output.strip())
    assert set(payload.keys()) == {"pause"}
    assert payload["pause"]["policy"] == "mission_paused"
    assert "operator hold" in payload["pause"]["detail"]


def test_cli_complete_accepts_exact_rust_argv(tmp_path, monkeypatch):
    """Drive cmd_complete with the byte-exact flag list the Rust close()
    builds, so a Typer signature change cannot silently break the seam."""
    from typer.testing import CliRunner
    from fno.megatron import cli as megatron_cli

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    monkeypatch.setattr(megatron_cli, "_fleet_root", lambda: fleet_root)

    # Mirror loop_megatron.rs::close argv exactly (after the binary + verb).
    rust_argv = [
        "complete",
        "ab-mq0001",
        "--project",
        "backend",
        "--wave",
        "1",
        "--outcome",
        "done",
        "--reason",
        "NoWork",
    ]
    result = CliRunner().invoke(megatron_cli.app, rust_argv)

    assert result.exit_code == 0, result.output
    assert json.loads(result.output.strip())["result"] == "recorded"


def test_cli_complete_records(tmp_path, monkeypatch):
    from typer.testing import CliRunner
    from fno.megatron import cli as megatron_cli

    fleet_root, manifest_path, state_path = _fleet(
        tmp_path, TWO_PROJECT_MANIFEST, "ab-mq0001"
    )
    monkeypatch.setattr(megatron_cli, "_fleet_root", lambda: fleet_root)

    result = CliRunner().invoke(
        megatron_cli.app,
        [
            "complete",
            "ab-mq0001",
            "--project",
            "backend",
            "--wave",
            "1",
            "--outcome",
            "done",
            "--reason",
            "NoWork",
        ],
    )

    assert result.exit_code == 0, result.output
    out = json.loads(result.output.strip())
    assert out["result"] == "recorded"
    assert (state_path.parent / "completions" / "wave-1" / "backend.json").exists()
