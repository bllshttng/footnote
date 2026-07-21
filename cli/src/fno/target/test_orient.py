"""Tests for the target orientation report (x-a7be, change A)."""
from __future__ import annotations

import os
from pathlib import Path

from fno.target import orient


def test_render_aligns_all_six_lines() -> None:
    lines = [orient.OrientLine("node", "fresh"), orient.OrientLine("done-when", "x")]
    out = orient.render(lines)
    assert "node:" in out and "done-when:" in out
    # labels right-padded to a common width
    assert out.splitlines()[0].startswith("node:     ")


def test_node_line_no_node() -> None:
    assert orient._node_line(None, Path("/")).startswith("fresh")


def test_node_line_not_in_graph(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: None)
    line = orient._node_line("x-zzzz", Path("/"))
    assert "unknown" in line and "fno backlog get x-zzzz" in line


def test_node_line_shipped(monkeypatch) -> None:
    monkeypatch.setattr(
        orient, "_graph_entry", lambda *_: {"status": "done", "pr_number": 42}
    )
    assert orient._node_line("x-1", Path("/"), manifest_raw={}) == "shipped (PR #42 merged)"


def test_node_line_done_without_pr(monkeypatch) -> None:
    # A done node with no PR (advisory/no-ship/manual) must read terminal, not
    # fall through to in-progress/fresh.
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: {"status": "done"})
    raw = {"target_claim_key": "node:x-1", "target_claim_holder": "ts:me"}
    line = orient._node_line("x-1", Path("/"), manifest_raw=raw)
    assert line == "done (no PR)"


def test_node_line_half_done(monkeypatch) -> None:
    monkeypatch.setattr(
        orient, "_graph_entry", lambda *_: {"status": "ready", "pr_number": 7}
    )
    assert orient._node_line("x-1", Path("/"), manifest_raw={}) == "half-done (PR #7)"


def test_node_line_in_progress_from_manifest_claim(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: {"status": "ready"})
    raw = {"target_claim_key": "node:x-1", "target_claim_holder": "target-session:abc"}
    line = orient._node_line("x-1", Path("/"), manifest_raw=raw)
    assert "in-progress" in line and "target-session:abc" in line


def test_node_line_graph_error_degrades(monkeypatch) -> None:
    def boom(*_):
        raise RuntimeError("graph blew up")

    monkeypatch.setattr(orient, "_graph_entry", boom)
    line = orient._node_line("x-1", Path("/"), manifest_raw={})
    assert "unknown" in line and "resolve:" in line


def test_attended_line_from_manifest() -> None:
    assert orient._attended_line({"attended": True}).startswith("true")
    assert orient._attended_line({"attended": False}).startswith("false")


def test_attended_line_authority_grant(monkeypatch) -> None:
    # x-6390: `/target beastmode` stamps `authority: full`; the orienter surfaces it
    # on the attended line so /think and /blueprint read one posture, not two.
    monkeypatch.setattr(orient, "_claim_state", lambda _k: "live")
    raw = {"attended": False, "authority": "full", "target_claim_key": "node:x-1"}
    assert "authority: full" in orient._attended_line(raw)
    assert "authority" not in orient._attended_line({"attended": False})


def test_authority_fails_closed_on_claimless_abandoned_manifest(monkeypatch) -> None:
    """x-6390: a free-text `/target beastmode` records NO claim, and a claimless
    manifest with a dead transient pid reads `live` by design (right for
    attended, wrong for authority). Without a fail-closed rule an abandoned run
    advertises its grant forever - the x-4af4 stale-autonomy bug, reintroduced.
    """
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: False)
    raw = {"attended": True, "authority": "full", "owner_pid": "999999"}
    # the manifest still reads live...
    assert orient._manifest_liveness(raw)[0] == "live"
    # ...but the grant does not survive the missing proof of life.
    assert orient._authority_granted(raw) is False
    assert "authority" not in orient._attended_line(raw)


def test_authority_granted_when_life_is_proven(monkeypatch) -> None:
    """The converse: a live claim is real proof, so the grant stands.
    Fail-closed must not mean never-granted."""
    monkeypatch.setattr(orient, "_claim_state", lambda _k: "live")
    assert orient._authority_granted(
        {"authority": "full", "target_claim_key": "node:x-1"}
    ) is True


def test_a_live_owner_pid_alone_never_grants_authority(monkeypatch) -> None:
    """owner_pid is alive for EVERY session at init time, claimless ones
    included. A pid-based grant therefore reads granted at init and evaporates
    minutes later, so the operator walks away holding a grant that already
    lapsed. Only a claim proves both live-now and survives-this-process."""
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: True)
    assert orient._authority_granted({"authority": "full", "owner_pid": "1"}) is False
    # ...and a claim that is present but not live is equally insufficient.
    monkeypatch.setattr(orient, "_claim_state", lambda _k: "free")
    assert orient._authority_granted(
        {"authority": "full", "target_claim_key": "node:x-1", "owner_pid": "1"}
    ) is False


def test_attended_line_dead_manifest_never_grants_authority(monkeypatch) -> None:
    # x-4af4 liveness lesson: a defunct session's stamped grant must not survive
    # it. Dead manifest resolves to plain attended -- no authority, no autonomy.
    monkeypatch.setattr(orient, "_claim_state", lambda _k: "stale")
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: False)
    raw = {
        "attended": False,
        "authority": "full",
        "target_claim_key": "node:x-1",
        "owner_pid": "999999",
    }
    line = orient._attended_line(raw)
    assert line.startswith("true") and "authority" not in line


def test_attended_line_substrate(monkeypatch) -> None:
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    monkeypatch.delenv("FNO_BG", raising=False)
    monkeypatch.delenv("TARGET_UNATTENDED", raising=False)
    assert orient._attended_line(None).startswith("true")
    monkeypatch.setenv("FNO_AGENT_SELF", "worker-x")
    assert orient._attended_line(None).startswith("false")


# --- T2: live-manifest predicate (x-4af4) -----------------------------------


def test_manifest_liveness_none_when_no_manifest() -> None:
    assert orient._manifest_liveness(None)[0] == "none"
    assert orient._manifest_liveness({})[0] == "none"


def test_manifest_liveness_live_claim_beats_dead_pid(monkeypatch) -> None:
    # AC2-EDGE: a dead owner_pid but a held+unexpired claim is LIVE. Claim-first
    # (x-ba4b): the manifest pid snapshot lies after a supervisor respawn.
    monkeypatch.setattr(orient, "_claim_state", lambda _k: "suspect")
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: False)
    raw = {"target_claim_key": "node:x-1", "owner_pid": "999999"}
    assert orient._manifest_liveness(raw)[0] == "live"


def test_manifest_liveness_dead_claim_and_dead_pid(monkeypatch) -> None:
    # AC2-HP: claim expired AND owner_pid dead -> DEAD (both signals required).
    monkeypatch.setattr(orient, "_claim_state", lambda _k: "stale")
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: False)
    raw = {"target_claim_key": "node:x-1", "owner_pid": "999999"}
    assert orient._manifest_liveness(raw)[0] == "dead"


def test_manifest_liveness_no_claim_key_never_dead(monkeypatch) -> None:
    # Codex P1: a live NON-node target (free-text/plan input) has NO claim key and
    # a dead TRANSIENT owner_pid post-init. owner_pid can only PROVE life, never
    # death - so a no-claim manifest is LIVE whether the pid is alive or dead, and
    # the GC must never archive it.
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: True)
    assert orient._manifest_liveness({"owner_pid": "123"})[0] == "live"
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: False)
    assert orient._manifest_liveness({"owner_pid": "123"})[0] == "live"
    assert orient._manifest_liveness({"graph_node_id": "null"})[0] == "live"


def test_manifest_liveness_claim_read_error_biased_live(monkeypatch) -> None:
    # A claim read error means the claim signal is NOT confirmed-dead, so
    # both-signals-dead can never hold -> LIVE (never archive a maybe-live run).
    monkeypatch.setattr(orient, "_claim_state", lambda _k: None)
    monkeypatch.setattr(orient, "_pid_alive", lambda _p: False)
    raw = {"target_claim_key": "node:x-1", "owner_pid": "999999"}
    assert orient._manifest_liveness(raw)[0] == "live"


def test_claim_state_routes_to_global_node_root(monkeypatch) -> None:
    # Regression: a node:/dispatch: claim lives at the GLOBAL claims root, not the
    # per-repo default. Without routing, _claim_state reads `free` from every
    # worktree and marks a LIVE session dead (would archive its manifest).
    from fno.claims.io import claims_root_for

    captured = {}

    def _fake_status(key, root=None):
        captured["root"] = root
        return {"state": "live"}

    monkeypatch.setattr("fno.claims.core.claim_status", _fake_status)
    assert orient._claim_state("node:x-1") == "live"
    assert captured["root"] == claims_root_for("node:x-1")


def test_attended_line_dead_manifest_is_attended(monkeypatch) -> None:
    # AC2-HP: a dead manifest stamped attended:false STILL resolves to attended,
    # and names the dead manifest so the posture change is not silent.
    monkeypatch.setattr(
        orient, "_manifest_liveness", lambda _r: ("dead", "claim stale + owner_pid dead")
    )
    line = orient._attended_line({"attended": False, "owner_pid": "9"})
    assert line.startswith("true") and "dead manifest" in line


def test_attended_line_live_manifest_keeps_stamp(monkeypatch) -> None:
    # AC1-UI: a live manifest keeps its stamped posture; the note names it live.
    monkeypatch.setattr(orient, "_manifest_liveness", lambda _r: ("live", "claim node:x-1 live"))
    assert orient._attended_line({"attended": False}).startswith("false")
    assert orient._attended_line({"attended": True}).startswith("true")


def test_manifest_live_line_shapes(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_manifest_liveness", lambda _r: ("dead", "why"))
    dead = orient._manifest_live_line({"attended": False})
    assert dead.startswith("dead") and "fno state archive" in dead
    monkeypatch.setattr(orient, "_manifest_liveness", lambda _r: ("none", "no manifest"))
    assert orient._manifest_live_line(None).startswith("none")


def test_worktree_line(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(orient, "_is_linked_worktree", lambda _: False)
    line = orient._worktree_line(tmp_path, "x-9")
    assert "fno target start x-9" in line
    monkeypatch.setattr(orient, "_is_linked_worktree", lambda _: True)
    assert orient._worktree_line(tmp_path, "x-9") == str(tmp_path)


def test_tests_line_detection(tmp_path) -> None:
    assert "unknown" in orient._tests_line(tmp_path)
    (tmp_path / "pyproject.toml").write_text("[project]\n", encoding="utf-8")
    assert "pytest" in orient._tests_line(tmp_path)
    (tmp_path / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
    assert "cargo test" in orient._tests_line(tmp_path)


def test_done_when_advisory() -> None:
    assert "advisory" in orient._done_when_line({"no_ship": "true"}, Path("/"))


def test_done_when_pr_and_handoff(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_required_bots", lambda _: ["codex-bot"])
    line = orient._done_when_line({"attended": False}, Path("/"))
    assert "codex-bot" in line and "hand off" in line


def test_done_when_no_review_gate(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_required_bots", lambda _: [])
    line = orient._done_when_line({"attended": True}, Path("/"))
    assert "PR + CI only" in line and "hand off" not in line


def test_plan_line(tmp_path) -> None:
    assert "none" in orient._plan_line(None, tmp_path)
    plan = tmp_path / "p.md"
    plan.write_text("edits `a/b.py`\n", encoding="utf-8")
    assert "stale-reference" in orient._plan_line(str(plan), tmp_path)


def test_build_report_is_read_only_eight_lines(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: None)
    lines = orient.build_report(tmp_path, node_id="x-1", plan_path=None, manifest_raw={})
    labels = [ln.label for ln in lines]
    assert labels == [
        "node", "attended", "worktree", "tests", "plan",
        "boundary-reconcile", "manifest-live", "done-when",
    ]


def test_boundary_line_no_node() -> None:  # AC4-UI: line always renders
    assert orient._boundary_line(None, None, Path("/")).startswith("fresh")


def test_boundary_line_not_in_graph(monkeypatch) -> None:  # AC8-FR degrade
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: None)
    assert "not in graph" in orient._boundary_line("x-zzzz", None, Path("/"))


def test_boundary_line_graph_error_degrades(monkeypatch) -> None:  # AC8-FR
    def boom(*_):
        raise RuntimeError("graph blew up")

    monkeypatch.setattr(orient, "_graph_entry", boom)
    assert "unknown" in orient._boundary_line("x-1", None, Path("/"))


def test_render_boundary_verdicts() -> None:  # AC4-UI all four verdict shapes
    from fno.plan.boundary import BlockerVerdict

    assert orient._render_boundary([]) == "fresh (no landed blocker to reconcile)"
    stale = orient._render_boundary(
        [BlockerVerdict("x-e317", "stale", pr_number=141, completed_at="2026-07-02T09:12:12+00:00")]
    )
    assert stale == "STALE vs x-e317 (PR #141, merged 2026-07-02) - Step 0 required"
    assert "marker present" in orient._render_boundary([BlockerVerdict("x-1", "reconciled")])
    assert orient._render_boundary(
        [BlockerVerdict("x-1", "unknown", reason="oops")]
    ) == "unknown (x-1: oops)"
    assert "no done blocker" in orient._render_boundary([BlockerVerdict("x-1", "fresh")])


def test_read_manifest_merges_body_keys(tmp_path, monkeypatch) -> None:
    # graph_node_id + target_claim_* live in the manifest BODY, below the
    # frontmatter load_agent_context parses -- _read_manifest must regex them in.
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        '---\nattended: true\n---\n# Target Session State\n'
        'target_claim_key: "node:x-7"\n'
        'target_claim_holder: "target-session:z"\n'
        "graph_node_id: x-7\n",
        encoding="utf-8",
    )

    def _no_frontmatter(*_a, **_k):
        raise RuntimeError("force body-only path")

    monkeypatch.setattr("fno.agent.state.load_agent_context", _no_frontmatter)
    raw = orient._read_manifest(tmp_path)
    assert raw is not None
    assert raw["graph_node_id"] == "x-7"
    assert raw["target_claim_key"] == "node:x-7"
    # and the node line then reports in-progress from that claim
    line = orient._node_line("x-7", tmp_path, manifest_raw=raw)
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: {"status": "ready"})
    assert "in-progress" in orient._node_line("x-7", tmp_path, manifest_raw=raw)


def test_load_orientation_node_override(tmp_path, monkeypatch) -> None:
    monkeypatch.setattr(orient, "_read_manifest", lambda _: {"graph_node_id": "x-manifest"})
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: None)
    lines = orient.load_orientation(tmp_path, node_id="x-override")
    node = next(ln.value for ln in lines if ln.label == "node")
    assert "x-override" in node and "x-manifest" not in node


def test_self_check_runs() -> None:
    orient._self_check()
