"""x-4342: a node-bearing spawn opens a sessions row on the node.

A spawned contributor that never holds the claim crossed no stamping
chokepoint (claim acquire/release, plan-bind, PR-link), so its review work
landed in no sessions array. Coverage:

  - AC1: `spawn --node X --substrate bg` with a resolvable worker uuid opens a
    row carrying the WORKER's harness session id (never the spawner's, never
    the 8-hex short id). Phase: a /target-family message stamps do (the
    worker's claim-acquire stamp fills the same row), anything else stamps
    review.
  - AC1-fallback: a review-verb prompt naming exactly ONE node id (no --node)
    opens the same row; prose or a two-id prompt arms nothing.
  - AC1-ERR: a review prompt naming an unresolvable id exits 0 with a named
    skip on stderr and writes no row; a bad --session-phase refuses before
    anything spawns (exit 2).
  - AC2: `session add --phase review` stamps (exit 2 before the enum gained
    review), and the roster renders the review slot between do and ship.
  - Closing: `session reap-open --phase review` fills ended_at and KEEPS the
    row; `--phase all` settles a dead session's do AND review windows in one
    call (the daemon observer's spelling).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


NODE = "x-4ab1"
FULL_UUID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"


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
    from fno.graph.store import read_graph

    return next(e for e in read_graph(paths.graph_json()) if e["id"] == NODE).get(
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
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def resolvable_uuid(monkeypatch):
    """Make the fake claude's 8-hex job id resolve to a full session uuid."""
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "resolve_session_uuid", lambda short_id: FULL_UUID)


# ---------------------------------------------------------------------------
# AC1: --node opens the row with the worker's own session id
# ---------------------------------------------------------------------------


def test_spawn_with_node_opens_review_row(workdir_claude, resolvable_uuid) -> None:
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "row-worker", "-H", "claude", "--substrate", "bg",
            "--effort", "xhigh",
            "--node", NODE, "review this diff",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output

    rows = _node_rows()
    assert len(rows) == 1, f"expected exactly one sessions row, got {rows!r}"
    row = rows[0]
    assert row["phase"] == "review"
    assert row["harness"] == "claude"
    # The WORKER's full uuid, not the fake's 8-hex short id and not the
    # spawning session's id - the observed_model join needs the full form.
    assert row["session_id"] == FULL_UUID
    assert row["started_at"]
    assert "ended_at" not in row
    assert row["observed_model"].get("kind") != "unreadable"
    assert "effort" in row
    assert row["effort"] == "xhigh"


def test_stamp_duplicate_fill_keeps_one_row(workdir_claude, resolvable_uuid) -> None:
    """The retried-stamp shape: the same worker identity stamped twice (a
    retried spawn, a re-run dispatcher) collapses onto one row. The registry
    refuses a second spawn under one session id - one session IS one worker -
    so the second stamp goes through the helper the spawn path calls."""
    from fno.agents.cli import _stamp_spawned_session_row, agents_app

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "row-retry", "-H", "claude", "--substrate", "bg",
         f"/code-review {NODE} please"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert len(_node_rows()) == 1

    _stamp_spawned_session_row(
        node=NODE, message="", phase="review",
        worker_name="row-retry", worker_harness="claude",
        worker_session_uuid=FULL_UUID,
    )

    rows = _node_rows()
    assert len(rows) == 1, f"duplicate stamp must not add a row, got {rows!r}"
    assert rows[0]["session_id"] == FULL_UUID


def test_spawn_without_uuid_skips_named(workdir_claude) -> None:
    """No resolvable full uuid (the autouse stub answers None): no row, named skip."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "nouuid-worker", "-H", "claude", "--substrate", "bg",
            "--node", NODE, "review this diff",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert _node_rows() == []
    assert "session row open skipped" in result.stderr


# ---------------------------------------------------------------------------
# AC1-fallback / AC1-ERR: prompt-parse path
# ---------------------------------------------------------------------------


def test_spawn_prompt_node_opens_row(workdir_claude, resolvable_uuid) -> None:
    """A review-verb prompt naming one node id (no --node) opens the same row;
    no guard path."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "prompt-worker", "-H", "claude", "--substrate", "bg",
            f"/code-review the diff for {NODE} and drain the threads",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    rows = _node_rows()
    assert len(rows) == 1
    assert rows[0]["phase"] == "review"
    assert rows[0]["session_id"] == FULL_UUID


def test_spawn_prompt_unresolvable_id_skips_named(workdir_claude, resolvable_uuid) -> None:
    """A review-verb prompt naming an id no graph node carries: exit 0, named
    skip, no row."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn", "--name", "ghost-worker", "-H", "claude", "--substrate", "bg",
            "/code-review x-deadbeef please",
        ],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert _node_rows() == []
    assert "session row open skipped" in result.stderr
    assert "x-deadbeef" in result.stderr


def test_spawn_prose_prompt_names_nothing_stays_silent(
    workdir_claude, resolvable_uuid
) -> None:
    """Prose naming a node (no review verb, no --node) arms nothing: the prompt
    lane is conservative because a bare id in prose names a sibling more often
    than a target."""
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


def test_spawn_prompt_two_ids_arms_nothing(workdir_claude, resolvable_uuid) -> None:
    """A review prompt naming TWO node ids is ambiguous; first-by-position would
    assert a reviewer worked on a node the operator never targeted."""
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "twoid-worker", "-H", "claude", "--substrate", "bg",
         f"/review {NODE} then x-4ab2"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    assert _node_rows() == []


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


def test_spawn_unlabelable_verb_skips_named(workdir_claude, resolvable_uuid) -> None:
    """A /think worker with --node is neither do nor review; a guessed label
    would lie on an append-only record, so nothing stamps and the skip names
    the escape hatch."""
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
    assert _node_rows() == []
    assert "session row open skipped" in result.stderr
    assert "--session-phase" in result.stderr


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
    window settles both - the do row removed, the review row filled."""
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
    assert receipt["row_removed"] is True and receipt["row_closed"] is True

    rows = _node_rows()
    assert len(rows) == 1, f"the review row survives, got {rows!r}"
    assert rows[0]["phase"] == "review" and rows[0]["ended_at"]


def test_reap_open_do_default_still_removes(graph_cli_home) -> None:
    """The do flavor keeps its remove semantics (status unwedge), default phase."""
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
    assert json.loads(result.stdout)["row_removed"] is True
    assert _node_rows() == []
