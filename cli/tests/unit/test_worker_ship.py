"""Tests for fno.worker.blueprint and fno.worker.ship."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest


# ---- Helpers ----

def _make_state(tmp_path: Path, extra: dict | None = None) -> Path:
    """Create a minimal target-state.md in tmp_path/.fno/."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    state = {
        "status": "IN_PROGRESS",
        "session_id": "20260421T120000Z-99999-aabbcc",
        "artifact_shipped": False,
        "auto_merge_approved": False,
        "pr_number": None,
    }
    if extra:
        state.update(extra)
    import yaml
    content = "---\n" + yaml.dump(state, default_flow_style=False) + "---\n# State\n"
    path = state_dir / "target-state.md"
    path.write_text(content)
    return path


# ---- AC1-HP: blueprint returns llm_blueprint action ----

def test_ac1_hp_blueprint_returns_llm_dispatch(tmp_path):
    """blueprint() returns {"action": "llm_blueprint", "plan_path": ...} without writing code."""
    from fno.worker.blueprint import blueprint

    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Test plan\n")

    result = blueprint(plan_path=str(plan_path))

    assert result["action"] == "llm_blueprint"
    assert result["plan_path"] == str(plan_path)
    # CLI must NOT have written any implementation code
    py_files = list(tmp_path.rglob("*.py"))
    assert len(py_files) == 0


def test_ac1_hp_blueprint_nonexistent_plan(tmp_path):
    """blueprint() with a non-existent plan path still returns llm_dispatch (path is for skill)."""
    from fno.worker.blueprint import blueprint

    result = blueprint(plan_path="/nonexistent/plan.md")

    assert result["action"] == "llm_blueprint"
    assert result["plan_path"] == "/nonexistent/plan.md"


# ---- AC2-HP: ship creates PR + artifact + event + state ----

def test_ac2_hp_ship_creates_pr(tmp_path, monkeypatch):
    """ship() calls gh pr create and writes artifact when no existing PR."""
    monkeypatch.chdir(tmp_path)
    state_path = _make_state(tmp_path)

    # Calls: git rev-parse (branch), gh pr list, gh pr create
    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
        MagicMock(returncode=0, stdout="https://github.com/owner/repo/pull/42", stderr=""),  # gh pr create
    ]

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ):
        from fno.worker.ship import ship
        result = ship(
            state_path=state_path,
            title="feat: test feature",
            body="Auto-generated PR body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result["action"] == "pr_created"
    assert result["pr_number"] == 42
    assert result["pr_url"] == "https://github.com/owner/repo/pull/42"

    # gh pr create was called (3rd call, index 2)
    create_call = mock_run.call_args_list[2]
    cmd_args = create_call[0][0]
    assert "gh" in cmd_args
    assert "pr" in cmd_args
    assert "create" in cmd_args


def test_ac2_hp_ship_writes_artifact(tmp_path, monkeypatch):
    """ship() writes .fno/artifacts/ship-{session_id}.md."""
    monkeypatch.chdir(tmp_path)
    state_path = _make_state(tmp_path)
    artifacts_dir = tmp_path / ".fno" / "artifacts"

    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
        MagicMock(returncode=0, stdout="https://github.com/owner/repo/pull/99", stderr=""),  # gh pr create
    ]

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ):
        from fno.worker.ship import ship
        ship(
            state_path=state_path,
            title="feat: artifact test",
            body="body",
            artifacts_dir=artifacts_dir,
        )

    artifact_path = artifacts_dir / "ship-20260421T120000Z-99999-aabbcc.md"
    assert artifact_path.exists()
    content = artifact_path.read_text()
    assert "99" in content


# ---- AC3-EDGE: ship is idempotent (no duplicate PR) ----

def test_ac3_edge_ship_no_duplicate_pr(tmp_path, monkeypatch):
    """ship() detects existing PR and does NOT call gh pr create again."""
    monkeypatch.chdir(tmp_path)
    state_path = _make_state(tmp_path)

    existing_pr = [{"number": 55, "url": "https://github.com/owner/repo/pull/55", "state": "OPEN"}]
    mock_run = MagicMock()
    # All subprocess.run calls return the same mock (pr list returns existing PR,
    # git rev-parse also returns OK - use side_effect for ordered calls)
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout=json.dumps(existing_pr), stderr=""),  # gh pr list
    ]

    with patch("subprocess.run", mock_run):
        from importlib import reload
        import fno.worker.ship as ship_mod
        reload(ship_mod)
        result = ship_mod.ship(
            state_path=state_path,
            title="feat: idempotent",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    # Should return existing PR info
    assert result["pr_number"] == 55
    # Only 2 calls made (git + gh pr list), no gh pr create
    assert mock_run.call_count == 2
    calls = [str(c) for c in mock_run.call_args_list]
    assert not any("create" in c for c in calls)


# ---- AC4-HP: ship arms auto-merge when approved ----

def test_ac4_hp_ship_arms_automerge(tmp_path, monkeypatch):
    """When auto_merge_approved=true, ship() sets merge flag in result."""
    monkeypatch.chdir(tmp_path)
    state_path = _make_state(tmp_path, {"auto_merge_approved": True})

    mock_run = MagicMock()
    # Calls: git rev-parse, gh pr list, gh pr create, gh pr merge
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
        MagicMock(returncode=0, stdout="https://github.com/owner/repo/pull/77", stderr=""),  # gh pr create
        MagicMock(returncode=0, stdout="", stderr=""),                 # gh pr merge
    ]

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ):
        from importlib import reload
        import fno.worker.ship as ship_mod
        reload(ship_mod)
        result = ship_mod.ship(
            state_path=state_path,
            title="feat: automerge",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result.get("auto_merge_armed") is True


# ---- x-a166: ship stamps the backlog node <-> PR link ----

def _make_state_with_node(tmp_path: Path, node_id: str) -> Path:
    """target-state.md with graph_node_id appended to the BODY (below the
    frontmatter), exactly as init-target-state.sh writes it."""
    path = _make_state(tmp_path)
    with path.open("a", encoding="utf-8") as fh:
        fh.write(f"graph_node_id: {node_id}\n")
    return path


def test_ac2_hp_ship_stamps_node_pr_link(tmp_path):
    """AC2-HP: with a graph_node_id, ship() runs `fno backlog update --pr-number`,
    then stamps the ship-phase lifecycle provenance (x-b6e4)."""
    state_path = _make_state_with_node(tmp_path, "x-1a2b")

    # Calls: git rev-parse, gh pr list, gh pr create, fno backlog update,
    #        fno backlog session add (ship-phase provenance, x-b6e4).
    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
        MagicMock(returncode=0, stdout="https://github.com/owner/repo/pull/42", stderr=""),  # gh pr create
        MagicMock(returncode=0, stdout="", stderr=""),                 # fno backlog update
        MagicMock(returncode=0, stdout="", stderr=""),                 # fno backlog session add
    ]

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ):
        from fno.worker.ship import ship
        result = ship(
            state_path=state_path,
            title="feat: test",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result["action"] == "pr_created"
    stamp_cmd = mock_run.call_args_list[3][0][0]
    assert stamp_cmd == [
        "fno", "backlog", "update", "x-1a2b",
        "--pr-number", "42",
        "--pr-url", "https://github.com/owner/repo/pull/42",
    ]
    # x-b6e4: ship-phase provenance stamped with ambient identity (no explicit
    # --session-id -- it records the conversation id, not the target run id).
    sess_cmd = mock_run.call_args_list[4][0][0]
    assert sess_cmd == ["fno", "backlog", "session", "add", "x-1a2b", "--phase", "ship"]


def test_ac2_err_stamp_failure_never_fails_ship(tmp_path):
    """AC2-ERR: a non-zero stamp is logged but ship still reports the PR created."""
    state_path = _make_state_with_node(tmp_path, "x-1a2b")

    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
        MagicMock(returncode=0, stdout="https://github.com/owner/repo/pull/42", stderr=""),  # gh pr create
        MagicMock(returncode=1, stdout="", stderr="graph locked"),     # fno backlog update FAILS
        MagicMock(returncode=1, stdout="", stderr="graph locked"),     # session add also fails
    ]

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ):
        from fno.worker.ship import ship
        result = ship(
            state_path=state_path,
            title="feat: test",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result["action"] == "pr_created"
    assert result["pr_number"] == 42


def test_no_node_id_skips_stamp(tmp_path):
    """No graph_node_id (the pre-x-a166 manifest) => no stamp subprocess is issued."""
    state_path = _make_state(tmp_path)  # no graph_node_id in body

    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
        MagicMock(returncode=0, stdout="https://github.com/owner/repo/pull/42", stderr=""),  # gh pr create
    ]

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(0, None)
    ):
        from fno.worker.ship import ship
        ship(
            state_path=state_path,
            title="feat: test",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    # Exactly 3 subprocess calls: no 4th (stamp) call.
    assert mock_run.call_count == 3


# ---- AC1-UI: a stale base short-circuits before gh pr create ----

def test_stale_base_refusal_blocks_pr_create(tmp_path, monkeypatch):
    """A stale base makes ship() error out with the refusal text and never
    reach gh pr create."""
    monkeypatch.chdir(tmp_path)
    state_path = _make_state(tmp_path)

    mock_run = MagicMock()
    mock_run.side_effect = [
        MagicMock(returncode=0, stdout="feature/test\n", stderr=""),  # git rev-parse
        MagicMock(returncode=0, stdout="[]", stderr=""),               # gh pr list
    ]
    refusal = "stale base: your branch's merge-base with origin/main is 3.0 days behind"

    with patch("subprocess.run", mock_run), patch(
        "fno.pr._preflight.check_stale_base", return_value=(3, refusal)
    ):
        from importlib import reload
        import fno.worker.ship as ship_mod
        reload(ship_mod)
        result = ship_mod.ship(
            state_path=state_path,
            title="feat: stale",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result["action"] == "error"
    assert refusal in result["error"]
    # gh pr create was never invoked (only git rev-parse + gh pr list ran).
    assert mock_run.call_count == 2
    assert not any("create" in str(c) for c in mock_run.call_args_list)


# ---- F2: incarnation fence blocks PR creation ----

def test_incarnation_fence_blocks_pr_create(tmp_path, monkeypatch):
    """F2: a losing incarnation (its session claim held by another) never reaches
    gh pr create / gh pr list / git rev-parse - the fence refuses first."""
    monkeypatch.chdir(tmp_path)
    state_path = _make_state(tmp_path)

    mock_run = MagicMock()  # any subprocess.run call means an outward action slipped through

    with patch("subprocess.run", mock_run), patch(
        "fno.claims.incarnation.resolve_fence_session_uuid", return_value="transcript-uuid"
    ), patch(
        "fno.claims.incarnation.incarnation_fence_blocks",
        return_value=(True, "incarnation-fence: session:transcript-uuid held by other"),
    ):
        from fno.worker.ship import ship

        result = ship(
            state_path=state_path,
            title="feat: fenced",
            body="body",
            artifacts_dir=tmp_path / ".fno" / "artifacts",
        )

    assert result["action"] == "blocked"
    assert "incarnation-fence" in result["error"]
    assert mock_run.call_count == 0  # no outward action ran
