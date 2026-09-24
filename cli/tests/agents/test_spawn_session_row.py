"""A node-bearing spawn opens a contributor session row.

A worker that never holds the claim crosses no stamping chokepoint (claim
acquire/release, plan-bind, PR-link). Reviews stay inline in the builder's
session and do not create review-worker rows. Coverage:

  - AC1: `spawn --node X --substrate bg` with a resolvable worker uuid opens a
    row carrying the WORKER's harness session id (never the spawner's, never
    the 8-hex short id). Phase: the message's verb labels the row via the
    spawn_phase.toml table - do, review, blueprint, think, ship all stamp
    their spellings; an unlabeled --node spawn is refused (x-007c), so no
    row is ever born mislabeled or driverless.
  - Review labels and review seeds refuse before launch; invalid phases also
    refuse before launch.
  - AC2: `session add --phase review` stamps (exit 2 before the enum gained
    review), and the roster renders the review slot between do and ship.
  - Closing: `session reap-open --phase review` fills ended_at and KEEPS the
    row; `--phase all` settles a dead session's do AND review windows in one
    call (the daemon observer's spelling).
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


NODE = "x-4ab1"
FULL_UUID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"


@pytest.fixture(autouse=True)
def _readable_node_row(monkeypatch):
    """A graph row for NODE, so the verb seam's `--node` consult passes.

    The seam refuses a `--node` spawn whose row it cannot read; these tests
    drive the door for the session ROW, not the verb contract, so every test
    sees one target-deriving row unless it stubs its own (the blueprint tests
    re-stub with a medium row so the derived verb agrees with their seed)."""
    row = {"id": NODE, "slug": "sess", "dispatch_verb": "/target", "difficulty": "low"}

    def _load_graph():
        return [dict(row)]

    monkeypatch.setattr("fno.graph.load.load_graph", _load_graph)
    yield row


@pytest.fixture(autouse=True)
def _isolated_claims_root(tmp_path, monkeypatch):
    """Pin the global claims root at the tmp home.

    node:/dispatch: claims root at $FNO_CLAIMS_ROOT regardless of the tmp fno
    home, so without this the spawn guard's reservation lands in the machine's
    live claims store (and a run killed mid-flight leaves it held). Pinned
    here, both directions stay in tmp; the release sweep below is belt-only.
    """
    claims = tmp_path / "claims"
    claims.mkdir()
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(claims))
    yield
    from fno.claims.core import claim_status, release_claim
    from fno.claims.io import claims_root_for

    for key in (f"node:{NODE}", f"dispatch:{NODE}"):
        try:
            root = claims_root_for(key)
            holder = claim_status(key, root=root).get("holder")
            if holder:
                release_claim(key, holder=holder, root=root)
        except Exception:
            pass


def _seed_graph() -> None:
    """One scratch node in the tmp-home graph the whole suite resolves to."""
    from fno import paths

    g = paths.graph_json()
    g.parent.mkdir(parents=True, exist_ok=True)
    g.write_text(
        json.dumps({"entries": [{
            "id": NODE, "title": "scratch provenance target",
            "type": "feature", "project": "fno", "status": "ready",
        }]}),
        encoding="utf-8",
    )


def _node_rows() -> list[dict]:
    from fno import paths
    from fno.graph.store import read_graph_strict

    return next(e for e in read_graph_strict(paths.graph_json()) if e["id"] == NODE).get(
        "sessions", []
    )


@pytest.fixture
def graph_cli_home(tmp_path: Path, monkeypatch) -> Path:
    """Graph-side CLI tests: tmp home, seeded node, and the CLI graph path
    pinned to that tmp graph.

    The pin matters: sibling suites setattr the lazy GRAPH_JSON module attr,
    whose monkeypatch teardown plants a concrete value in the module dict and
    silences __getattr__ for the rest of the process - without the pin, these
    tests read whatever graph the previous file pinned (changed-smoke runs
    its whole changed subset in one process).
    """
    import fno.graph.cli as graph_cli
    from fno import paths

    use_tmpdir(monkeypatch, tmp_path)
    _seed_graph()
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: paths.graph_json())
    return tmp_path


@pytest.fixture
def workdir_claude(tmp_path: Path, monkeypatch) -> Path:
    """Isolated fno home + graph with a scratch node + fake claude on PATH."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    _seed_graph()
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    # Keep the native event-store writer reachable alongside the fake provider.
    monkeypatch.setenv("PATH", os.pathsep.join((str(bin_dir), os.environ.get("PATH", ""))))
    return tmp_path


@pytest.fixture
def resolvable_uuid(monkeypatch):
    """Make the fake claude's 8-hex job id resolve to a full session uuid."""
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "resolve_session_uuid", lambda short_id: FULL_UUID)


# ---------------------------------------------------------------------------
# AC1: --node opens the row with the worker's own session id
# ---------------------------------------------------------------------------


@pytest.mark.dev_build
def test_spawn_with_node_and_review_verb_is_refused(
    workdir_claude, native_backlog_door, monkeypatch
) -> None:
    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "row-worker", "-H", "claude", "--substrate", "bg",
            "--effort", "xhigh",
            "--node", NODE, "/code-review this diff",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 89, result.output
    assert '"reason":"review_session"' in result.output.replace(" ", "")
    assert load_registry() == []
    assert _node_rows() == []
    for key in (f"node:{NODE}", f"dispatch:{NODE}"):
        assert claim_status(key, root=claims_root_for(key)).get("holder") is None


@pytest.mark.dev_build
@pytest.mark.parametrize(
    ("phase", "seed"),
    [("review", "/fno:triage deep"), ("do", "/code-review this diff")],
)
def test_spawn_review_label_or_seed_is_refused(
    workdir_claude, native_backlog_door, monkeypatch, phase, seed
) -> None:
    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "review-probe", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "--session-phase", phase, seed,
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 89, result.output
    assert '"reason":"review_session"' in result.output.replace(" ", "")
    assert load_registry() == []
    assert _node_rows() == []
    for key in (f"node:{NODE}", f"dispatch:{NODE}"):
        assert claim_status(key, root=claims_root_for(key)).get("holder") is None


def test_spawn_with_prose_and_node_composes_a_labeled_seed(
    workdir_claude, resolvable_uuid
) -> None:
    """Arbitrary prose with a `--node` is no longer unlabelable: the verb
    seam composes the node's command in front, so the seed names the verb
    and the row stamps from it instead of refusing a lying default."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "row-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "review this diff",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1, f"one row for the composed seed: {_node_rows()!r}"
    assert rows[0]["phase"] == "do"
    assert rows[0]["session_id"] == FULL_UUID
    assert rows[0]["started_at"]
    assert rows[0].get("ended_at") is None


def test_stamp_duplicate_fill_keeps_one_row(workdir_claude, resolvable_uuid) -> None:
    """The retried-stamp shape: the same worker identity stamped twice (a
    retried spawn, a re-run dispatcher) collapses onto one row. The registry
    refuses a second spawn under one session id - one session IS one worker -
    so the second stamp goes through the helper the spawn path calls."""
    from fno.agents.cli import _stamp_spawned_session_row, agents_app

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "row-retry", "-H", "claude", "--substrate", "bg",
         "--node", NODE,
         f"/fno:think {NODE} please"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert len(_node_rows()) == 1

    _stamp_spawned_session_row(
        node=NODE, message="", phase="think",
        worker_name="row-retry", worker_harness="claude",
        worker_session_uuid=FULL_UUID,
    )

    rows = _node_rows()
    assert len(rows) == 1, f"duplicate stamp must not add a row, got {rows!r}"
    assert rows[0]["session_id"] == FULL_UUID


def test_spawn_without_uuid_parks_the_row(workdir_claude) -> None:
    """No resolvable full uuid (the autouse stub answers None): no node row
    opens at spawn, and the owed payload parks on the worker's registry row
    for SessionStart's first id observation to open. The spawn stays silent."""
    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "nouuid-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:think this diff",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert _node_rows() == []
    assert "session row open skipped" not in result.stderr
    row = next(r for r in load_registry() if r.name == "nouuid-worker")
    assert row.pending_session_row["phase"] == "think"
    assert row.pending_session_row["merge_grant"] is None
    assert row.pending_session_row["started_at"]


def test_stamp_no_worker_name_skips_named(workdir_claude, capsys) -> None:
    """With no worker_name there is no registry row to park on: today's named
    skip, and nothing written."""
    from fno.agents.cli import _stamp_spawned_session_row

    _stamp_spawned_session_row(
        node=NODE, message="", phase="review",
        worker_name=None, worker_harness="claude",
        worker_session_uuid=None,
    )
    assert "session row open skipped" in capsys.readouterr().err
    assert _node_rows() == []


def test_spawn_prose_prompt_names_nothing_stays_silent(
    workdir_claude, resolvable_uuid
) -> None:
    """Prose naming a node without a mapped seed or --node writes no row."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "prose-worker", "-H", "claude", "--substrate", "bg",
         f"look at {NODE} and report"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert _node_rows() == []
    assert "session row open skipped" not in result.stderr


def test_spawn_target_family_stamps_do(workdir_claude, resolvable_uuid) -> None:
    """A /target-family payload names a do worker: the row stamps do (the
    worker's own claim-acquire stamp duplicate-fills it), never review."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "do-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:target resume",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "do"
    assert rows[0]["session_id"] == FULL_UUID


def test_spawn_unlabelable_verb_refuses_before_spawn(
    workdir_claude, resolvable_uuid
) -> None:
    """AC4 (x-007c): a /fno:triage worker with --node is none of the table's
    verbs, so the spawn refuses fail-closed before anything launches: exit 2,
    the flag and the allowed values on stderr, no registry row, no claims
    taken, no sessions row."""
    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "triage-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:triage deep",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 2, result.output
    assert "--session-phase" in result.stderr
    assert "No worker launched" in result.stderr
    for phase in ("do", "review", "blueprint", "think", "ship"):
        assert phase in result.stderr, phase
    assert load_registry() == []  # nothing launched
    assert _node_rows() == []
    for key in (f"node:{NODE}", f"dispatch:{NODE}"):
        assert claim_status(key, root=claims_root_for(key)).get("holder") is None, key


def _medium_row(monkeypatch) -> None:
    """Re-stub the graph row so the seam derives /blueprint, matching the
    blueprint seed this test types."""
    row = {"id": NODE, "slug": "sess", "dispatch_verb": "", "difficulty": "medium"}

    def _load_graph():
        return [dict(row)]

    monkeypatch.setattr("fno.graph.load.load_graph", _load_graph)


def test_spawn_bare_blueprint_spelling_stamps_blueprint(
    workdir_claude, resolvable_uuid, monkeypatch
) -> None:
    """AC3 (x-007c): the bare /blueprint spelling - what claude and agy
    autonomous dispatch render - stamps the blueprint row at dispatch time."""
    from fno.agents.cli import agents_app

    _medium_row(monkeypatch)

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "bp-bare-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/blueprint x-4ab1",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "blueprint"
    assert rows[0]["session_id"] == FULL_UUID


def test_spawn_explicit_phase_rescues_unmapped_verb(
    workdir_claude, resolvable_uuid
) -> None:
    """AC5 (x-007c): an explicit --session-phase on an unmapped verb is the
    operator's label and stamps the row instead of refusing."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "triage-labeled", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "--session-phase", "think", "/fno:triage deep",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "think"


def test_spawn_think_verb_stamps_think(workdir_claude, resolvable_uuid) -> None:
    """A /fno:think worker names a think planner: the phase is in the
    vocabulary, and the retirement's planning lane keys on it."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "think-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:think deep",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "think"


def test_spawn_blueprint_verb_stamps_blueprint(workdir_claude, resolvable_uuid, monkeypatch) -> None:
    """A /fno:blueprint worker names a blueprint planner: the row stamps the
    planning phase instead of skipping."""
    from fno.agents.cli import agents_app

    _medium_row(monkeypatch)

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "bp-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:blueprint x-5baf",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "blueprint"


def test_spawn_codex_blueprint_spelling_stamps_blueprint(
    workdir_claude, resolvable_uuid, monkeypatch
) -> None:
    """The codex spelling travels on the normalized form: `$fno:blueprint`
    stamps the same planning row the slash spelling does."""
    from fno.agents.cli import agents_app

    _medium_row(monkeypatch)

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "bp-codex-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "$fno:blueprint the plan doc",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "blueprint"


def test_spawn_no_node_anywhere_writes_nothing_and_stays_silent(
    workdir_claude, resolvable_uuid
) -> None:
    """An ad-hoc spawn names no node: no row and no skip line (nothing to say)."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "adhoc-worker", "-H", "claude", "--substrate", "bg",
         "just a prose prompt"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert _node_rows() == []
    assert "session row open skipped" not in result.stderr


def test_spawn_bad_session_phase_refuses_before_spawn(workdir_claude) -> None:
    """--session-phase is validated against the enum fail-closed (exit 2)."""
    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "badphase-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "--session-phase", "verif", "review this",
        ],
    )
    assert result.exit_code == 2
    assert "--session-phase" in result.stderr
    assert load_registry() == []  # nothing launched
    assert _node_rows() == []


# ---------------------------------------------------------------------------
# AC2: the enum and the roster
# ---------------------------------------------------------------------------


def test_session_add_accepts_review_phase(graph_cli_home) -> None:
    """`session add --phase review` stamps and exits 0 (exit 2 before x-4342)."""
    import fno.graph.cli as graph_cli

    result = CliRunner().invoke(
        graph_cli.cli,
        [
            "session", "add", NODE, "--phase", "review",
            "--harness", "claude", "--session-id", FULL_UUID,
            "--started-at", "2026-08-23T10:00:00Z", "--json",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1 and rows[0]["phase"] == "review"


def test_roster_renders_review_between_do_and_ship(graph_cli_home) -> None:
    """The lifecycle roster gains a review slot between do and ship."""
    import fno.graph.cli as graph_cli

    CliRunner().invoke(
        graph_cli.cli,
        ["session", "add", NODE, "--phase", "do",
         "--harness", "codex", "--session-id", "c" * 32,
         "--started-at", "2026-08-23T09:00:00Z", "--ended-at", "2026-08-23T10:00:00Z"],
        catch_exceptions=False,
    )
    CliRunner().invoke(
        graph_cli.cli,
        ["session", "add", NODE, "--phase", "review",
         "--harness", "claude", "--session-id", FULL_UUID,
         "--started-at", "2026-08-23T10:00:00Z", "--ended-at", "2026-08-23T11:00:00Z"],
        catch_exceptions=False,
    )
    CliRunner().invoke(
        graph_cli.cli,
        ["session", "add", NODE, "--phase", "ship",
         "--harness", "codex", "--session-id", "d" * 32,
         "--started-at", "2026-08-23T11:00:00Z", "--ended-at", "2026-08-23T12:00:00Z"],
        catch_exceptions=False,
    )

    lines, summary = graph_cli._lifecycle_roster(_node_rows())
    text = "\n".join(lines)
    assert "review" in text and "claude" in text
    assert text.index("do") < text.index("review") < text.index("ship")
    phases = [p["phase"] for p in summary["phases"] if isinstance(p, dict)] if isinstance(
        summary.get("phases"), list) else []
    assert "review" in phases


# ---------------------------------------------------------------------------
# Closing: reap-open fills ended_at for review, removes only for do
# ---------------------------------------------------------------------------


def test_reap_open_fills_review_row_and_keeps_it(graph_cli_home) -> None:
    import fno.graph.cli as graph_cli

    CliRunner().invoke(
        graph_cli.cli,
        ["session", "add", NODE, "--phase", "review",
         "--harness", "claude", "--session-id", FULL_UUID,
         "--started-at", "2026-08-23T10:00:00Z"],
        catch_exceptions=False,
    )

    result = CliRunner().invoke(
        graph_cli.cli,
        ["session", "reap-open", NODE, "--harness", "claude",
         "--session-id", FULL_UUID, "--phase", "review", "--json"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["row_closed"] is True
    assert receipt["row_removed"] is False

    rows = _node_rows()
    assert len(rows) == 1, "a reaped review row is closed, never erased"
    assert rows[0]["ended_at"], "the close fills ended_at"
    assert rows[0]["phase"] == "review"


def test_reap_open_all_closes_every_open_row(graph_cli_home) -> None:
    """The death-cascade spelling: one session holding a do window AND a review
    window settles both - each row filled and kept."""
    import fno.graph.cli as graph_cli

    for phase in ("do", "review"):
        CliRunner().invoke(
            graph_cli.cli,
            ["session", "add", NODE, "--phase", phase,
             "--harness", "claude", "--session-id", FULL_UUID,
             "--started-at", "2026-08-23T10:00:00Z"],
            catch_exceptions=False,
        )

    result = CliRunner().invoke(
        graph_cli.cli,
        ["session", "reap-open", NODE, "--harness", "claude",
         "--session-id", FULL_UUID, "--phase", "all", "--json"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["row_removed"] is False and receipt["row_closed"] is True

    rows = _node_rows()
    assert len(rows) == 2, f"both rows survive filled, got {rows!r}"
    assert all(row["ended_at"] for row in rows)


def test_reap_open_do_fills_and_keeps(graph_cli_home) -> None:
    """The do flavor fills ended_at and keeps the row (status unwedges either
    way, and the provenance survives), default phase."""
    import fno.graph.cli as graph_cli

    CliRunner().invoke(
        graph_cli.cli,
        ["session", "add", NODE, "--phase", "do",
         "--harness", "codex", "--session-id", "c" * 32,
         "--started-at", "2026-08-23T10:00:00Z"],
        catch_exceptions=False,
    )

    result = CliRunner().invoke(
        graph_cli.cli,
        ["session", "reap-open", NODE, "--harness", "codex",
         "--session-id", "c" * 32, "--json"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["row_removed"] is False
    assert receipt["row_closed"] is True
    rows = _node_rows()
    assert len(rows) == 1 and rows[0]["ended_at"]


# ---------------------------------------------------------------------------
# The spawner records the durable merge grant on the do row
# ---------------------------------------------------------------------------


def test_spawn_do_row_records_refusal_without_config(workdir_claude, resolvable_uuid) -> None:
    """No standing grant: the row still records an EXPLICIT approved=false, so
    absence-on-a-row never has to be guessed at resolve time."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "do-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:target resume",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    grant = _node_rows()[0]["merge_grant"]
    assert grant["approved"] is False
    assert grant["source"] == "none"
    assert grant["recorded_by"]
    assert grant["recorded_at"]


def test_spawn_do_row_records_config_grant(workdir_claude, resolvable_uuid, monkeypatch) -> None:
    """enabled=true + grant=dispatch: the row records the positive grant with
    source naming the config."""
    from fno.agents.cli import agents_app
    from fno.config import AutoMergeBlock, load_settings

    real = load_settings

    def granted_settings():
        return real().model_copy(
            update={"auto_merge": AutoMergeBlock(enabled=True, grant="dispatch")}
        )

    monkeypatch.setattr("fno.config.load_settings", granted_settings)

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "do-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:target resume",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    grant = _node_rows()[0]["merge_grant"]
    assert grant["approved"] is True
    assert grant["source"] == "config"


def test_spawn_no_merge_flag_outranks_config_grant(workdir_claude, resolvable_uuid, monkeypatch) -> None:
    """A /target message carrying --no-merge records approved=false with the
    flag named as the source, even while the standing config would grant
    (AC9-EDGE's newer refusal)."""
    from fno.agents.cli import agents_app
    from fno.config import AutoMergeBlock, load_settings

    real = load_settings

    def granted_settings():
        return real().model_copy(
            update={"auto_merge": AutoMergeBlock(enabled=True, grant="dispatch")}
        )

    monkeypatch.setattr("fno.config.load_settings", granted_settings)

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "do-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:target resume --no-merge",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    grant = _node_rows()[0]["merge_grant"]
    assert grant["approved"] is False
    assert grant["source"] == "no-merge-flag"


def test_spawn_think_row_carries_no_grant(workdir_claude, resolvable_uuid) -> None:
    """A think worker never merges; its row records no grant at all."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "row-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "/fno:think this diff",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert "merge_grant" not in _node_rows()[0]
