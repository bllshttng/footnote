"""BDD decision-matrix tests for born-with-why /think spawn (x-6a10).

Covers every user story's acceptance criteria for
:func:`fno.provenance.spawn_think.maybe_spawn_think` plus the load-bearing
invariants:

- US1: a node born with its resolved why (transcript pointer, not paraphrase).
- US2: an attended operator gets a one-line handoff, not an autonomous spawn.
- US3: an away operator gets a fire-and-forget bg /think + a forward stamp.
- US4: opt-in, bounded, non-fatal.

Claim isolation: every spawn test routes claims under a tmp FNO_CLAIMS_ROOT +
FNO_REPO_ROOT so the dispatch dedup token never touches the real .fno/claims.
The ``fno agents spawn`` subprocess is always patched at the
``_spawn_think_worker`` seam - no test shells out.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.harness_identity import resolve_harness_identity
from fno.provenance import spawn_think as st
from fno.provenance.resolver import ResolvedTranscript


# ---------------------------------------------------------------------------
# Fixtures / helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Isolate claims + arm the gate + pin presence; return the events path."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_THINK_SPAWN", "1")  # armed by default for tests
    return tmp_path / ".fno" / "events.jsonl"


def _events(events_path: Path) -> list[dict]:
    if not events_path.exists():
        return []
    return [json.loads(ln) for ln in events_path.read_text().splitlines() if ln.strip()]


def _node(**over) -> dict:
    """A generated organic node WITH an origin (eligible by default)."""
    base = {
        "id": "x-2222aaaa",
        "slug": "born-with-why",
        "title": "Idea nodes born with their why",
        "details": "the why lives in the transcript",
        "cwd": "/tmp/proj",
        "source_harness": "claude",
        "source_session_id": "abc12345",
        "source_cwd": "/tmp/sess",
        "source_node_id": "x-0000aaaa",
        "roadmap_id": None,
        "vision_path": None,
    }
    base.update(over)
    return base


def test_assemble_seed_chain_blueprint_appends_chain(monkeypatch):
    # x-edf7 US3: a fan-out seed must instruct the worker to continue into
    # /blueprint + link, else the flagged child stays designless/idea.
    _resolved(monkeypatch, ok=True)
    seed = st.assemble_seed(_node(), chain_blueprint=True)
    assert "/blueprint" in seed.prompt
    assert "FAILED pass" in seed.prompt
    assert "--plan-path" in seed.prompt


def test_assemble_seed_default_has_no_blueprint_chain(monkeypatch):
    _resolved(monkeypatch, ok=True)
    seed = st.assemble_seed(_node())  # default: born-with-why /think only
    assert "/blueprint" not in seed.prompt


def test_assemble_seed_why_digest_present_even_when_transcript_unresolved(monkeypatch):
    # P2: with the origin transcript pruned, the seed must still carry the epic
    # digest so the fan-out worker isn't designing from the title alone.
    _resolved(monkeypatch, ok=False)
    seed = st.assemble_seed(_node(), why_digest="Gate launch on a real plan.\n\nLD1: inline-fill.")
    assert "GROUNDING" in seed.prompt
    assert "Gate launch on a real plan." in seed.prompt
    assert "LD1: inline-fill." in seed.prompt


def test_assemble_seed_project_root_scopes_output_path(monkeypatch, tmp_path):
    # P1: the /think output path resolves from the passed project_root, so a
    # cross-repo child writes into its own repo (a settings.local plansDirectory
    # pin makes the resolution deterministic).
    _resolved(monkeypatch, ok=True)
    (tmp_path / ".claude").mkdir()
    (tmp_path / ".claude" / "settings.local.json").write_text(
        '{"plansDirectory": "child-plans"}'
    )
    seed = st.assemble_seed(_node(), project_root=tmp_path)
    assert str(tmp_path / "child-plans") in seed.output_path


@pytest.fixture
def patch_spawn(monkeypatch: pytest.MonkeyPatch):
    """Patch the spawn seam + forward stamp; return (spawn_calls, stamp_calls)."""
    spawn_calls: list[tuple] = []
    stamp_calls: list[tuple] = []

    def fake_spawn(node_id, prompt, node_cwd, node_slug, reason="birth",
                   invocation_suffix=None, model=None, provider=None, node=None):
        spawn_calls.append(
            (node_id, prompt, node_cwd, node_slug, reason, invocation_suffix, model, provider, node)
        )
        return "deadbeef"

    monkeypatch.setattr(st, "_spawn_think_worker", fake_spawn)
    monkeypatch.setattr(
        st, "_stamp_forward",
        lambda nid, sess, root, output_path=None: stamp_calls.append(
            (nid, sess, root, output_path)
        ),
    )
    return spawn_calls, stamp_calls


def _resolved(monkeypatch, *, ok: bool, path: str = "/x/t.jsonl", reason="not-found"):
    """Pin assemble_seed's transcript resolution result."""
    def fake(harness, sid, cwd, **kw):
        if ok:
            return ResolvedTranscript(harness, sid, cwd, True, transcript_path=path)
        return ResolvedTranscript(harness, sid, cwd, False, reason=reason)
    monkeypatch.setattr(st, "resolve_transcript", fake)


# ---------------------------------------------------------------------------
# US4 - opt-in & bounded
# ---------------------------------------------------------------------------


def test_gate_off_is_complete_noop(iso, monkeypatch, patch_spawn):
    """AC4-HP: gate off -> no event, no spawn, decision=noop."""
    monkeypatch.setenv("FNO_THINK_SPAWN", "0")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "noop" and res.event is None
    assert spawn_calls == []
    assert _events(iso) == []


def test_malformed_config_fails_safe_disabled(monkeypatch, tmp_path):
    """AC4-ERR: a settings read that raises degrades to disabled, never raises."""
    monkeypatch.delenv("FNO_THINK_SPAWN", raising=False)

    def boom():
        raise RuntimeError("settings exploded")

    monkeypatch.setattr("fno.config.load_settings", boom)
    assert st.think_spawn_enabled(project_root=tmp_path, env={}) is False


def test_bulk_intake_skipped(iso, monkeypatch, patch_spawn):
    """AC4-UI: a roadmap/vision intake node is ineligible -> skip{bulk-intake}."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(roadmap_id="rm-1"),
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "bulk-intake"
    assert spawn_calls == []
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["data"]["reason"] == "bulk-intake"


def test_blast_radius_cap(iso, monkeypatch, patch_spawn, capsys):
    """AC4-EDGE: a run over max_per_run skips the rest and logs the truncation.

    Uses REASON_WORK_START: an unforced birth never advances RunState.spawned
    at all (x-42c5 - it never reaches the real spawn seam), so the cap counter
    needs a reason that still spawns to exercise the cap.
    """
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    monkeypatch.setattr(st, "_max_per_run", lambda root: 1)
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    rs = st.RunState()
    env = dict(__import__("os").environ)

    r1 = st.maybe_spawn_think(_node(id="x-a"), reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent, run_state=rs)
    r2 = st.maybe_spawn_think(_node(id="x-b"), reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent, run_state=rs)

    assert r1.decision == "spawned"
    assert r2.decision == "skipped" and r2.reason == "cap-exceeded"
    assert len(spawn_calls) == 1
    assert "blast-radius cap" in capsys.readouterr().err


def test_blast_radius_cap_quiet_suppresses_warning(iso, monkeypatch, patch_spawn, capsys):
    """x-c9d8 (gemini PR#120): quiet=True suppresses the cap warning print so a
    bulk decompose --json over the cap can't pollute a captured stream, while
    the cap is still enforced (over-cap node returns cap-exceeded)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    monkeypatch.setattr(st, "_max_per_run", lambda root: 1)
    _resolved(monkeypatch, ok=True)
    rs = st.RunState()
    env = dict(__import__("os").environ)

    st.maybe_spawn_think(_node(id="x-a"), reason=st.REASON_WORK_START, env=env,
                         events_path=iso, project_root=iso.parent.parent, run_state=rs, quiet=True)
    r2 = st.maybe_spawn_think(_node(id="x-b"), reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent, run_state=rs, quiet=True)

    assert r2.decision == "skipped" and r2.reason == "cap-exceeded"  # cap enforced
    assert "blast-radius cap" not in capsys.readouterr().err          # no leak


def test_dedup_at_most_one_spawn(iso, monkeypatch, patch_spawn):
    """AC4-FR: two evaluations of the same LIFECYCLE (non-birth) dispatch spawn
    at most once. Birth's own repeat-evaluation behavior is covered by
    test_unforced_birth_never_spawns_even_repeated below - birth never reaches
    the dedup claim at all (x-42c5: it resolves to offered before that step)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    env = dict(__import__("os").environ)
    node = _node()

    r1 = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent)
    # second observation: the dispatch:think:<id> TTL token is still live.
    r2 = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent)

    assert r1.decision == "spawned"
    assert r2.decision == "skipped" and r2.reason == "already-claimed"
    assert len(spawn_calls) == 1


def test_unforced_birth_never_spawns_even_repeated(iso, monkeypatch, patch_spawn):
    """x-42c5: an unforced birth trigger always offers, never spawns - including
    on a repeat evaluation of the same node (it never reaches the dedup claim,
    since step 6 returns before step 7 for every unforced birth call)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    env = dict(__import__("os").environ)
    node = _node()

    r1 = st.maybe_spawn_think(node, env=env, events_path=iso,
                              project_root=iso.parent.parent)
    r2 = st.maybe_spawn_think(node, env=env, events_path=iso,
                              project_root=iso.parent.parent)

    assert r1.decision == "offered" and r2.decision == "offered"
    assert spawn_calls == []


# ---------------------------------------------------------------------------
# US1 - node born with resolved why
# ---------------------------------------------------------------------------


def test_resolved_seed_carries_transcript_pointer(iso, monkeypatch, patch_spawn):
    """AC1-HP: resolved claude origin -> seed has the transcript path + node id.

    Uses REASON_WORK_START (a consented lifecycle trigger that still spawns)
    rather than the default birth reason: birth never spawns (x-42c5), so this
    away-spawn assertion needs a reason that still reaches the spawn seam.
    """
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True, path="/real/transcript.jsonl")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned" and res.resolved is True
    prompt = spawn_calls[0][1]
    assert "/real/transcript.jsonl" in prompt  # the POINTER, not a paraphrase
    assert "x-2222aaaa" in prompt
    assert "origin node chain: x-0000aaaa" in prompt


def test_no_origin_skipped(iso, monkeypatch, patch_spawn):
    """AC1-ERR: all provenance pointers null -> skip{no-origin}, no spawn."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    spawn_calls, _ = patch_spawn
    node = _node(source_session_id=None, source_harness=None, source_cwd=None,
                 source_node_id=None)
    res = st.maybe_spawn_think(node, env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "no-origin"
    assert spawn_calls == []
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["data"]["reason"] == "no-origin"


def test_bug_node_skipped_not_design_warranting(iso, monkeypatch, patch_spawn):
    # A `bug` node is a reproduce-and-fix, not a design fan-out: it must skip an
    # automatic /think even when otherwise eligible (the x-0e29 mechanism fix).
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(type="bug"), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "not-design-warranting"
    assert spawn_calls == []


def test_size_s_node_skipped_not_design_warranting(iso, monkeypatch, patch_spawn):
    # A size-S node (even a feature) is too small for a design fan-out.
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(type="feature", size="S"),
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "not-design-warranting"
    assert spawn_calls == []


def test_design_warranting_feature_still_spawns(iso, monkeypatch, patch_spawn):
    # Guard against over-gating: a plain feature (not a bug, not size-S) still
    # fans out to /think exactly as before, on a consented (non-birth) trigger.
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(type="feature", size="M"),
                               reason=st.REASON_WORK_START,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned"
    assert len(spawn_calls) == 1


def test_conversational_bypasses_design_warranting_gate(iso, monkeypatch, patch_spawn):
    # The explicit conversational verb is the operator's opt-in: it dispatches a
    # /think on ANYTHING, including a bug, bypassing the auto-fan-out gate.
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    monkeypatch.setenv("FNO_THINK_SPAWN_ATTENDED", "spawn")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(type="bug", size="S"),
                               reason=st.REASON_CONVERSATIONAL,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned"
    assert len(spawn_calls) == 1


def test_forced_decompose_child_bypasses_design_warranting_gate(iso, monkeypatch, patch_spawn):
    # A `needs_think` decompose child rides chain_blueprint=True as operator
    # consent (decompose is the opt-in). Even a bug/size-S child must get its
    # forced design pass, not be left an unlinked idea by the size gate (Codex P1).
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    monkeypatch.setenv("FNO_THINK_SPAWN_ATTENDED", "spawn")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(type="bug", size="S"),
                               chain_blueprint=True,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned"
    assert len(spawn_calls) == 1


def test_exactly_one_event_per_evaluation(iso, monkeypatch, patch_spawn):
    """AC1-UI: a gate-on evaluation emits exactly one decision event."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                         events_path=iso, project_root=iso.parent.parent)
    assert len(_events(iso)) == 1


def test_unforced_birth_foreign_harness_offers_unresolved(iso, monkeypatch, patch_spawn):
    """x-42c5: an unforced birth trigger with an unresolvable harness still only
    offers (never spawns) - it degrades gracefully to the bare offer line, same
    as any other unforced birth, and the durable event still records resolved=False."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=False, reason="harness-not-supported")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(source_harness="codex"),
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "offered" and res.resolved is False
    assert spawn_calls == []
    evs = _events(iso)
    assert evs[0]["data"]["resolved"] is False


# ---------------------------------------------------------------------------
# US2 - operator present
# ---------------------------------------------------------------------------


def test_attended_offers_line_not_spawn(iso, monkeypatch, patch_spawn, capsys):
    """AC2-HP/UI: attended -> single-line /think offer, no autonomous spawn."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    _resolved(monkeypatch, ok=True, path="/t.jsonl")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "offered" and spawn_calls == []
    assert res.offer_line.startswith("/think x-2222aaaa")
    assert "\n" not in res.offer_line  # single copy-pasteable line (AC2-UI)
    # x-af8d AC1-HP: the offer stderr line is imperative ("nothing spawned"),
    # not the old `handoff ->` status log that the agent misread as a spawn.
    err = capsys.readouterr().err
    assert "OFFER PENDING (nothing spawned)" in err
    assert "/think x-2222aaaa" in err
    assert "handoff ->" not in err
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "think_offered"


def test_attended_quiet_suppresses_print_but_keeps_event(iso, monkeypatch, patch_spawn, capsys):
    """x-c9d8: quiet=True (machine mode, e.g. `decompose --json`) drops the
    human OFFER PENDING print so it can't pollute a captured stream, but the
    durable think_offered event still fires and the decision is unchanged."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    _resolved(monkeypatch, ok=True, path="/t.jsonl")
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent,
                               quiet=True)
    assert res.decision == "offered" and spawn_calls == []   # behavior unchanged
    assert capsys.readouterr().err == ""                     # no human print at all
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "think_offered"  # event preserved


def test_attended_unresolved_degrades_to_bare_line(iso, monkeypatch, patch_spawn):
    """AC2-ERR: attended + unresolved -> bare /think line, resolved=False."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    _resolved(monkeypatch, ok=False)
    res = st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "offered" and res.resolved is False
    assert res.offer_line == "/think x-2222aaaa"


def test_attended_spawn_optin_dispatches(iso, monkeypatch, patch_spawn):
    """AC4-HP (B, x-5d51): attended + config.think_spawn.attended=spawn -> real bg
    /think dispatch instead of the stderr offer line - on a consented (non-birth)
    trigger. Unlike the pre-x-42c5 behavior, birth itself is exempt from this
    opt-in (see test_unforced_birth_ignores_attended_spawn_optin below): the
    operator's ruling is that a birth trigger never auto-spawns, full stop, even
    when this config is armed."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    monkeypatch.setenv("FNO_THINK_SPAWN_ATTENDED", "spawn")
    _resolved(monkeypatch, ok=True)
    spawn_calls, stamp_calls = patch_spawn
    res = st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned" and len(spawn_calls) == 1
    assert len(stamp_calls) == 1
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "think_spawned"
    assert evs[0]["data"]["presence"] == "attended"  # honest about the opt-in source


def test_unforced_birth_ignores_attended_spawn_optin(iso, monkeypatch, patch_spawn):
    """x-42c5: an unforced birth trigger offers even when the operator has
    configured think_spawn.attended=spawn (the B/x-5d51 opt-in). Birth is the
    one trigger with zero human decision behind it, so no config can make it
    auto-spawn - only chain_blueprint=True (a caller-marked consented dispatch,
    e.g. decompose) or a non-birth reason may bypass this."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    monkeypatch.setenv("FNO_THINK_SPAWN_ATTENDED", "spawn")
    _resolved(monkeypatch, ok=True)
    spawn_calls, stamp_calls = patch_spawn
    res = st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "offered"
    assert spawn_calls == [] and stamp_calls == []


def test_attended_mode_env_override_is_authoritative_when_present():
    """gemini PR #33: a PRESENT FNO_THINK_SPAWN_ATTENDED wins over config; a
    set-but-garbage value resolves to 'offer', never leaking to a config spawn."""
    assert st._attended_mode(env={st._ENV_ATTENDED: "spawn"}) == "spawn"
    assert st._attended_mode(env={st._ENV_ATTENDED: "garbage"}) == "offer"
    assert st._attended_mode(env={st._ENV_ATTENDED: ""}) == "offer"


# ---------------------------------------------------------------------------
# US3 - operator away
# ---------------------------------------------------------------------------


def test_unforced_birth_away_never_spawns_no_stamp(iso, monkeypatch, patch_spawn):
    """x-42c5 (the core fix): an unforced birth trigger observed while the
    originating session classifies AWAY - the near-miss scenario - offers
    instead of spawning. Before this fix this was the one path that fired a
    real bg /think worker with zero human decision."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, stamp_calls = patch_spawn
    res = st.maybe_spawn_think(_node(), env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "offered" and res.presence == "away"
    assert spawn_calls == [] and stamp_calls == []
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "think_offered"
    assert evs[0]["data"]["presence"] == "away"


def test_away_work_start_still_spawns_and_stamps(iso, monkeypatch, patch_spawn):
    """AC3-HP/UI: a consented (non-birth) away trigger still fires the real bg
    /think spawn + node stamp + single think_spawned, exactly as birth used to
    before x-42c5 (birth itself now always offers - see the test above)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, stamp_calls = patch_spawn
    res = st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned" and res.think_session == "deadbeef"
    assert len(spawn_calls) == 1
    assert len(stamp_calls) == 1
    nid, sess, root, output_path = stamp_calls[0]
    assert (nid, sess, root) == ("x-2222aaaa", "deadbeef", iso.parent.parent)
    # x-ff83 W1: doc lands in plans-dir; x-8af8: filename ends -<node-id>.md.
    assert output_path and output_path.endswith("-born-with-why-x-2222aaaa.md")
    assert "plans" in output_path
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "think_spawned"
    assert evs[0]["data"]["think_session"] == "deadbeef"


def test_away_prompt_carries_output_path(iso, monkeypatch, patch_spawn):
    """AC1-HP/UI (x-ff83 W1): the headless worker is handed a plans-dir output
    path in its prompt so its /think doc lands where /blueprint mutates it."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START,
                         env=dict(__import__("os").environ),
                         events_path=iso, project_root=iso.parent.parent)
    prompt = spawn_calls[0][1]
    assert "WRITE YOUR /think OUTPUT" in prompt
    assert "-born-with-why-x-2222aaaa.md" in prompt


def test_away_spawn_failure_skips_no_stamp(iso, monkeypatch, patch_spawn):
    """AC3-ERR: a non-zero spawn -> skip{spawn-failed}, no stamp, never raises."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    _, stamp_calls = patch_spawn

    def boom(*a, **k):
        raise st.SpawnError("mesh down")

    monkeypatch.setattr(st, "_spawn_think_worker", boom)
    res = st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "spawn-failed"
    assert stamp_calls == []
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["data"]["reason"] == "spawn-failed"


def test_away_claim_error_skips(iso, monkeypatch, patch_spawn):
    """A raising acquire_claim resolves to skip{claim-error}, never raises."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    monkeypatch.setattr(st, "_claim_is_live", lambda key: False)
    monkeypatch.setattr(
        "fno.claims.core.acquire_claim",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("lock dir gone")),
    )
    res = st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START,
                               env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "claim-error"
    assert spawn_calls == []
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["data"]["reason"] == "claim-error"


def test_away_spawn_failure_releases_reservation(iso, monkeypatch, patch_spawn):
    """AC2-FR analogue: a failed spawn frees dispatch:think:<id> for a retry."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    calls = {"n": 0}

    def flaky(*a, **k):
        calls["n"] += 1
        if calls["n"] == 1:
            raise st.SpawnError("transient")
        return "second-try"

    monkeypatch.setattr(st, "_spawn_think_worker", flaky)
    monkeypatch.setattr(st, "_stamp_forward", lambda *a, **k: None)
    env = dict(__import__("os").environ)
    node = _node()

    r1 = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent)
    r2 = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent)
    assert r1.reason == "spawn-failed"
    assert r2.decision == "spawned"  # reservation was released, retry succeeds


# ---------------------------------------------------------------------------
# Presence classifier (Locked Decision 3)
# ---------------------------------------------------------------------------


def test_presence_bg_env_is_away():
    assert st.classify_presence(env={"FNO_BG": "1"}) == "away"


def test_presence_interactive_session_is_attended(tmp_path):
    assert st.classify_presence(
        env={"CLAUDE_CODE_SESSION_ID": "sid"}, project_root=tmp_path
    ) == "attended"


def test_presence_codex_thread_is_attended(tmp_path):
    assert st.classify_presence(
        env={"CODEX_THREAD_ID": "thread-id"}, project_root=tmp_path
    ) == "attended"


def test_presence_codex_spawned_worker_is_away(tmp_path):
    assert st.classify_presence(
        env={"CODEX_THREAD_ID": "thread-id", "FNO_AGENT_SELF": "think-x-1-foo"},
        project_root=tmp_path,
    ) == "away"


def test_presence_gemini_behavior_remains_away(tmp_path):
    assert st.classify_presence(
        env={"GEMINI_SESSION_ID": "gemini-id"}, project_root=tmp_path
    ) == "away"


def test_presence_no_signal_defaults_away(tmp_path):
    assert st.classify_presence(env={}, project_root=tmp_path) == "away"


def test_presence_spawned_worker_is_away(tmp_path):
    """codex PR #9: a spawned bg worker exposes CLAUDE_CODE_SESSION_ID + the
    FNO_AGENT_SELF marker but NOT FNO_BG and may have no manifest yet; it must
    classify away (else US3's autonomous /think never fires)."""
    assert st.classify_presence(
        env={"CLAUDE_CODE_SESSION_ID": "sid", "FNO_AGENT_SELF": "think-x-1-foo"},
        project_root=tmp_path,
    ) == "away"


def test_presence_owned_autonomous_manifest_is_away(tmp_path):
    """An owned target-state with attended:false classifies away (autonomous)."""
    fno = tmp_path / ".fno"
    fno.mkdir()
    (fno / "target-state.md").write_text(
        "claude_transcript_id: sid-123\nattended: false\n", encoding="utf-8"
    )
    assert st.classify_presence(
        env={"CLAUDE_CODE_SESSION_ID": "sid-123"}, project_root=tmp_path
    ) == "away"


def test_presence_foreign_manifest_ignored(tmp_path):
    """A manifest whose transcript-id != this session is not trusted."""
    fno = tmp_path / ".fno"
    fno.mkdir()
    (fno / "target-state.md").write_text(
        "claude_transcript_id: OTHER\nattended: false\n", encoding="utf-8"
    )
    # Falls through to the interactive-session signal -> attended.
    assert st.classify_presence(
        env={"CLAUDE_CODE_SESSION_ID": "sid-123"}, project_root=tmp_path
    ) == "attended"


def test_codex_identity_never_owns_claude_manifest(tmp_path):
    fno = tmp_path / ".fno"
    fno.mkdir()
    (fno / "target-state.md").write_text(
        "claude_transcript_id: thread-id\nattended: false\n", encoding="utf-8"
    )
    env = {"CODEX_THREAD_ID": "thread-id"}
    assert st._owned_manifest_attended(tmp_path, env) is None
    assert st.classify_presence(env=env, project_root=tmp_path) == "attended"


# ---------------------------------------------------------------------------
# Result invariant
# ---------------------------------------------------------------------------


def test_invalid_decision_event_combo_raises():
    """A mismatched (decision, event) is a loud construction failure."""
    with pytest.raises(ValueError):
        st.ThinkSpawnResult("spawned", st.EVENT_SKIPPED)


# ---------------------------------------------------------------------------
# Robust short_id parsing (gemini PR #9)
# ---------------------------------------------------------------------------


def test_parse_short_id_compact():
    out = '{"name":"think-x-1","short_id":"abc123","provider":"claude"}\n'
    assert st._parse_short_id(out) == "abc123"


def test_parse_short_id_pretty_printed():
    """A pretty-printed receipt (short_id on its own line) must still parse."""
    out = '{\n  "name": "think-x-1",\n  "short_id": "abc123"\n}\n'
    assert st._parse_short_id(out) == "abc123"


def test_parse_short_id_among_noise():
    """A receipt line among banner/log noise is found; noise is ignored."""
    out = 'INFO booting agent\nWARN short_id not ready\n{"short_id":"def456"}\n'
    assert st._parse_short_id(out) == "def456"


def test_parse_short_id_absent():
    assert st._parse_short_id("no json here at all") == ""


# ---------------------------------------------------------------------------
# _spawn_think_worker - dispatch pin passthrough (model/provider on the cmd)
# ---------------------------------------------------------------------------


def _capture_spawn_cmd(monkeypatch) -> list:
    """Patch subprocess.run inside _spawn_think_worker; return the captured cmd."""
    captured: dict = {}

    class _Proc:
        returncode = 0
        stdout = '{"short_id":"abc123"}'
        stderr = ""

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(st.subprocess, "run", fake_run)
    return captured


def test_spawn_worker_default_provider_claude_no_model(monkeypatch):
    """Byte-for-byte default: no pins -> --provider claude, no --model token."""
    cap = _capture_spawn_cmd(monkeypatch)
    st._spawn_think_worker("x-1", "prompt", None, "slug")
    cmd = cap["cmd"]
    assert "--harness" in cmd and cmd[cmd.index("--harness") + 1] == "claude"
    assert "--model" not in cmd


def test_spawn_worker_tags_the_spawn_subprocess_with_its_cause(monkeypatch):
    """x-42c5: the reason this spawn happened rides FNO_SPAWN_TRIGGER in the
    `fno agents spawn` subprocess's own environment, so the registry row it
    writes can record a CAUSE, not just a parent session id. Before this,
    the only evidence for "did a birth trigger spawn this?" was a timestamp
    gap between a node's created_at and a worker's registry row."""
    captured: dict = {}

    class _Proc:
        returncode = 0
        stdout = '{"short_id":"abc123"}'
        stderr = ""

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        captured["env"] = kw.get("env")
        return _Proc()

    monkeypatch.setattr(st.subprocess, "run", fake_run)
    st._spawn_think_worker("x-1", "prompt", None, "slug", reason=st.REASON_WORK_START)
    assert captured["env"]["FNO_SPAWN_TRIGGER"] == "think_spawn:work-start"
    # The parent's other env vars still ride through (a fresh env would strand
    # PATH/HOME and break the subprocess).
    assert captured["env"].get("PATH")


def test_codex_ambient_pointer_keeps_default_worker_provider_claude(
    iso, monkeypatch
):
    """Codex may own the source conversation without becoming the worker provider."""
    identity = resolve_harness_identity({"CODEX_THREAD_ID": "codex-thread-123"})
    assert identity.harness == "codex"

    seen: dict = {}

    def fake_resolve(harness, sid, cwd, **kw):
        seen["pointer"] = (harness, sid, cwd)
        return ResolvedTranscript(
            harness, sid, cwd, True, transcript_path="/live/codex-thread.jsonl"
        )

    class _Proc:
        returncode = 0
        stdout = '{"short_id":"abc123"}'
        stderr = ""

    def fake_run(cmd, **kw):
        seen["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(st, "resolve_transcript", fake_resolve)
    monkeypatch.setattr(st.subprocess, "run", fake_run)
    monkeypatch.setattr(st, "_stamp_forward", lambda *a, **kw: None)
    monkeypatch.setattr(st, "_daily_cap", lambda root: 0)

    res = st.dispatch_conversational(
        _node(),
        session_id=identity.session_id,
        cwd="/tmp/codex-live",
        harness=identity.harness or "claude",
        events_path=iso,
        project_root=iso.parent.parent,
    )

    assert res.decision == "spawned"
    assert seen["pointer"] == ("codex", "codex-thread-123", "/tmp/codex-live")
    cmd = seen["cmd"]
    assert cmd[cmd.index("--harness") + 1] == "claude"
    assert cmd[cmd.index("--substrate") + 1] == "bg"


def test_spawn_worker_threads_model_and_provider(monkeypatch):
    """A dispatch pin reaches the spawn cmd as exact --model / --provider tokens."""
    cap = _capture_spawn_cmd(monkeypatch)
    st._spawn_think_worker("x-1", "prompt", None, "slug",
                           model="glm-4.7", provider="codex")
    cmd = cap["cmd"]
    assert cmd[cmd.index("--harness") + 1] == "codex"
    assert cmd[cmd.index("--model") + 1] == "glm-4.7"


def test_maybe_spawn_threads_node_pins(iso, monkeypatch, patch_spawn):
    """maybe_spawn_think carries node['model']/['provider'] to the spawn seam.

    Uses REASON_WORK_START: an unforced birth (the default reason) never
    reaches the spawn seam at all (x-42c5).
    """
    spawn_calls, _ = patch_spawn
    _resolved(monkeypatch, ok=True)
    monkeypatch.setenv("FNO_BG", "1")  # away -> real spawn
    node = _node(model="glm-4.7", provider="codex")
    res = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, events_path=iso)
    assert res.decision == "spawned"
    # tuple: (node_id, prompt, node_cwd, node_slug, reason, suffix, model, provider)
    assert spawn_calls[0][6] == "glm-4.7"
    assert spawn_calls[0][7] == "codex"


# ---------------------------------------------------------------------------
# _think_output_path - plans-dir birth location (x-ff83 W1)
# ---------------------------------------------------------------------------


def test_think_output_path_honors_plansdirectory(monkeypatch, tmp_path):
    """AC-HP (x-8af8): plansDirectory wins; filename ends -<node-id>.md."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    local = tmp_path / ".claude"
    local.mkdir()
    (local / "settings.local.json").write_text(json.dumps({"plansDirectory": "internal/fno/plans"}))
    import re
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert str(tmp_path / "internal" / "fno" / "plans") in out
    # node-id-suffixed YYYY-MM-DD-<slug>-<node-id>.md
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2}-my-slug-x-2222aaaa\.md", Path(out).name)


def test_think_output_path_falls_back_to_config_plans_dir(monkeypatch, tmp_path):
    """AC-HP: no settings.local -> config.plans_dir default, still in plans-dir."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out.endswith("-my-slug-x-2222aaaa.md")
    assert "plans" in out


def test_think_output_path_suffix_carries_configured_prefix(monkeypatch, tmp_path):
    """AC-EDGE: a configured (non ab-) prefix/width is preserved verbatim in the
    suffix - fixtures for x-2157-like and x-8b89-like nodes end -x-2157.md / -x-8b89.md."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    assert st._think_output_path("x-2157", "feat-a").endswith("-feat-a-x-2157.md")
    assert st._think_output_path("x-8b89", "feat-b").endswith("-feat-b-x-8b89.md")


def test_think_output_path_reuses_existing_node_doc_on_redispatch(monkeypatch, tmp_path):
    """AC-EDGE: a re-dispatch reuses this node's existing doc keyed on the node id,
    not a fresh date file - and is stable across a slug edit between dispatches."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    plans = tmp_path / ".fno" / "plans"
    plans.mkdir(parents=True)
    prior = plans / "2020-01-01-old-slug-x-2222aaaa.md"
    prior.write_text("# earlier dispatch\n")
    # The slug changed to my-slug since the first dispatch; the node id is stable,
    # so the prior doc is reused rather than minting a second file (idempotent).
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out == str(prior)


def test_think_output_path_reuses_frontmatter_claiming_stub(monkeypatch, tmp_path):
    """AC-FR: a pre-created stub whose frontmatter claims the node is the doc's
    home even though its name carries no node-id suffix (reuse-if-claimed)."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    plans = tmp_path / ".fno" / "plans"
    plans.mkdir(parents=True)
    stub = plans / "2020-01-01-hand-created-stub.md"
    stub.write_text("---\nclaims: x-2222aaaa\nstatus: design\n---\n# stub\n")
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out == str(stub)


def test_think_output_path_reuses_legacy_slug_only_doc(monkeypatch, tmp_path):
    """codex P2: a legacy YYYY-MM-DD-<slug>.md (pre-suffix convention, no
    frontmatter claim) is reused on re-dispatch under the same slug, not
    duplicated by a fresh …-<slug>-<node_id>.md mint."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    plans = tmp_path / ".fno" / "plans"
    plans.mkdir(parents=True)
    legacy = plans / "2020-01-01-my-slug.md"
    legacy.write_text("# legacy dispatch, no frontmatter link\n")
    # An unrelated doc that merely ENDS with -my-slug must NOT over-match.
    (plans / "2020-01-01-awesome-my-slug.md").write_text("# unrelated\n")
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out == str(legacy)


def test_think_output_path_node_id_doc_beats_legacy_slug(monkeypatch, tmp_path):
    """A node-id-suffixed doc wins over a legacy slug-only doc for the same node
    (node-id resolution stays primary; the legacy glob is only a last resort)."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    plans = tmp_path / ".fno" / "plans"
    plans.mkdir(parents=True)
    (plans / "2020-01-01-my-slug.md").write_text("# legacy\n")
    suffixed = plans / "2020-02-02-my-slug-x-2222aaaa.md"
    suffixed.write_text("# node-id-suffixed\n")
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out == str(suffixed)


def test_think_output_path_claim_beats_name_suffix(monkeypatch, tmp_path):
    """A frontmatter claim outranks a mere name-suffix match for the same node."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    plans = tmp_path / ".fno" / "plans"
    plans.mkdir(parents=True)
    (plans / "2020-01-01-suffix-x-2222aaaa.md").write_text("# name match only\n")
    claimed = plans / "2020-02-02-the-real-home.md"
    claimed.write_text("---\ngraph_node_id: x-2222aaaa\n---\n# claimed\n")
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out == str(claimed)


def test_think_output_path_empty_slug_uses_node_id(monkeypatch, tmp_path):
    """AC-EDGE: an empty slug degrades to <date>-<node_id>.md, never a dangling
    <date>--<node_id>.md."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    import re
    out = st._think_output_path("x-2222aaaa", "")
    assert out.endswith("-x-2222aaaa.md")
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2}-x-2222aaaa\.md", Path(out).name)


def test_think_output_path_unresolvable_falls_back_to_briefs(monkeypatch, tmp_path, capsys):
    """AC1-ERR: an unresolvable plans dir degrades to briefs/ with a visible warning."""
    def boom():
        raise RuntimeError("no repo root")
    monkeypatch.setattr(st, "_plans_output_dir", boom)
    out = st._think_output_path("x-2222aaaa", "my-slug")
    assert out.endswith("think-x-2222aaaa.md")
    assert "briefs" in out
    assert "plans dir unresolvable" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# project_root-aware settings load (gemini PR #9)
# ---------------------------------------------------------------------------


def test_enabled_honors_project_root(monkeypatch, tmp_path):
    """think_spawn_enabled reads the NODE's repo settings when project_root given."""
    monkeypatch.delenv("FNO_THINK_SPAWN", raising=False)
    seen = []

    class _S:
        class think_spawn:
            enabled = True
            max_per_run = 7

    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda root: seen.append(root) or _S,
    )
    assert st.think_spawn_enabled(project_root=tmp_path, env={}) is True
    assert st._max_per_run(tmp_path) == 7
    assert seen == [tmp_path, tmp_path]  # repo-specific loader used both times


# ---------------------------------------------------------------------------
# on_node_born() - the shared birth seam (v2 A1)
# ---------------------------------------------------------------------------


def _write_graph(path: Path, entries: list[dict]) -> None:
    path.write_text(json.dumps({"entries": entries}) + "\n")


def test_on_node_born_gate_off_is_complete_noop(iso, monkeypatch):
    """Gate OFF => no dispatch attempt AND no graph re-read (gate-first)."""
    monkeypatch.setenv("FNO_THINK_SPAWN", "0")
    reached = []
    monkeypatch.setattr(st, "maybe_spawn_think", lambda *a, **k: reached.append(1))
    # Any graph re-read would import read_graph; assert it is never called.
    import fno.graph.store as gs
    monkeypatch.setattr(gs, "read_graph", lambda *a, **k: reached.append("read"))
    assert st.on_node_born(_node()) is None
    assert reached == []


def _patch_maybe_spawn_recorder(monkeypatch):
    """Patch st.maybe_spawn_think to a recorder returning a fixed 'offered'
    result (x-42c5's real terminal state for an unforced birth call), and
    return the list of nodes it was called with.

    Used by the on_node_born tests below: they exist to prove the durable
    re-read / persisted-skip / RunState-threading wiring, not the spawn/offer
    decision itself (that decision is maybe_spawn_think's own, covered
    exhaustively above) - so they no longer need the spawn subprocess seam.
    """
    calls: list[dict] = []

    def fake(node, **k):
        calls.append(node)
        return st.ThinkSpawnResult("offered", st.EVENT_OFFERED, node_id=node.get("id"))

    monkeypatch.setattr(st, "maybe_spawn_think", fake)
    return calls


def test_on_node_born_rereads_durable_node(iso, tmp_path, monkeypatch):
    """The dispatch carries the post-persist durable node, not a stale caller copy."""
    calls = _patch_maybe_spawn_recorder(monkeypatch)
    durable = _node(slug="durable-slug", cwd=str(tmp_path))
    g = tmp_path / "graph.json"
    _write_graph(g, [durable])
    # Caller hands a pre-slug stub; on_node_born must re-read the durable one.
    st.on_node_born(
        {"id": durable["id"], "slug": "stale-stub", "cwd": str(tmp_path)},
        graph_path=g,
    )
    assert calls and calls[0]["slug"] == "durable-slug"


def test_on_node_born_falls_back_when_node_absent(iso, tmp_path, monkeypatch):
    """A node not yet visible in the graph re-read falls back to the passed dict."""
    calls = _patch_maybe_spawn_recorder(monkeypatch)
    g = tmp_path / "graph.json"
    _write_graph(g, [])  # empty - the born node is not present
    node = _node(cwd=str(tmp_path))
    st.on_node_born(node, graph_path=g)
    assert calls and calls[0]["id"] == node["id"]


def test_on_node_born_is_strictly_non_fatal(iso, monkeypatch):
    """A raising dispatch resolves to None, never propagates into birth."""
    monkeypatch.setattr(
        st, "maybe_spawn_think",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("kaboom")),
    )
    assert st.on_node_born(_node()) is None


def test_on_node_born_threads_run_state(iso, tmp_path, monkeypatch):
    """The hook's run_state kwarg reaches maybe_spawn_think unchanged (bulk paths
    thread ONE RunState through every birth so the blast-cap bounds the whole
    run). x-42c5: RunState.spawned itself no longer advances from an organic
    birth call (birth never spawns), so this checks the plumbing, not the
    counter - the counter's real advance is covered by test_blast_radius_cap via
    a direct maybe_spawn_think call on a consented reason."""
    seen_run_states: list = []

    def fake(node, *, run_state=None, **k):
        seen_run_states.append(run_state)
        return st.ThinkSpawnResult("offered", st.EVENT_OFFERED, node_id=node.get("id"))

    monkeypatch.setattr(st, "maybe_spawn_think", fake)
    g = tmp_path / "graph.json"
    _write_graph(g, [_node(cwd=str(tmp_path))])
    rs = st.RunState()
    st.on_node_born(_node(cwd=str(tmp_path)), graph_path=g, run_state=rs)
    assert seen_run_states == [rs]


def test_on_node_born_persisted_skips_reread(iso, monkeypatch):
    """persisted=True dispatches the passed node directly, skipping the re-read."""
    calls = _patch_maybe_spawn_recorder(monkeypatch)

    import fno.graph.store as gs
    reached: list = []
    monkeypatch.setattr(gs, "read_graph", lambda *a, **k: reached.append("read") or [])

    st.on_node_born(_node(slug="durable-slug"), persisted=True)

    assert calls and calls[0]["slug"] == "durable-slug"
    assert reached == []  # the graph was never re-read


def test_gate_armed_via_real_config_birth_never_spawns(tmp_path, monkeypatch, patch_spawn):
    """x-42c5 regression: reproduces the incident this node exists to close.

    Unlike every other test in this file, the gate is armed by writing a REAL
    ``[think_spawn] enabled = true`` to the repo's config.toml - not the
    FNO_THINK_SPAWN test-seam env var - so this exercises the exact path a
    live ``fno backlog idea`` takes: a node filed in a repo where the operator
    has armed the gate, observed from an AWAY (headless/spawned-worker)
    session. Before x-42c5 this fired a real bg /think worker in ~3 seconds
    with nobody deciding that; now it must resolve to a durable offer and
    NOTHING spawns.
    """
    monkeypatch.delenv("FNO_THINK_SPAWN", raising=False)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _write_config(tmp_path, "[think_spawn]\nenabled = true\n")
    _resolved(monkeypatch, ok=True)
    spawn_calls, stamp_calls = patch_spawn

    events_path = tmp_path / ".fno" / "events.jsonl"
    g = tmp_path / "graph.json"
    node = _node(cwd=str(tmp_path))
    _write_graph(g, [node])

    res = st.on_node_born(node, project_root=tmp_path, graph_path=g)

    assert res is not None and res.decision == "offered"
    assert spawn_calls == [], "a birth trigger must never spawn a real worker"
    assert stamp_calls == []
    evs = _events(events_path)
    assert len(evs) == 1 and evs[0]["type"] == "think_offered"


def test_on_node_born_does_not_key_gate_off_node_cwd(iso, tmp_path, monkeypatch, patch_spawn):
    """Regression (codex P2): the hook must NOT auto-derive project_root from the
    node's durable cwd, or a worktree-born autonomous node's away-manifest is
    looked for in the canonical checkout and it misclassifies as attended."""
    _resolved(monkeypatch, ok=True)
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    seen_roots: list = []

    def fake_enabled(*, project_root=None, env=None):
        seen_roots.append(project_root)
        return True

    monkeypatch.setattr(st, "think_spawn_enabled", fake_enabled)
    g = tmp_path / "graph.json"
    _write_graph(g, [_node(cwd="/some/canonical/checkout")])
    st.on_node_born(_node(cwd="/some/canonical/checkout"), graph_path=g)
    # Every gate consult uses ambient (None), never the node's durable cwd.
    assert seen_roots and all(r is None for r in seen_roots)


# ---------------------------------------------------------------------------
# A2 lifecycle triggers (x-122a): work-start + retro-at-done
# ---------------------------------------------------------------------------


def test_config_subflags_default_off_and_coerce():
    """A2 sub-flags + daily_cap default OFF/sane and fail-safe on garbage."""
    from fno.config import ThinkSpawnBlock

    b = ThinkSpawnBlock()
    assert b.on_work_start is False and b.on_retro is False and b.daily_cap == 20
    # Affirmative strings enable; garbage fails safe to off; bad cap -> 20; 0 honored.
    on = ThinkSpawnBlock(on_work_start="yes", on_retro=1, daily_cap="nonsense")
    assert on.on_work_start is True and on.on_retro is True and on.daily_cap == 20
    assert ThinkSpawnBlock(on_work_start="maybe").on_work_start is False
    assert ThinkSpawnBlock(daily_cap=0).daily_cap == 0


def test_work_start_dispatches_away_with_trigger_tag(iso, monkeypatch, patch_spawn):
    """AC2-HP: a resolved away work-start fires a spawn tagged trigger=work-start."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    res = st.maybe_spawn_think(_node(), reason=st.REASON_WORK_START, env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned"
    assert len(spawn_calls) == 1
    ev = [e for e in _events(iso) if e["type"] == "think_spawned"][-1]
    assert ev["data"]["trigger"] == "work-start"


def test_lifecycle_idempotent_second_suppressed(iso, monkeypatch, patch_spawn):
    """AC2-EDGE: a node worked twice dispatches work-start at most once."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    node, env = _node(), dict(__import__("os").environ)
    r1 = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent)
    r2 = st.maybe_spawn_think(node, reason=st.REASON_WORK_START, env=env,
                              events_path=iso, project_root=iso.parent.parent)
    assert r1.decision == "spawned"
    assert r2.decision == "skipped" and r2.reason == "already-claimed"
    assert len(spawn_calls) == 1


def test_birth_and_retro_both_dispatch_reason_scoped(iso, monkeypatch, patch_spawn):
    """Concurrency invariant: dedup is per-(node, reason) - retro fires independently
    of whatever birth resolved to. x-42c5: birth itself now always offers (it never
    reaches the dedup claim at all), so only retro reaches the spawn seam here -
    but the two evaluations are still independent, which is the point of the
    per-(node, reason) scoping this test guards."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    node, env = _node(), dict(__import__("os").environ)
    r_birth = st.maybe_spawn_think(node, reason=st.REASON_BIRTH, env=env,
                                   events_path=iso, project_root=iso.parent.parent)
    r_retro = st.maybe_spawn_think(node, reason=st.REASON_RETRO, env=env,
                                   events_path=iso, project_root=iso.parent.parent)
    assert r_birth.decision == "offered" and r_retro.decision == "spawned"
    assert len(spawn_calls) == 1


def test_lifecycle_relevance_filter_skips_unresolved(iso, monkeypatch, patch_spawn):
    """A2 relevance filter: a lifecycle trigger needs a RESOLVED pointer; birth does
    not - it degrades to the stored triple and offers (never spawns, x-42c5)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=False, reason="harness-not-supported")
    spawn_calls, _ = patch_spawn
    env = dict(__import__("os").environ)
    # retro (lifecycle) with an unresolved pointer -> skip, no spawn.
    r_life = st.maybe_spawn_think(_node(), reason=st.REASON_RETRO, env=env,
                                  events_path=iso, project_root=iso.parent.parent)
    assert r_life.decision == "skipped" and r_life.reason == "unresolved-pointer"
    assert len(spawn_calls) == 0
    # birth (A1) with the same unresolved pointer degrades to the bare triple and
    # offers - it never reaches the relevance filter's spawn seam either way.
    r_birth = st.maybe_spawn_think(_node(id="x-bbbb"), reason=st.REASON_BIRTH, env=env,
                                   events_path=iso, project_root=iso.parent.parent)
    assert r_birth.decision == "offered" and r_birth.resolved is False
    assert len(spawn_calls) == 0


def test_daily_cap_skips_when_reached(iso, monkeypatch, patch_spawn):
    """A2 firehose guard: at the per-day ceiling an away spawn is skipped."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    monkeypatch.setattr(st, "_daily_cap", lambda root: 3)
    monkeypatch.setattr(st, "_daily_count", lambda: 3)
    res = st.maybe_spawn_think(_node(), reason=st.REASON_RETRO, env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "skipped" and res.reason == "daily-cap"
    assert len(spawn_calls) == 0


def test_daily_cap_zero_disables_ceiling(iso, monkeypatch, patch_spawn):
    """A daily_cap of 0 disables the ceiling entirely (spawns regardless of count)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    monkeypatch.setattr(st, "_daily_cap", lambda root: 0)
    monkeypatch.setattr(st, "_daily_count", lambda: 999)
    res = st.maybe_spawn_think(_node(), reason=st.REASON_RETRO, env=dict(__import__("os").environ),
                               events_path=iso, project_root=iso.parent.parent)
    assert res.decision == "spawned" and len(spawn_calls) == 1


def test_spawn_bumps_daily_count(iso, monkeypatch, patch_spawn):
    """A successful away spawn increments the persisted per-day counter."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    assert st._daily_count() == 0
    st.maybe_spawn_think(_node(), reason=st.REASON_RETRO, env=dict(__import__("os").environ),
                         events_path=iso, project_root=iso.parent.parent)
    assert st._daily_count() == 1


def test_on_node_work_start_subflag_gate(iso, monkeypatch, patch_spawn):
    """The work-start wrapper fires only when on_work_start is armed (even with layer on)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    # sub-flag OFF -> no dispatch even though FNO_THINK_SPAWN=1 (layer on).
    monkeypatch.setattr(st, "_subflag_on", lambda name, root: False)
    assert st.on_node_work_start(_node(), project_root=iso.parent.parent) is None
    assert len(spawn_calls) == 0
    # sub-flag ON -> dispatch.
    monkeypatch.setattr(st, "_subflag_on", lambda name, root: name == "on_work_start")
    res = st.on_node_work_start(_node(), project_root=iso.parent.parent)
    assert res is not None and res.decision == "spawned"
    assert len(spawn_calls) == 1


def test_on_node_retro_subflag_gate(iso, monkeypatch, patch_spawn):
    """The retro wrapper fires only when on_retro is armed."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    monkeypatch.setattr(st, "_subflag_on", lambda name, root: False)
    assert st.on_node_retro(_node(), project_root=iso.parent.parent) is None
    monkeypatch.setattr(st, "_subflag_on", lambda name, root: name == "on_retro")
    res = st.on_node_retro(_node(), project_root=iso.parent.parent)
    assert res is not None and res.decision == "spawned"
    assert len(spawn_calls) == 1


def test_worker_agent_name_reason_scoped():
    """codex P2: birth name is byte-for-byte; lifecycle names are distinct per reason.

    The spawned `fno agents spawn` name must be reason-scoped or the second
    lifecycle trigger for a node collides on name and is wrongly skipped.
    """
    assert st._worker_agent_name("x-1", "slug") == "think-x-1-slug"  # default birth
    assert st._worker_agent_name("x-1", "slug", st.REASON_BIRTH) == "think-x-1-slug"
    assert st._worker_agent_name("x-1", "slug", st.REASON_WORK_START) == "think-x-1-work-start-slug"
    assert st._worker_agent_name("x-1", "slug", st.REASON_RETRO) == "think-x-1-retro-slug"
    names = {st._worker_agent_name("x-1", "slug", r)
             for r in (st.REASON_BIRTH, st.REASON_WORK_START, st.REASON_RETRO)}
    assert len(names) == 3  # no collision across a node's lifecycle


def test_worker_agent_name_capped_at_64_keeps_node_id():
    """x-2c27 (AC2-ERR): the assembled name never exceeds the 64-char spawn limit.

    Per-component slugging caps each part at 30, but a long slug + a long
    invocation suffix on a lifecycle reason can overflow the assembled name and
    crash `fno agents spawn` with "name must be 1-64 chars". The cap trims the
    tail while keeping the `think-<node-id>` lead.
    """
    long_slug = "a-very-long-descriptive-node-slug-that-keeps-going-and-going"
    suffix = "sessaaaa"
    name = st._worker_agent_name("x-2c27", long_slug, st.REASON_WORK_START, suffix)
    assert len(name) <= 64, f"name overflowed: {len(name)} chars: {name!r}"
    assert name.startswith("think-x-2c27-work-start"), f"node id/reason dropped: {name!r}"
    assert not name.endswith("-"), f"trailing hyphen not trimmed: {name!r}"
    # codex P2: the per-session suffix is the uniqueness discriminator - capping
    # must trim the slug, never the suffix, or two repeat dispatches collide.
    assert name.endswith("sessaaaa"), f"session suffix shaved by the cap: {name!r}"
    # Two distinct suffixes on the same long-slug node must yield distinct names.
    other = st._worker_agent_name("x-2c27", long_slug, st.REASON_WORK_START, "sessbbbb")
    assert name != other, f"name collision across suffixes: {name!r} == {other!r}"


def test_lifecycle_wrapper_strictly_non_fatal(iso, monkeypatch):
    """A wrapper swallows any internal failure and returns None (never raises)."""
    monkeypatch.setattr(st, "_subflag_on", lambda name, root: True)

    def boom(*a, **k):
        raise RuntimeError("kaboom")

    monkeypatch.setattr(st, "maybe_spawn_think", boom)
    assert st.on_node_work_start(_node()) is None
    assert st.on_node_retro(_node()) is None


# ---------------------------------------------------------------------------
# US5 (C, x-0a9c) - explicit conversational dispatch verb
# ---------------------------------------------------------------------------


def _stored_origin_node(**over) -> dict:
    """A node whose STORED birth origin differs from the live session, so a test
    can prove the LIVE pointer (not the birth origin) is what gets carried."""
    return _node(
        source_harness="codex",
        source_session_id="STORED-birth-sid",
        source_cwd="/tmp/birth-origin",
        **over,
    )


def test_dispatch_conversational_carries_live_pointer(iso, monkeypatch, patch_spawn):
    """AC5-HP: the spawned think resolves the LIVE session pointer, NOT the node's
    stored birth origin; reason=conversational; one think_spawned event."""
    seen: dict = {}

    def fake_resolve(harness, sid, cwd, **kw):
        seen["triple"] = (harness, sid, cwd)
        return ResolvedTranscript(harness, sid, cwd, True, transcript_path="/live/t.jsonl")

    monkeypatch.setattr(st, "resolve_transcript", fake_resolve)
    spawn_calls, _ = patch_spawn

    res = st.dispatch_conversational(
        _stored_origin_node(), session_id="LIVE-sid", cwd="/tmp/live-sess",
        harness="claude", events_path=iso, project_root=iso.parent.parent,
    )
    assert res.decision == "spawned" and res.think_session == "deadbeef"
    # resolve_transcript saw the LIVE triple, never the stored (codex/STORED.../...).
    assert seen["triple"] == ("claude", "LIVE-sid", "/tmp/live-sess")
    # The prompt carries the live transcript POINTER (never a paraphrase).
    assert "/live/t.jsonl" in spawn_calls[0][1]
    # Spawn used the conversational reason => reason-scoped worker name + dedup token.
    assert spawn_calls[0][4] == st.REASON_CONVERSATIONAL
    evs = _events(iso)
    assert evs[-1]["type"] == st.EVENT_SPAWNED
    assert evs[-1]["data"]["trigger"] == "conversational"


def test_dispatch_forces_spawn_when_gate_off_and_attended(monkeypatch, tmp_path, patch_spawn):
    """AC5-HP: the explicit verb spawns even when the config gate is OFF and the
    session is attended - the invocation IS the opt-in, and it is a real spawn
    (not the default stderr offer line)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_THINK_SPAWN", raising=False)        # config gate OFF (ambient)
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")  # operator at the keyboard
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    events_path = tmp_path / ".fno" / "events.jsonl"

    res = st.dispatch_conversational(
        _node(), session_id="LIVE-sid", cwd="/tmp/live",
        events_path=events_path, project_root=tmp_path,
    )
    # Not noop (gate forced on) and not offered (attended forced to spawn).
    assert res.decision == "spawned"
    assert res.presence == "attended"  # presence is recorded truthfully
    assert len(spawn_calls) == 1


def test_dispatch_no_live_session_skips_no_origin(iso, monkeypatch, patch_spawn):
    """An empty live session_id has nothing to carry -> skip{no-origin} (the CLI
    verb rejects this earlier with a clearer message; the core still fails safe)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    spawn_calls, _ = patch_spawn
    res = st.dispatch_conversational(
        _node(), session_id="", cwd="/tmp/live",
        events_path=iso, project_root=iso.parent.parent,
    )
    assert res.decision == "skipped" and res.reason == "no-origin"
    assert spawn_calls == []


def test_dispatch_conversational_dedup_at_most_once(iso, monkeypatch, patch_spawn):
    """Invariant: two dispatches for the same node within the TTL spawn once
    (reason-scoped dedup token dispatch:think:<id>:conversational)."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    n = _node()
    r1 = st.dispatch_conversational(n, session_id="s", cwd="/tmp/l",
                                    events_path=iso, project_root=iso.parent.parent)
    r2 = st.dispatch_conversational(n, session_id="s", cwd="/tmp/l",
                                    events_path=iso, project_root=iso.parent.parent)
    assert r1.decision == "spawned"
    assert r2.decision == "skipped" and r2.reason == "already-claimed"
    assert len(spawn_calls) == 1
    # The worker name + dedup token carried the live session discriminator.
    assert spawn_calls[0][5] == "s"


def test_dispatch_different_sessions_both_spawn(iso, monkeypatch, patch_spawn):
    """codex P2: two DIFFERENT conversations dispatching the same node each get
    their own worker (distinct name + session-scoped dedup token), so a later
    conversation can re-dispatch - the verb is repeatable, unlike once-per-moment
    birth/lifecycle triggers whose names/tokens are reason-scoped only."""
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    _resolved(monkeypatch, ok=True)
    spawn_calls, _ = patch_spawn
    n = _node()
    r1 = st.dispatch_conversational(n, session_id="sessAAAA", cwd="/l",
                                    events_path=iso, project_root=iso.parent.parent)
    r2 = st.dispatch_conversational(n, session_id="sessBBBB", cwd="/l",
                                    events_path=iso, project_root=iso.parent.parent)
    assert r1.decision == "spawned" and r2.decision == "spawned"
    assert len(spawn_calls) == 2
    assert spawn_calls[0][5] == "sessAAAA" and spawn_calls[1][5] == "sessBBBB"


def test_worker_name_unique_per_conversation():
    """The per-invocation suffix lands in the worker name (the permanent registry
    key `fno agents spawn` rejects on collision)."""
    a = st._worker_agent_name("x-1", "slug", st.REASON_CONVERSATIONAL, "sessAAAA")
    b = st._worker_agent_name("x-1", "slug", st.REASON_CONVERSATIONAL, "sessBBBB")
    assert a != b
    assert a.endswith("-sessaaaa") and b.endswith("-sessbbbb")
    # No suffix -> byte-for-byte the prior name (birth/lifecycle unchanged).
    assert st._worker_agent_name("x-1", "slug", st.REASON_BIRTH) == "think-x-1-slug"


# ---------------------------------------------------------------------------
# x-3218: provenance naming is now a caller of the canonical owner
# ---------------------------------------------------------------------------


def test_provenance_name_is_byte_identical_to_the_canonical_owner():
    """AC3: same semantic components -> same name, whichever caller asks."""
    from fno.agents.naming import agent_name

    assert st._worker_agent_name("x-1", "slug") == agent_name("think", "x-1", slug="slug")
    assert st._worker_agent_name("x-1", "slug", st.REASON_RETRO, "sessaaaa") == agent_name(
        "think", "x-1", qualifier=st.REASON_RETRO, slug="slug", discriminator="sessaaaa"
    )


def test_over_budget_provenance_identity_fails_closed_instead_of_shaving():
    """AC4 + AC5: the old backstop sliced the tail (shaving the discriminator).

    Two dispatches differing only by their session suffix would collapse onto
    one name and the second would be wrongly skipped as a duplicate. The
    canonical owner refuses instead.
    """
    from fno.agents.naming import AgentNameError

    long_id = "n-" + "z" * 50
    with pytest.raises(AgentNameError) as exc:
        st._worker_agent_name(long_id, "slug", st.REASON_WORK_START, "sessaaaa")
    assert long_id in str(exc.value)


# ---------------------------------------------------------------------------
# shared dispatch substrate + on_decompose_wave0
# ---------------------------------------------------------------------------


def _write_config(root: Path, body: str) -> None:
    (root / ".fno").mkdir(parents=True, exist_ok=True)
    (root / ".fno" / "config.toml").write_text(body)


def test_AC10_HP_absent_substrate_key_still_yields_bg(monkeypatch, tmp_path):
    """The historical hardcode is the default, so nothing changes unconfigured."""
    cap = _capture_spawn_cmd(monkeypatch)
    st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")
    assert cap["cmd"][cap["cmd"].index("--substrate") + 1] == "bg"


def test_AC10_HP_configured_dispatch_substrate_reaches_every_spawn(monkeypatch, tmp_path):
    """The shared dispatch selector applies to every context-think spawn."""
    _write_config(tmp_path, "[dispatch]\nsubstrate = \"headless\"\n")
    cap = _capture_spawn_cmd(monkeypatch)
    st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")
    assert cap["cmd"][cap["cmd"].index("--substrate") + 1] == "headless"


def test_legacy_think_spawn_substrate_remains_a_compatibility_fallback(
    monkeypatch, tmp_path
):
    _write_config(tmp_path, "[think_spawn]\nsubstrate = \"headless\"\n")
    cap = _capture_spawn_cmd(monkeypatch)
    st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")
    assert cap["cmd"][cap["cmd"].index("--substrate") + 1] == "headless"


def test_shared_dispatch_substrate_overrides_legacy_compatibility_key(
    monkeypatch, tmp_path
):
    _write_config(
        tmp_path,
        '[think_spawn]\nsubstrate = "headless"\n\n[dispatch]\nsubstrate = "bg"\n',
    )
    cap = _capture_spawn_cmd(monkeypatch)
    st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")
    assert cap["cmd"][cap["cmd"].index("--substrate") + 1] == "bg"


def test_a_garbage_dispatch_substrate_fails_loud(monkeypatch, tmp_path):
    from fno.agents.harness_map import DispatchResolveError

    _write_config(tmp_path, "[dispatch]\nsubstrate = \"quantum\"\n")
    _capture_spawn_cmd(monkeypatch)
    with pytest.raises(DispatchResolveError, match="unknown substrate"):
        st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")


def test_AC8_HP_wave0_fanout_is_off_by_default(tmp_path, monkeypatch):
    """No key present -> off, byte-identical to the flagged-only lane."""
    monkeypatch.delenv("FNO_THINK_SPAWN_WAVE0", raising=False)
    assert st.think_spawn_on_decompose_wave0(project_root=tmp_path) is False


def test_wave0_needs_its_own_flag_not_just_enabled(tmp_path, monkeypatch):
    """Same shape as on_work_start / on_retro: `enabled` alone never arms it.

    Read off the settings block rather than through `think_spawn_enabled`:
    conftest pins FNO_THINK_SPAWN=0 process-wide so no test can spawn a real
    worker, and that override outranks config by design.
    """
    monkeypatch.delenv("FNO_THINK_SPAWN_WAVE0", raising=False)
    _write_config(tmp_path, "[think_spawn]\nenabled = true\n")
    from fno.config import load_settings_for_repo

    block = load_settings_for_repo(tmp_path).think_spawn
    assert block.enabled is True
    assert block.on_decompose_wave0 is False
    assert st.think_spawn_on_decompose_wave0(project_root=tmp_path) is False


def test_wave0_arms_when_its_own_flag_is_set(tmp_path, monkeypatch):
    monkeypatch.delenv("FNO_THINK_SPAWN_WAVE0", raising=False)
    _write_config(tmp_path, "[think_spawn]\non_decompose_wave0 = true\n")
    assert st.think_spawn_on_decompose_wave0(project_root=tmp_path) is True


def test_wave0_env_override_wins(tmp_path, monkeypatch):
    _write_config(tmp_path, "[think_spawn]\non_decompose_wave0 = true\n")
    assert st.think_spawn_on_decompose_wave0(
        project_root=tmp_path, env={"FNO_THINK_SPAWN_WAVE0": "0"}
    ) is False
    assert st.think_spawn_on_decompose_wave0(
        project_root=Path("/nonexistent"), env={"FNO_THINK_SPAWN_WAVE0": "1"}
    ) is True


def test_wave0_fails_safe_on_a_malformed_block(tmp_path, monkeypatch):
    """A spurious fan-out spends real tokens on cold sessions; fail to off."""
    monkeypatch.delenv("FNO_THINK_SPAWN_WAVE0", raising=False)
    _write_config(tmp_path, "[think_spawn]\non_decompose_wave0 = \"maybe\"\n")
    assert st.think_spawn_on_decompose_wave0(project_root=tmp_path) is False


def test_wave0_adds_no_new_ceiling(tmp_path):
    """It inherits max_per_run and daily_cap rather than inventing parallel caps."""
    _write_config(tmp_path, "[think_spawn]\non_decompose_wave0 = true\n")
    from fno.config import load_settings_for_repo

    block = load_settings_for_repo(tmp_path).think_spawn
    assert block.max_per_run == 5
    assert block.daily_cap == 20
    assert not [f for f in type(block).model_fields if "wave0_cap" in f]


# non-bg spawn receipts (codex P1) --------------------------------------------


def _capture_with_stdout(monkeypatch, stdout: str) -> dict:
    """Patch subprocess.run with a scripted receipt; return the captured cmd."""
    captured: dict = {}

    class _Proc:
        returncode = 0
        stderr = ""

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        p = _Proc()
        p.stdout = stdout
        return p

    monkeypatch.setattr(st.subprocess, "run", fake_run)
    return captured


def test_a_pane_receipt_without_short_id_is_still_a_launch(monkeypatch, tmp_path):
    """The pane receipt carries mux_session/pane_id, never short_id.

    Raising here reported `skipped` for a worker that IS running, so decompose
    marked the child unowned and inline-filled it out from under a live pane
    worker - the exact double-write the ownership receipt prevents.
    """
    _write_config(tmp_path, "[dispatch]\nsubstrate = \"pane\"\n")
    _capture_with_stdout(
        monkeypatch, '{"status":"live","mux_session":"m1","pane_id":"%1"}\n'
    )
    handle = st._spawn_think_worker(
        "x-1", "prompt", str(tmp_path), "slug", provider="codex"
    )
    assert handle, "a launched pane worker must return an addressable handle"


def test_a_headless_reply_launches_but_stamps_no_session_pointer(
    monkeypatch, tmp_path
):
    """The headless `once` path writes the provider reply verbatim, not JSON.

    It must NOT raise (the work happened), and must NOT invent a session id: the
    one-shot has already torn down, so a handle here would be stamped into
    `think_session_id` and resolve to nothing on every successful spawn. Empty
    is the honest answer; the `think_spawned` event still carries `agent_name`.
    """
    _write_config(tmp_path, "[dispatch]\nsubstrate = \"headless\"\n")
    _capture_with_stdout(monkeypatch, "I have finished the design pass.\n")
    handle = st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")
    assert handle == ""


def test_an_empty_session_is_not_stamped_over_the_node(monkeypatch, tmp_path):
    """A pointer that resolves to nothing is worse than no pointer."""
    captured: dict = {}

    def fake_mutate(path, mutator, **kw):
        entries = [{"id": "x-1", "think_session_id": "prior"}]
        captured["out"] = mutator(entries)

    monkeypatch.setattr(
        "fno.graph.store.locked_mutate_graph", fake_mutate, raising=False
    )
    st._stamp_forward("x-1", "", None, output_path="/tmp/doc.md")
    if "out" in captured:
        assert captured["out"][0]["think_session_id"] == "prior"
        assert captured["out"][0]["think_output_path"] == "/tmp/doc.md"


def test_the_non_bg_handle_is_the_agent_name_not_a_fabricated_id(
    monkeypatch, tmp_path
):
    """It must address a real worker: `fno agents logs <name>` has to work."""
    _write_config(tmp_path, "[dispatch]\nsubstrate = \"pane\"\n")
    cap = _capture_with_stdout(monkeypatch, '{"pane_id":"%1"}\n')
    handle = st._spawn_think_worker(
        "x-1", "prompt", str(tmp_path), "slug", provider="codex"
    )
    assert handle == cap["cmd"][cap["cmd"].index("--name") + 1]


def test_bg_still_REQUIRES_a_short_id_receipt(monkeypatch, tmp_path):
    """Unchanged where the receipt contract holds - no blanket loosening."""
    _write_config(tmp_path, "[dispatch]\nsubstrate = \"bg\"\n")
    _capture_with_stdout(monkeypatch, "some banner with no receipt\n")
    with pytest.raises(st.SpawnError):
        st._spawn_think_worker("x-1", "prompt", str(tmp_path), "slug")


def test_a_nonzero_exit_still_raises_on_every_substrate(monkeypatch, tmp_path):
    """Exit 0 is the launch signal; a real failure must still surface."""
    _write_config(tmp_path, "[dispatch]\nsubstrate = \"pane\"\n")

    class _Proc:
        returncode = 1
        stdout = ""
        stderr = "substrate unavailable"

    monkeypatch.setattr(st.subprocess, "run", lambda cmd, **kw: _Proc())
    with pytest.raises(st.SpawnError):
        st._spawn_think_worker(
            "x-1", "prompt", str(tmp_path), "slug", provider="codex"
        )
