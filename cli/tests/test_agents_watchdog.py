"""x-55c3: the external fleet watchdog's verdicts, apply lanes, and the
``--all`` enumeration they read.

The classifier is pure, so every verdict test injects transcripts, claims,
and node state - no live fleet. The apply-lane tests inject runners and tail
reads. The one real-filesystem test (worktree refusal) builds a throwaway git
repo. The two traps are pinned here on purpose: identity joins on the claim
holder, never a name; a wake is confirmed by transcript content, never a state
field.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from types import SimpleNamespace

import pytest
import typer

from fno.agents import watchdog
from fno.agents.watchdog import (
    CONTENDED,
    GHOST,
    LEAVE,
    SANDBOX_BLOCKED,
    Row,
    STALE,
    TailFacts,
    UNCLAIMED,
    Verdict,
    WAKE,
    apply_verdict,
    rate_limit_window,
    verdicts,
)

# 2026-08-16T18:40:00Z. The plan's measured case: a 02:48:21 SGT reset stamp
# (= 18:48:21Z) is 8m out at 18:40Z and 2m past at 18:50Z.
NOW_1840 = datetime(2026, 8, 16, 18, 40, 0, tzinfo=timezone.utc).timestamp()
NOW_1850 = datetime(2026, 8, 16, 18, 50, 0, tzinfo=timezone.utc).timestamp()
RATE_LIMIT_TAIL = "API Error: 429 rate limit, window resets at 02:48:21 SGT"


def _facts(
    text: str, age_min: float = 5, role: str = "assistant", kind: str = "text"
) -> TailFacts:
    del kind  # the tool-call reading left with the reap lane; kept for callers
    epoch = NOW_1840 - age_min * 60
    return TailFacts([(epoch, text)], epoch, text, role, text)


def _run(rows, transcripts, *, claims=None, nodes=None, now_s=NOW_1840,
         pr_state_for=None):
    return verdicts(
        rows,
        transcript_for=lambda sid: transcripts.get(sid),
        claim_for=lambda node: (claims or {}).get(node, {}),
        node_state_for=lambda node: (nodes or {}).get(node),
        now_s=now_s,
        pr_state_for=pr_state_for,
    )


# ---------------------------------------------------------------------------
# Verdicts
# ---------------------------------------------------------------------------

def test_no_transcript_working_row_is_ghost_and_apply_all_still_refuses():
    rows = [Row("bbbb2222-0000", "w1", "working", None, "/tmp/w1")]
    [v] = _run(rows, {})
    assert v.verdict == GHOST
    assert v.basis == "no transcript for bbbb2222-0000"
    assert v.action == "report"
    outcome, detail = apply_verdict(v, lanes="all")
    assert outcome == watchdog.SKIPPED
    assert "outside" in detail


def _linked_worktree_dir(tmp_path, name: str) -> str:
    """A stand-in linked worktree: a directory whose ``.git`` is a FILE, which
    is the one marker ``_is_linked_worktree`` reads."""
    wt = tmp_path / name
    wt.mkdir()
    (wt / ".git").write_text("gitdir: /tmp/elsewhere/main\n")
    return str(wt)


def test_two_live_rows_in_one_worktree_are_contended(tmp_path):
    wt = _linked_worktree_dir(tmp_path, "w1")
    a = Row("aaaa1111-0000", "w1", "working", None, wt)
    b = Row("bbbb2222-0000", "w2", "working", None, wt)
    vs = _run(
        [a, b],
        {"aaaa1111-0000": _facts("still on it"),
         "bbbb2222-0000": _facts("still on it")},
    )
    assert [v.verdict for v in vs] == [CONTENDED, CONTENDED]
    assert "2 live sessions" in vs[0].basis
    assert "bbbb2222-0000" in vs[0].basis
    assert "aaaa1111-0000" in vs[1].basis


def test_run_sweep_carries_the_pr_state_seam_end_to_end():
    from fno.agents import watchdog as wd

    entries = [
        {"message": {"role": "user", "content": [
            {"type": "tool_result",
             "content": [{"type": "text",
                          "text": '{"pr":1371,"state":"MERGED"}'}]},
        ]}},
    ] + [
        {"message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "Bash",
             "input": {"command": "gh pr view 1371"}},
        ]}},
    ] * 2
    row = Row("aaaa1111-0000", "w1", "working", None, "/tmp/w1")
    payload, out_rows = wd.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: ([row], []),
        transcript_fn=lambda sid: wd._facts_from_entries(entries, 10),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
        pr_state_fn=lambda cwd, n: "MERGED",
    )
    assert not payload.get("refused")
    [v] = payload["verdicts"]
    assert v["verdict"] == watchdog.POLLING_SETTLED
    assert payload["counts"]["polling_settled"] == 1
    assert out_rows[0].row_id == "aaaa1111-0000"


def test_single_sgt_429_is_report_only_until_provider_quorum():
    row = Row("cccc3333-0000", "r1", "blocked", None, "/tmp/r1")
    # 125m silent: past classify_tail's 2h window, so the tail POSITIVELY
    # asserts the session owes its next move (stalled) - the wake gate's
    # resumability marker, not a missing-429 absence.
    [before] = _run([row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL, age_min=125)})
    assert before.verdict == LEAVE
    assert "provider quorum" in before.basis
    [after] = _run(
        [row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL, age_min=125)}, now_s=NOW_1850
    )
    assert after.verdict == LEAVE
    assert "429 window passed; return owned by provider-cap" in after.basis


def test_taxonomy_quota_phrasings_mark_the_window():
    """The wake window must mark every spelling the failover taxonomy marks.
    Three workers died on the usage-limit phrasing on 2026-08-17, and a
    rate-limit-only regex reads their tails as ordinary silence."""
    assert rate_limit_window(
        [(None, "API Error: Claude usage limit reached. Window resets at 02:48:21 SGT")],
        NOW_1840,
    )[0] == "live"
    assert rate_limit_window(
        [(None, "API Error: quota exceeded, resets 02:48:21 SGT")], NOW_1840
    )[0] == "live"
    # A usage-limit tail with no parseable stamp is unknown: fail safe.
    assert rate_limit_window(
        [(None, "API Error Request rejected 429, Usage limit reached for 5 hour")],
        NOW_1840,
    )[0] == "unknown"
    # A real quota body with no error-shaped words still holds the window:
    # the classifier is the gate, never an extra word prefilter.
    assert rate_limit_window(
        [(None, "429 rate limit reached, retry after 8s. window resets at 02:48:21 SGT")],
        NOW_1840,
    )[0] == "live"
    # Prose quoting the full vocabulary errs CLOSED (withholds a wake) - the
    # accepted cost, since erring open burns a turn inside a live window.
    assert rate_limit_window(
        [(None, "we are rate limited on the search API. sync at 02:48:21 SGT")],
        NOW_1840,
    )[0] == "live"
    # A bare 429 mention without the taxonomy's phrases is not a rate event.
    assert rate_limit_window(
        [(None, "reviewing PR 429 now")], NOW_1840
    )[0] == "none"
    row = Row("ab12cd34-0000", "u1", "blocked", None, "/tmp/u1")
    [v] = _run(
        [row],
        {"ab12cd34-0000": _facts(
            "API Error Request rejected 429, Usage limit reached for 5 hour",
            age_min=125,
        )},
    )
    assert v.verdict == LEAVE


def test_stopped_row_is_wakeable():
    rows = [Row("dddd4444-0000", "k1", "stopped", None, "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("stopped mid turn", age_min=130)})
    assert v.verdict == WAKE


def test_wake_age_ceiling_buckets_old_rows_as_stale_not_wake():
    # The lead's measured case: a session stopped 87852 minutes (61 days) is
    # not recovery - dead node, stale branch, stale context. Past the 1d
    # ceiling the row is a needs-human bucket and never reaches an action
    # lane, not even under --apply=all.
    rows = [Row("dddd4444-0000", "k1", "stopped", None, "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("stopped mid turn", age_min=87852)})
    assert v.verdict == STALE
    assert "wake ceiling" in v.basis
    assert v.action == "report"
    outcome, _ = apply_verdict(v, lanes="all")
    assert outcome == watchdog.SKIPPED


def test_done_node_row_past_the_ceiling_is_spent_not_stale():
    # The measured re-nag shape: a session whose node shipped, quiet past the
    # wake ceiling. The work is over; filing it stale asked a human to triage
    # a finished row that can never age out of the ask.
    rows = [Row("dddd4444-0000", "k1", "stopped", "abc12345", "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("stopped mid turn", age_min=61 * 1440)},
               nodes={"abc12345": {"status": "done"}})
    assert v.verdict == watchdog.SPENT
    assert v.action == "none"
    assert "node abc12345 done" in v.basis
    assert "needs a human" not in v.basis
    outcome, _ = apply_verdict(v, lanes="all")
    assert outcome == watchdog.SKIPPED


def test_ledger_finds_nodes_for_manifest_less_rows(monkeypatch, tmp_path):
    # A worker that ran in the canonical checkout has no manifest of its own;
    # the execution ledger's machine-written (sessions -> node) row is its
    # recorded identity. The ``t-`` shorthand name stays untrusted: the node's
    # dash is stripped in it, so the slug boundary is unknowable.
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        # graph_node_id is the documented join key; title is the free-text
        # task input and must join nothing - not even a bare node-shaped one.
        {"title": "fix the login bug", "graph_node_id": "x-d214",
         "sessions": ["e65d5fff-8ba4-46d4-b2df-f19b2eb832f1"]},
        {"title": "x-d214", "sessions": ["aaaa1111-0000"]},
    ]}))
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    nodes = watchdog._ledger_nodes()
    assert nodes["e65d5fff-8ba4-46d4-b2df-f19b2eb832f1"] == "x-d214"
    assert "aaaa1111-0000" not in nodes


def test_no_parseable_evidence_never_wakes():
    # A tail with no parseable timestamp is the "basis no-evidence" row the
    # lead caught marked wake: absence of evidence must never reach an action.
    facts = TailFacts([(None, "stopped mid turn")], None, "stopped mid turn",
                      "assistant", "stopped mid turn")
    rows = [Row("eeee6666-0000", "n1", "stopped", None, "/tmp/n1")]
    [v] = _run(rows, {"eeee6666-0000": facts})
    assert v.verdict == LEAVE
    assert "no parseable transcript evidence" in v.basis
    # A transcript-less CLOUD row is LEAVE, for its own reason: the session
    # lives on claude.ai, so no local transcript is BY DESIGN, not the ghost
    # a dead local session would read.
    [cloud] = _run(
        [Row("session_cloud01", "w1", "working", None, "/tmp/w1", substrate="cloud")],
        {},
    )
    assert cloud.verdict == LEAVE
    assert "claude.ai" in cloud.basis
    assert cloud.action == "report"


def test_stopped_row_under_live_429_window_waits_not_wakes():
    # Reroute only catches blocked rows; a stopped row whose window is still
    # closed must not wake into the bounce that costs a real turn.
    rows = [Row("eeee5555-0000", "s1", "stopped", None, "/tmp/s1")]
    [v] = _run(rows, {"eeee5555-0000": _facts(RATE_LIMIT_TAIL)})
    assert v.verdict == LEAVE
    assert "window not open" in v.basis and "18:48:21Z" in v.basis


# ---------------------------------------------------------------------------
# Apply lanes
# ---------------------------------------------------------------------------

class _Proc:
    def __init__(self, returncode=0, stderr="", stdout=""):
        self.returncode = returncode
        self.stderr = stderr
        self.stdout = stdout


def test_lifecycle_subprocesses_run_in_the_rows_worktree(monkeypatch):
    monkeypatch.setattr(watchdog, "_CONFIRM_ATTEMPTS", 3)
    monkeypatch.setattr(watchdog, "_CONFIRM_INTERVAL_S", 0.0)
    """A registry-less row from another project must resolve in its own
    project: the delegated resume carries the row's cwd, never the sweep
    caller's ambient one."""
    seen = {}
    reads = {"n": 0}

    def fake_tail(*a, **k):
        reads["n"] += 1
        if reads["n"] == 1:
            return _facts("stopped mid turn")
        return _facts("continue", age_min=0)

    def runner(argv, **kw):
        seen["cwd"] = kw.get("cwd")
        return _Proc(0)

    monkeypatch.setattr(watchdog, "tail_facts", fake_tail)
    v = Verdict("dddd4444-0000", "k1", "stopped", WAKE, "stopped 30m", "resume")
    outcome, detail = apply_verdict(
        v, lanes="wake", cwd="/tmp/other-project", runner=runner
    )
    assert outcome == "applied", detail
    assert seen["cwd"] == "/tmp/other-project"


def test_rows_without_a_session_id_are_skipped_loudly(monkeypatch, tmp_path):
    """A short id never resolves a transcript, so a live row keyed on it
    reads ghost. Such rows are skipped with a warning, never classified."""
    from fno.agents import registry as registry_mod
    from fno.agents.harnesses import claude as claude_mod
    import fno.paths as paths_mod

    raw = [
        {"id": "51f36553", "state": "working", "cwd": "/tmp/w2", "name": "tgt-live"},
        {"sessionId": "baf9409a-2af5-444a-846c-059e8fa2f758", "state": "stopped",
         "cwd": "/tmp/w", "name": "tgt-stopped"},
    ]
    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **k: (raw, []))
    monkeypatch.setattr(
        registry_mod, "load_registry", lambda: (_ for _ in ()).throw(OSError())
    )
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: tmp_path / "no.json")

    rows, warnings = watchdog.fleet_rows()
    assert [r.row_id for r in rows] == ["baf9409a-2af5-444a-846c-059e8fa2f758"]
    assert any("no session id" in w for w in warnings)


def test_fleet_rows_includes_live_nonclaude_registry_rows(monkeypatch, tmp_path):
    """The Claude roster is not the fleet: live Codex rows still join by node."""
    from fno.agents import registry as registry_mod
    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.registry import AgentEntry

    row = AgentEntry(
        name="codex-thread",
        harness="codex",
        harness_session_id="thread-535c",
        cwd=str(tmp_path),
        log_path="",
        status="live",
        origin="spawn",
        node="x-535c",
    )
    monkeypatch.setattr(registry_mod, "load_registry", lambda: [row])
    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **_k: ([], []))

    rows, warnings = watchdog.fleet_rows()

    assert warnings == []
    assert rows == [
        Row("thread-535c", "codex-thread", "working", "x-535c", str(tmp_path), "codex")
    ]


def test_sweep_reads_each_rows_transcript_under_that_rows_harness(monkeypatch, tmp_path):
    """A codex row's tick read resolves the codex transcript store, so its
    facts are real and the row is never a ghost."""
    from fno.agents import provider_outage as po
    from fno.agents import registry as registry_mod

    rows = [Row("thread-535c", "codex-thread", "working", None, str(tmp_path), "codex")]
    monkeypatch.setattr(watchdog, "fleet_rows", lambda timeout=None: (rows, []))
    monkeypatch.setattr(registry_mod, "load_registry", lambda: [])
    monkeypatch.setattr(po, "journal_path", lambda: tmp_path / "po.json")

    seen: list[str] = []

    def fake_tail_entries(sid, cwd, *, agent):
        seen.append(agent)
        return [{
            "type": "assistant",
            "timestamp": "2026-08-16T18:39:00Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "mid task"}]},
        }]

    monkeypatch.setattr(watchdog, "tail_entries", fake_tail_entries)
    payload, _out = watchdog.run_sweep(
        now_s=NOW_1840, claim_fn=lambda key: {}, graph_fn=lambda: {},
    )
    assert seen == ["codex"]
    [v] = payload["verdicts"]
    assert v["verdict"] != GHOST


def test_apply_wake_codex_row_accepts_resumes_own_daemon_receipt(monkeypatch):
    """A codex thread wake that exits 0 is delivered: the daemon accepted
    the turn, which is the same receipt mail trusts. The transcript
    re-confirm must not run - a lagging rollout write would read as a
    refusal on a delivered wake and invite a duplicate turn."""
    v = Verdict("thread-9", "codex-worker", "blocked", WAKE, "silent", "resume", "codex")

    monkeypatch.setattr(watchdog, "tail_facts", lambda *a, **kw: None)

    def _no_confirm(*a, **kw):
        raise AssertionError("the transcript confirm must not run for codex")

    monkeypatch.setattr(watchdog, "confirm_wake_landed", _no_confirm)

    def runner(cmd, **kw):
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    outcome, detail = apply_verdict(v, lanes="all", cwd="/tmp/x", runner=runner)
    assert outcome == "applied"
    assert "delivery receipt (codex)" in detail


def test_apply_wake_refusal_carries_resumes_own_receipt_line(monkeypatch):
    """AC5-HP (x-6ac3): resume's exit 0 was the false receipt, so its own
    before -> after line belongs in the refusal detail the next operator
    reads, without re-running anything."""
    v = Verdict("aaaa1111-0000", "w1", "blocked", WAKE, "silent", "resume", "claude")
    monkeypatch.setattr(watchdog, "confirm_wake_landed", lambda *a, **kw: False)
    def runner(cmd, **kw):
        return SimpleNamespace(
            returncode=0,
            stderr="",
            stdout="w1 (aaaa1111-0000): Needs input -> Working\n",
        )

    outcome, detail = apply_verdict(v, lanes="all", cwd="/tmp/x", runner=runner)
    assert outcome == "refused"
    assert "Needs input -> Working" in detail


def test_a_transcript_read_without_an_agent_refuses():
    """The silent claude default was the defect: an omitted agent is a bind
    error, never a wrong-file read."""
    with pytest.raises(TypeError):
        watchdog.tail_facts("sid", "/tmp")
    with pytest.raises(TypeError):
        watchdog.tail_entries("sid", "/tmp")


# ---------------------------------------------------------------------------
# Retire: stop a finished worker, destroy nothing
# ---------------------------------------------------------------------------
#
# A worker that finishes its deliverable and never exits holds a live slot
# against config.agents.max_live forever. terminal_stop.rs already stops that
# population, but its marker is written by finalize, which a /fno:blueprint or
# /fno:think worker never reaches. Nothing tells those workers to stop.


def test_every_registry_row_is_born_with_an_origin_and_a_substrate():
    """The producer half, and it has to cover EVERY birth site in BOTH languages.

    The consumer only acts on a positive marker, so a birth path that forgets
    one does not open a hole - it makes the lane silently unsatisfiable for the
    workers that path creates, which is the same defect pointing the other way.

    Scanning only Python was that defect in the test itself. Rust mints rows
    too, through `RegistryEntry { .. }` literals in the daemon, the three
    `*_ask` create paths and both adopt paths, and a seventh added later would
    have shipped unmarked with nothing failing. So this walks the Rust struct
    literals as well, and accepts the attribute call form (`registry.AgentEntry`)
    that a plain `func.id` check lets through.

    The walk carries TWO markers. `origin` says what created the row;
    `substrate` says which lane it was spawned on. An explicit `None` is the
    point for substrate: it records that the writer considered the question
    and could not answer it, where an omission cannot be told apart from
    never-recorded - and a silent default of "pane" is the exact guess the
    field exists to replace.
    """
    import ast
    import pathlib
    import re

    missing = []

    pkg = pathlib.Path(watchdog.__file__).parent
    for path in sorted(pkg.glob("*.py")):
        for node in ast.walk(ast.parse(path.read_text())):
            if not isinstance(node, ast.Call):
                continue
            func = node.func
            name = getattr(func, "id", None) or getattr(func, "attr", None)
            if name != "AgentEntry":
                continue
            # `AgentEntry(**row)` rehydrates a row that was already born.
            if any(kw.arg is None for kw in node.keywords):
                continue
            for marker in ("origin", "substrate"):
                if not any(kw.arg == marker for kw in node.keywords):
                    missing.append(f"{path.name}:{node.lineno} ({marker})")

    # Rust has no AST here, so read the literal's field list: from the opening
    # brace to the matching close at the same indent. Struct-update syntax
    # (`..other`) inherits the field and is not a birth statement.
    rust = next(
        (d / "crates" / "fno-agents" / "src")
        for d in pkg.parents
        if (d / "crates" / "fno-agents" / "src").is_dir()
    )
    # Match the literal ANYWHERE on the line. Anchoring it to the start of the
    # line after indent was the same mistake this test exists to catch: it saw 8
    # of the 20 literals, because most are written `let e = RegistryEntry {` or
    # `entries.push(state::RegistryEntry {`, and it passed by not looking.
    opener = re.compile(r"(?<![A-Za-z0-9_])(?:[A-Za-z_][A-Za-z0-9_]*::)*RegistryEntry\s*\{\s*$")
    seen = 0
    for path in sorted(rust.rglob("*.rs")):
        lines = path.read_text().split("\n")
        for i, line in enumerate(lines):
            # Type positions, not literals: `struct X {`, `impl X {`, and any
            # signature whose return type is X. Widening the match to find the
            # literals swept these in, and a type position has no fields to
            # carry a marker.
            if re.search(r"\b(struct|impl|trait|enum)\b", line) or "->" in line:
                continue
            if opener.search(line) is None:
                continue
            seen += 1
            body = []
            indent = len(line) - len(line.lstrip())
            for later in lines[i + 1:]:
                later_indent = len(later) - len(later.lstrip())
                if later_indent == indent and later.strip() in ("}", "};", "})", "}),"):
                    break
                body.append(later)
            joined = "\n".join(body)
            for marker in ("origin", "substrate"):
                if ".." in joined and not re.search(
                    rf"^\s*{marker}:", joined, re.MULTILINE
                ):
                    continue  # struct-update inherits this marker
                if not re.search(rf"^\s*{marker}:", joined, re.MULTILINE):
                    missing.append(f"{path.name}:{i + 1} ({marker})")

    # A scan that matches nothing passes for the same reason a correct one does,
    # so state the floor. This is a positive control on the instrument, and it
    # caught the anchored regex above reading 8 sites instead of 20.
    assert seen >= 15, f"the Rust scan reached only {seen} literals; it is not looking"
    assert not missing, (
        "every registry row must state what created it and which lane it ran; "
        "unmarked at " + ", ".join(missing)
    )


def test_wake_confirmation_polls_for_the_flushed_turn(monkeypatch):
    """resume returns when the live STATE reads working, before the injected
    turn is flushed, so a one-shot read reports a landed wake as refused."""
    reads = {"n": 0}

    def fake_tail(*a, **k):
        reads["n"] += 1
        if reads["n"] < 4:
            return _facts("stopped mid turn")
        return _facts("continue", age_min=0)

    monkeypatch.setattr(watchdog, "tail_facts", fake_tail)
    slept = []
    assert watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", NOW_1840 - 600, agent="claude",
        attempts=6, interval_s=0.01, sleep=slept.append,
    )
    assert slept, "a polling confirm must have waited at least once"

    # A wake that never lands still reports refused after the attempts run out.
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda *a, **k: _facts("stopped mid turn")
    )
    assert not watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", NOW_1840 - 600, agent="claude",
        attempts=3, interval_s=0.0, sleep=lambda _s: None,
    )


def test_persisted_registry_node_precedes_manifest_and_ledger_fallbacks(
    monkeypatch, tmp_path
):
    """A spawn-stamped node is row identity, not a name-derived fallback."""
    from fno.agents import registry as registry_mod
    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.registry import AgentEntry
    import fno.paths as paths_mod

    shared = tmp_path / "canonical"
    (shared / ".git").mkdir(parents=True)
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    monkeypatch.setattr(
        registry_mod,
        "load_registry",
        lambda: [
            AgentEntry(
                name="spawned-worker",
                harness="claude",
                harness_session_id="dddd4444-0000",
                cwd=str(shared),
                log_path="",
                created_at="2026-09-01T00:00:00Z",
                status="exited",
                origin="spawn",
                node="x-cafe",
            )
        ],
    )
    monkeypatch.setattr(
        claude_mod,
        "claude_agents_rows",
        lambda **k: ([{
            "sessionId": "dddd4444-0000",
            "state": "done",
            "cwd": str(shared),
        }], []),
    )

    rows, _warnings = watchdog.fleet_rows()

    assert rows[0].node == "x-cafe"


def test_sweep_payload_shape():
    rows = [
        Row("aaaa1111-0000", "w1", "working", None, "/tmp"),
        Row("bbbb2222-0000", "w2", "stopped", None, "/tmp"),
        Row("cccc3333-0000", "w3", "done", None, "/tmp"),
        Row("dddd4444-0000", "w4", "exited", None, "/tmp"),
    ]
    payload, out_rows = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: _facts("ok"),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert payload["generated_at"] == "2026-08-16T18:40:00Z"
    assert payload["verdicts"][0]["verdict"] == LEAVE
    assert payload["terminal_harness_rows"] == 3
    assert payload["provider_outages"] == {
        "instrument": "unknown",
        "breakers": [],
        "counts": {"provider_outage_collector_missing": 1},
        "refusals": [{"reason": "provider_outage_collector_missing", "count": 1}],
    }
    # Rows ride along index-aligned so apply lanes can reach each cwd.
    assert out_rows == rows


def _recovery_candidate(tmp_path, index, *, usable=True):
    from fno.agents.discover import RecoverableCodexRollout

    session_id = f"01a039bb-0000-7000-8000-{index:012x}"
    rollout = tmp_path / f"rollout-{session_id}.jsonl"
    records = [
        {
            "type": "session_meta",
            "payload": {"id": session_id, "cwd": str(tmp_path)},
        }
    ]
    if usable:
        records.append(
            {
                "timestamp": "2026-08-16T18:40:00Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "resume marker"}],
                },
            }
        )
    rollout.write_text(
        "".join(json.dumps(record) + "\n" for record in records),
        encoding="utf-8",
    )
    mtime = rollout.stat().st_mtime
    last_event_at = "2026-08-16T18:40:00Z"
    return RecoverableCodexRollout(
        session_id=session_id,
        cwd=str(tmp_path),
        rollout_path=rollout,
        mtime=mtime,
        transcript_usable=usable,
        last_event_at=last_event_at if usable else None,
        last_turn_marker="resume marker" if usable else None,
        unusable_reason=None if usable else "no_readable_transcript_turn",
    )


def test_recoverable_sweep_reports_discovered_usable_and_unusable_counts(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    usable = _recovery_candidate(tmp_path, 1)
    unusable = _recovery_candidate(tmp_path, 2, usable=False)
    scan = CodexRecoveryScan((usable, unusable), True, 2, 0, 0, ())

    payload, rows, result = watchdog.run_recoverable_sweep(
        cwd=tmp_path,
        recency_seconds=24 * 3600,
        now_s=NOW_1840,
        scan_fn=lambda *args, **kwargs: scan,
    )

    assert result is scan
    assert len(rows) == 2
    assert payload["recoverable_count"] == 2
    assert payload["usable_recoverable_count"] == 1
    assert payload["unusable_recoverable_count"] == 1
    by_id = {row["row_id"]: row for row in payload["verdicts"]}
    assert by_id[usable.session_id]["action"] == "adopt"
    assert by_id[unusable.session_id]["action"] == "refuse"
    assert "no_readable_transcript_turn" in by_id[unusable.session_id]["basis"]


def test_recoverable_apply_refuses_unusable_candidate_without_writing(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 3, usable=False)
    adopted = []

    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
        adopt_fn=lambda *args, **kwargs: adopted.append(args),
    )

    assert adopted == []
    assert results == [
        {
            "session_id": candidate.session_id,
            "outcome": "refused",
            "reason": "transcript_unusable",
            "transcript_usable": False,
            "detail": "transcript_unusable: no_readable_transcript_turn",
        }
    ]


def test_recoverable_apply_refuses_transcript_loss_after_adoption(tmp_path):
    from types import SimpleNamespace
    from fno import paths
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 4)
    registry_rows = []

    def adopt(_hit, **_kwargs):
        registry_rows.append(
            SimpleNamespace(
                harness="codex",
                harness_session_id=candidate.session_id,
                cwd=candidate.cwd,
                origin="adopted",
                name="01a039bb",
                log_path=str(
                    paths.state_dir() / "agents" / candidate.session_id / "output.jsonl"
                ),
            )
        )
        candidate.rollout_path.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": candidate.session_id,
                        "cwd": candidate.cwd,
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )

    def update_registry(updater, **_kwargs):
        registry_rows[:] = updater(list(registry_rows))
        return list(registry_rows)

    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
        adopt_fn=adopt,
        confine_fn=lambda token, hits, **kwargs: hits,
        load_registry_fn=lambda path: list(registry_rows),
        update_registry_fn=update_registry,
    )

    assert registry_rows == []
    assert results[0]["outcome"] == "refused"
    assert results[0]["reason"] == "transcript_changed"
    assert results[0]["transcript_usable"] is False
    assert results[0]["registry_rollback"] == "removed"
    assert "applied" not in results[0]["detail"]


def test_recoverable_apply_requires_complete_coverage_before_adoption(monkeypatch, tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    scan = CodexRecoveryScan(
        (), False, 1, 1, 0, ("rollout JSON failed: broken.jsonl",)
    )
    adopted = []

    results = watchdog.apply_recoverable(
        scan,
        scope_cwd=tmp_path,
        adopt_fn=lambda *args, **kwargs: adopted.append(args),
    )

    assert adopted == []
    assert results == [{
        "session_id": None,
        "outcome": "refused",
        "detail": "recovery coverage incomplete: rollout JSON failed: broken.jsonl",
    }]


def test_cli_recoverable_rejects_invalid_since_before_scanning(monkeypatch, tmp_path, capsys):
    from fno.agents import cli as agents_cli

    with pytest.raises(typer.Exit) as exc:
        agents_cli.cmd_watchdog(
            json_out=False,
            apply=False,
            apply_all=False,
            only=watchdog.RECOVERABLE,
            mail_to="",
            since="yesterday",
            cwd=str(tmp_path),
        )

    assert exc.value.exit_code == 2
    assert "invalid since" in capsys.readouterr().err


def test_recoverable_apply_checks_the_exact_adopted_registry_row(tmp_path):
    from types import SimpleNamespace
    from fno import paths
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 5)
    session_id = candidate.session_id
    scan = CodexRecoveryScan((candidate,), True, 1, 0, 0, ())
    adopted = []
    adopt_kwargs = {}

    def adopt(hit, **kwargs):
        adopted.append(hit)
        adopt_kwargs.update(kwargs)

    results = watchdog.apply_recoverable(
        scan,
        scope_cwd=tmp_path,
        adopt_fn=adopt,
        confine_fn=lambda token, hits, **kwargs: hits,
        load_registry_fn=lambda path: [SimpleNamespace(
            harness="codex",
            harness_session_id=session_id,
            cwd=str(tmp_path),
            origin="adopted",
            name="019f48e1",
            log_path=str(
                paths.state_dir() / "agents" / session_id / "output.jsonl"
            ),
        )],
    )

    # The follow-up path rides the adoption write, not a later patch.
    assert adopt_kwargs["log_path"] == str(
        paths.state_dir() / "agents" / session_id / "output.jsonl"
    )

    assert len(adopted) == 1
    assert results == [
        {
            "session_id": session_id,
            "outcome": "applied",
            "transcript_usable": True,
            "last_event_at": candidate.last_event_at,
            "last_turn_marker": candidate.last_turn_marker,
            "registry_row_count": 1,
            "detail": f"adopted {session_id} handle=019f48e1",
        }
    ]


def test_recoverable_apply_refuses_a_row_without_the_follow_up_path(tmp_path):
    from types import SimpleNamespace
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 7)
    session_id = candidate.session_id

    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
        adopt_fn=lambda hit, **kwargs: None,
        confine_fn=lambda token, hits, **kwargs: hits,
        load_registry_fn=lambda path: [SimpleNamespace(
            harness="codex",
            harness_session_id=session_id,
            cwd=candidate.cwd,
            origin="adopted",
            name="019f48e1",
            log_path="",
        )],
    )

    assert results[0]["outcome"] == "refused"
    assert results[0]["reason"] == "adoption_failed"
    assert "follow-up path" in results[0]["detail"]


def test_recoverable_apply_names_a_vanished_candidate(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 6)
    candidate.rollout_path.unlink()
    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
    )

    assert results[0]["outcome"] == "refused"
    assert "vanished" in results[0]["detail"]


def test_ac1_det_production_collector_joins_explicit_registry_route_identity(
    monkeypatch, tmp_path
):
    fup = (
        "API Error: Request rejected (429): Fair Usage Policy; "
        "submit a request to restore access"
    )
    paths = {}
    entries = []
    rows = []
    for index in (1, 2):
        sid = f"session-{index}"
        path = tmp_path / f"{sid}.jsonl"
        path.write_text(json.dumps({
            "type": "assistant",
            "timestamp": "2026-08-16T18:39:30Z",
            "isApiErrorMessage": True,
            "apiErrorStatus": 429,
            "message": {"role": "assistant", "content": fup},
        }) + "\n", encoding="utf-8")
        paths[sid] = path
        entries.append(SimpleNamespace(
            harness_session_id=sid,
            harness="claude",
            route_provider_id="anthropic",
            account_record_id="acct-a",
            cwd=str(tmp_path),
        ))
        rows.append(Row(sid, f"worker-{index}", "working", "x-abcd", str(tmp_path)))

    report = watchdog.measure_provider_outages(
        rows,
        now_s=NOW_1840,
        entries_provider=lambda: entries,
        transcript_path_for=lambda identity: paths[identity.session_id],
        journal=tmp_path / "provider-outages.json",
    )

    assert report["instrument"] == "measured"
    assert report["breakers"][0]["provider"] == "anthropic"
    assert report["breakers"][0]["account"] == "acct-a"


def test_ac6_edge_one_transcript_read_per_tick(monkeypatch, tmp_path):
    """x-581b AC6-EDGE: one tick reads and parses each transcript at most once.

    The default sweep path shares one parsed tail between the outage
    evidence collector and the tail classifier; before the sharing, both
    legs opened and parsed the same file. The counting fake sits at the read
    seam (tail_entries), not a timing measurement, and both consumers must
    have run: the provider report is measured from that parse and the
    verdicts are classified from it.
    """
    from fno.agents import provider_outage as po
    from fno.agents import registry as registry_mod

    rows = [
        Row("aaaa1111-0000", "w1", "working", None, str(tmp_path)),
        Row("bbbb2222-0000", "w2", "working", None, str(tmp_path)),
    ]
    monkeypatch.setattr(watchdog, "fleet_rows", lambda timeout=None: (rows, []))

    def record(text, *, is_error=False, status=None):
        return {
            "type": "assistant",
            "timestamp": "2026-08-16T18:39:00Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "isApiErrorMessage": is_error,
            "apiErrorStatus": status,
        }

    entries_by_sid = {
        "aaaa1111-0000": [
            record("429 quota", is_error=True, status=429),
            record("continuing"),
        ],
        "bbbb2222-0000": [record("healthy")],
    }
    reads: list[str] = []

    def counting_tail_entries(sid, cwd, *, agent="claude"):
        reads.append(sid)
        return entries_by_sid.get(sid)

    monkeypatch.setattr(watchdog, "tail_entries", counting_tail_entries)
    monkeypatch.setattr(
        registry_mod,
        "load_registry",
        lambda: [
            SimpleNamespace(
                harness="claude", harness_session_id="aaaa1111-0000",
                route_provider_id="zai", account_record_id="acct-a",
                cwd=str(tmp_path), mux=None,
            ),
            SimpleNamespace(
                harness="claude", harness_session_id="bbbb2222-0000",
                route_provider_id="zai", account_record_id="acct-a",
                cwd=str(tmp_path), mux=None,
            ),
        ],
    )
    monkeypatch.setattr(
        po, "journal_path", lambda: tmp_path / "provider-outages.json"
    )

    payload, _out_rows = watchdog.run_sweep(
        now_s=NOW_1840,
        claim_fn=lambda key: {},
        graph_fn=lambda: {},
    )

    # The AC itself: each transcript read exactly once, not once per consumer.
    assert reads == ["aaaa1111-0000", "bbbb2222-0000"]
    # The outage leg actually consumed the shared parse: the 429 record and
    # both healthy records were accepted as evidence from it.
    assert payload["provider_outages"]["instrument"] == "measured"
    assert payload["provider_outages"]["counts"]["accepted"] == 3
    # The classifier leg ran off the same entries, not a second read.
    assert len(payload["verdicts"]) == 2


def test_cli_default_report_escalates_findings_not_session_rows(monkeypatch):
    """The default surface escalates the unfinished-work findings and never
    builds a durable question out of session verdicts."""
    from fno.agents import cli as agents_cli
    from fno.agents import stale_escalate

    # The default path never reads the session sweep; pin that by refusing
    # if it does.
    monkeypatch.setattr(
        watchdog,
        "run_sweep",
        lambda **kw: (_ for _ in ()).throw(AssertionError("report must not sweep")),
    )
    finding = uw.Finding(
        kind=uw.KIND_STARTED,
        subject="x-7d02",
        basis="in_progress, claim free, idle 116h",
        clear_command="/fno:target x-7d02",
        node_id="x-7d02",
    )

    class _Snap:
        generated_at = "x"
        findings = (finding,)
        complete = True
        warnings = ()
        dimensions = {
            dim: uw.DimensionState(uw.MEASURED, 1 if dim == uw.KIND_STARTED else 0, None)
            for dim in uw.DIMENSIONS
        }

    monkeypatch.setattr(uw, "build_report", lambda roots, **kw: _Snap())
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    captured = []
    monkeypatch.setattr(
        stale_escalate,
        "escalate_unfinished",
        lambda findings, **kwargs: captured.extend(findings) or ("recorded", "q-uw"),
    )

    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=None, mail_to=""
    )

    assert [f.subject for f in captured] == ["x-7d02"]


# ---------------------------------------------------------------------------
# The mail lane (push, not pull) and its change gate
# ---------------------------------------------------------------------------

def _payload_two_rows():
    return {
        "verdicts": [
            {"row_id": "aaaa1111-0000", "name": "w1", "state": "working",
             "verdict": LEAVE, "basis": "reachable", "action": "none"},
            {"row_id": "dddd4444-0000", "name": "k1", "state": "blocked",
             "verdict": WAKE, "basis": "blocked 30m", "action": "resume"},
        ],
        "counts": {LEAVE: 1, WAKE: 1},
    }


def test_digest_is_house_style_and_names_the_basis():
    text = watchdog.digest_text(_payload_two_rows())
    assert "- wake k1: blocked 30m" in text
    assert "terminal harness rows: 0" in text
    # The mail lane is style-gated at send time; a semicolon or the modal
    # 'could' makes the whole send exit non-zero without delivering, and a
    # bare verdict line under the header reads as an illegal mid-paragraph
    # wrap (rule 6) - the first tick's digest was refused by exactly that
    # and never delivered. Every body line after the header must be a list
    # item, which is the legal block shape.
    assert ";" not in text
    assert "could" not in text.lower()
    body = text.splitlines()
    assert body[1] == ""
    for line in body[2:]:
        assert line.startswith("- "), f"digest body line not a list item: {line!r}"


def test_mail_digest_sends_on_change_and_skips_when_unchanged(monkeypatch, tmp_path):
    import json as _json

    (tmp_path / "watchdog-sweep.json").write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": "old-sig"}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json")
    sent = []

    def runner(argv, **kw):
        sent.append(argv)
        return _Proc(0, stdout="msg-1 delivered (hosted)\n")

    payload = _payload_two_rows()
    ok, receipt = watchdog.mail_digest(payload, "lead", runner=runner)
    assert ok and "delivered" in receipt
    assert sent and "mail" in " ".join(sent[0]) and "lead" in sent[0]

    # Same signature as the stored sweep: no second send, no spam.
    watchdog.write_sweep_file(
        "tick", payload["counts"], NOW_1840, watchdog.verdict_signature(payload)
    )
    sent.clear()
    ok2, detail2 = watchdog.mail_digest(payload, "lead", runner=runner)
    assert ok2 and "unchanged" in detail2
    assert sent == []


def test_staleness_reads_loud_and_never_clean(monkeypatch, tmp_path):
    import json as _json
    import os

    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)

    # No sweep ever ran: the loudest case, never a clean one.
    assert watchdog.sweep_staleness(now_s=NOW_1840)["stale"] is True

    path.write_text(_json.dumps({"source": "tick", "at": "x", "counts": {}}))
    fresh_age = 300
    os.utime(path, (NOW_1840 - fresh_age, NOW_1840 - fresh_age))
    assert watchdog.sweep_staleness(now_s=NOW_1840) == {
        "age_s": fresh_age, "stale": False, "source": "tick", "at": "x",
    }

    dead_age = 26 * 3600  # the measured case: a cadence dead for 26 hours
    os.utime(path, (NOW_1840 - dead_age, NOW_1840 - dead_age))
    s = watchdog.sweep_staleness(now_s=NOW_1840)
    assert s["stale"] is True and s["age_s"] == dead_age


def test_event_lane_suppresses_unchanged_rows(monkeypatch, tmp_path):
    """The mail lane speaks on change, and so must the event lane: stuck
    rows on a 600s tick would append thousands of events a day saying one
    thing. The sweep file carries the event lane's own last signature so
    the two change gates never share state."""
    (tmp_path / "watchdog-sweep.json").write_text(json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": "",
         "events_signature": "dddd4444-0000:wake"}
    ))
    monkeypatch.setattr(
        watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json"
    )
    assert watchdog._last_events_signature() == "dddd4444-0000:wake"
    payload = _payload_two_rows()  # wake row dddd4444, leave row aaaa1111
    assert watchdog.fresh_non_leave(
        payload, watchdog._last_events_signature()
    ) == set()
    # A verdict that changed for the same row is fresh again.
    payload["verdicts"][1]["verdict"] = GHOST
    fresh = watchdog.fresh_non_leave(payload, "dddd4444-0000:wake")
    assert fresh == {"dddd4444-0000"}


def test_failed_send_keeps_the_gate_open(monkeypatch, tmp_path):
    """A digest that failed to deliver must not advance the change gate: the
    stamp stays the PREVIOUS signature so the next sweep retries instead of
    swallowing the verdict behind a signature it never sent."""
    import json as _json

    (tmp_path / "watchdog-sweep.json").write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": "old-sig"}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json")

    ok, receipt, stamp = watchdog.mail_gate(
        _payload_two_rows(), "lead", runner=lambda *a, **k: _Proc(1, stderr="boom")
    )
    assert not ok
    assert stamp == "old-sig"

    # Delivered: the current signature advances the gate.
    ok2, _, stamp2 = watchdog.mail_gate(
        _payload_two_rows(), "lead", runner=lambda *a, **k: _Proc(0)
    )
    assert ok2 and stamp2 == watchdog.verdict_signature(_payload_two_rows())

    # No recipient configured: nothing to warn about, gate unchanged.
    ok3, _, stamp3 = watchdog.mail_gate(_payload_two_rows(), "")
    assert ok3 and stamp3 == "old-sig"


# ---------------------------------------------------------------------------
# x-4c87: a zero-row roster is an unreadable instrument, never an empty fleet
# ---------------------------------------------------------------------------

def _refused_payload():
    payload, rows = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: ([], ["claude agents --json failed: exit 1"]),
    )
    assert rows == []
    return payload


def test_refused_sweep_writes_no_file_and_advances_no_gate(monkeypatch, tmp_path):
    import json as _json

    sweep = tmp_path / "watchdog-sweep.json"
    sweep.write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {"wake": 1}, "signature": "prev"}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: sweep)
    payload = _refused_payload()

    fired = []

    def runner(argv, **kw):
        fired.append(argv)
        return _Proc(0)

    ok, receipt = watchdog.mail_digest(payload, "lead", runner=runner)
    assert not ok and payload["refused"] in receipt
    ok2, receipt2, stamp = watchdog.mail_gate(payload, "lead", runner=runner)
    assert not ok2 and stamp == "prev"
    assert fired == []  # nothing sent: zero rows read is not zero rows found

    # The stored sweep file is untouched - mtime and signature both survive,
    # so staleness (not a clean write) is what the next status read shows.
    stored = _json.loads(sweep.read_text())
    assert stored["signature"] == "prev" and stored["counts"] == {"wake": 1}


def test_cli_refused_sweep_exits_loud_without_writing(monkeypatch, tmp_path):
    from fno.agents import cli as agents_cli

    refused = _refused_payload()  # built BEFORE the patch it would recurse into
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (refused, []))
    wrote = []
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: wrote.append(a))
    monkeypatch.setattr(watchdog, "mail_gate", lambda *a, **k: (_ for _ in ()).throw(
        AssertionError("mail must not run on a refused sweep")
    ))

    try:
        agents_cli.cmd_watchdog(json_out=False, apply=False, apply_all=False,
                                only=LEAVE, mail_to=None)
    except typer.Exit as e:
        assert e.exit_code == 3
    else:
        raise AssertionError("a refused sweep must exit non-zero")
    assert wrote == []


def test_manual_apply_survives_one_crashing_row_and_emits_verdicts(
    monkeypatch, tmp_path
):
    """One broken row never aborts the rest of an --apply run, and the
    classification events ride apply modes exactly like the tick's."""
    import contextlib
    import io as _io
    from fno.agents import cli as agents_cli

    verdicts = [
        Verdict("aaaa1111-0000", "w1", "stopped", WAKE, "blocked 30m", "resume"),
        Verdict("bbbb2222-0000", "w2", "stopped", WAKE, "blocked 30m", "resume"),
    ]
    rows = [Row("aaaa1111-0000", "w1", "stopped", None, "/tmp/w1"),
            Row("bbbb2222-0000", "w2", "stopped", None, "/tmp/w2")]
    payload = {"generated_at": "x", "verdicts": [v._asdict() for v in verdicts],
               "counts": {WAKE: 2}, "warnings": []}
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, rows))
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "mail_gate", lambda *a, **k: (True, "no recipient", "")
    )

    def crashy(v, **kw):
        if v.row_id.startswith("aaaa"):
            raise TypeError("boom")
        return "applied", "woke w2"

    monkeypatch.setattr(watchdog, "apply_verdict", crashy)
    events = []
    monkeypatch.setattr(
        watchdog, "emit_event",
        lambda kind, data, **kw: events.append(kind),
    )

    out = _io.StringIO()
    with contextlib.redirect_stdout(out):
        agents_cli.cmd_watchdog(json_out=True, apply=True, apply_all=False,
                                only=None, mail_to="")
    data = json.loads(out.getvalue().strip().splitlines()[-1])
    by_row = {r["row_id"]: r for r in data["results"]}
    assert by_row["aaaa1111-0000"]["outcome"] == "refused"
    assert "crashed" in by_row["aaaa1111-0000"]["detail"]
    assert by_row["bbbb2222-0000"]["outcome"] == "applied"
    # Verdict events fire in apply mode too, matching the tick's contract.
    assert events.count("watchdog_verdict") == 2


# ---------------------------------------------------------------------------
# Review round: the enumeration budget, shared worktrees, and honest receipts
# ---------------------------------------------------------------------------


def test_row_state_survives_an_alias_rename_and_odd_casing():
    """A raw ``r["state"]`` read reads "" under a rename, and "" is in no lane
    set - the whole fleet would classify leave behind a fresh sweep file."""
    assert watchdog._row_state({"status": "Working"}) == ("working", "")
    # `state` wins when both are present, matching _STATUS_KEYS' order.
    assert watchdog._row_state({"state": "blocked", "status": "idle"}) == ("blocked", "")
    # Both spellings of blocked fold onto one word. Missing this is what let
    # a `needs input` row fall through every lane to leave.
    assert watchdog._row_state({"state": "needs input"}) == ("blocked", "")
    assert watchdog._row_state({"state": "busy"}) == ("working", "")
    # An unknown spelling is loud, never a silent no-lane row.
    state, warning = watchdog._row_state({"state": "quiescing"})
    assert state == "quiescing" and "unmapped" in warning
    assert watchdog._row_state({})[1]


def test_a_bounded_caller_spends_its_own_budget_not_the_lanes(monkeypatch):
    """The tick's wall-clock deadline is the shorter budget AND the fatal one.

    A roster probe that outlives it raises the tick's deadline signal, which
    exits 75 and kills every later leg. Measured: arming the lane on the tick
    with the standalone 30s budget timed the whole tick out. So a bounded
    caller passes what it has left and the enumeration must honour it.
    """
    from fno.agents.harnesses import claude as claude_mod

    spent = {}

    def fake_rows(timeout=3.0):
        spent["timeout"] = timeout
        return [], []

    monkeypatch.setattr(claude_mod, "claude_agents_rows", fake_rows)
    watchdog.fleet_rows(timeout=4.0)
    assert spent["timeout"] == 4.0
    # The lane's own budget is still the CEILING, never a floor a caller
    # can raise past.
    watchdog.fleet_rows(timeout=999.0)
    assert spent["timeout"] == watchdog.ROSTER_TIMEOUT_S
    # An exhausted tick still probes rather than passing a zero timeout.
    watchdog.fleet_rows(timeout=0.0)
    assert spent["timeout"] == 1.0


def test_the_roster_refusal_carries_its_cause(monkeypatch, tmp_path, capsys):
    """The refusal says the roster was unreadable; the warnings say WHY.
    Dropping them leaves the one actionable line on the floor."""
    import contextlib
    import io as _io

    import pytest

    from fno.agents import cli as agents_cli

    payload = {"refused": "0 rows", "warnings": ["claude agents --json timed out after 30.0s"],
               "verdicts": [], "counts": {}}
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, []))
    err = _io.StringIO()
    with contextlib.redirect_stderr(err), pytest.raises(typer.Exit):
        agents_cli.cmd_watchdog(json_out=False, apply=False, apply_all=False,
                                only=LEAVE, mail_to=None)
    assert "timed out after 30.0s" in err.getvalue()


def test_a_watchdog_event_uses_a_source_the_schema_accepts(tmp_path, monkeypatch):
    """A source outside the envelope enum raised inside _build, and the
    swallow turned that into the whole manual lane writing NOTHING while
    every receipt still read normally. The event has to actually land."""
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    watchdog.emit_event(
        "watchdog_verdict",
        {"row_id": "aaaa1111-0000", "name": "w1", "verdict": WAKE, "basis": "b"},
    )
    from tests._event_rows import event_rows

    record = event_rows(tmp_path / "events.jsonl")[-1]
    assert record["type"] == "watchdog_verdict"
    assert record["source"] == "daemon"


# ---------------------------------------------------------------------------
# The reap predicate: one gate, and every failed read is UNKNOWN
# ---------------------------------------------------------------------------


def test_a_hand_run_sweep_does_not_report_the_cadence_stale(tmp_path, monkeypatch):
    """One file serves both cadences, so a manual write used to erase the
    only evidence the tick had run. Status then blamed the daemon for the
    operator having looked."""
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "sweep.json")

    watchdog.write_sweep_file("tick", {LEAVE: 3}, NOW_1840)
    watchdog.write_sweep_file("manual", {LEAVE: 3}, NOW_1840 + 60)

    state = watchdog.sweep_staleness(NOW_1840 + 120, stale_after_s=3600)
    assert state["stale"] is False, state
    # And a cadence that really died still reads stale, hand-run or not.
    dead = watchdog.sweep_staleness(NOW_1840 + 7200, stale_after_s=3600)
    assert dead["stale"] is True, dead


# ---------------------------------------------------------------------------
# the unclaimed advisory (x-cd1e)
# ---------------------------------------------------------------------------


def test_a_live_row_on_an_unclaimed_node_is_flagged():
    """Nothing today notices a live worker on a node no claim covers. Seven of
    nine nodes named by a live worker read free on 2026-08-19."""
    rows = [Row("cccc3333-0000", "t-x76d1-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0000": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "free"}},
    )
    assert v.verdict == UNCLAIMED
    assert "node x-76d1 carries NO claim while this row is live" in v.basis


def test_the_advisory_reaches_the_digest():
    """LEAVE rows are filtered out of every report, so a verdict that reported
    nothing would be a guard nobody ever reads."""
    from fno.agents.watchdog import digest_text

    v = Verdict("cccc3333-0006", "t-worker", "working", UNCLAIMED,
                "node x-76d1 carries NO claim while this row is live", "none")
    payload = {
        "verdicts": [v._asdict()],
        "counts": {UNCLAIMED: 1},
        "terminal_harness_rows": 0,
    }
    assert "t-worker" in digest_text(payload)


def test_codex_sandbox_denial_is_a_reap_verdict(monkeypatch):
    rows = [
        Row("cccc3333-0011", "t-sandbox", "working", "x-sandbox", "/tmp/w", "codex")
    ]
    monkeypatch.setattr(watchdog, "_branch_commit_count", lambda cwd: 0)
    [v] = _run(
        rows,
        {
            "cccc3333-0011": _facts(
                '<help reason="Codex sandbox blocks Git writes" '
                'evidence=".git/refs/heads/feature/x-sandbox.lock: '
                'Operation not permitted">'
            )
        },
        claims={"x-sandbox": {"state": "free"}},
    )
    assert v.verdict == SANDBOX_BLOCKED
    assert v.action == "reap"
    assert "sandbox" in v.basis.lower()


def test_codex_sandbox_denial_in_user_text_is_not_reaped(monkeypatch):
    monkeypatch.setattr(watchdog, "_branch_commit_count", lambda cwd: 0)
    rows = [
        Row("cccc3333-0013", "t-sandbox", "working", "x-sandbox", "/tmp/w", "codex")
    ]
    [v] = _run(
        rows,
        {
            "cccc3333-0013": _facts(
                '<help reason="Codex sandbox blocks Git writes" '
                'evidence=".git/refs/heads/feature/x-sandbox.lock: '
                'Operation not permitted">',
                role="user",
            )
        },
        claims={"x-sandbox": {"state": "free"}},
    )
    assert v.verdict != SANDBOX_BLOCKED
    assert v.action == "none"


@pytest.mark.parametrize(
    "text",
    [
        '<help reason="Codex sandbox blocks Git writes" '
        'evidence=".git/refs/heads/feature/x.lock: Operation not permitted',
        '<help reason="data-evidence=.git/refs/heads/feature/x.lock: '
        'Operation not permitted">',
    ],
)
def test_incomplete_or_mismatched_help_evidence_is_not_reaped(text, monkeypatch):
    monkeypatch.setattr(watchdog, "_branch_commit_count", lambda cwd: 0)
    rows = [
        Row("cccc3333-0014", "t-sandbox", "working", "x-sandbox", "/tmp/w", "codex")
    ]
    [v] = _run(
        rows,
        {"cccc3333-0014": _facts(text)},
        claims={"x-sandbox": {"state": "free"}},
    )
    assert v.verdict != SANDBOX_BLOCKED
    assert v.action == "none"


def test_claude_row_with_same_text_is_not_a_codex_sandbox_reap():
    rows = [Row("cccc3333-0016", "t-claude", "working", "x-sandbox", "/tmp/w")]
    [v] = _run(
        rows,
        {
            "cccc3333-0016": _facts(
                '<help reason="Codex sandbox blocks Git writes" '
                'evidence=".git/refs/heads/feature/x.lock: '
                'Operation not permitted">'
            )
        },
        claims={"x-sandbox": {"state": "free"}},
    )
    assert v.verdict != SANDBOX_BLOCKED
    assert v.action == "none"


@pytest.mark.parametrize(
    "claims,commit_count,guard_text",
    [
        ({"x-sandbox": {"state": "live", "holder": "target-session:other"}}, 0, "claim"),
        ({"x-sandbox": {"state": "free"}}, 2, "commit"),
    ],
)
def test_codex_sandbox_denial_stays_when_a_guard_holds(
    monkeypatch, claims, commit_count, guard_text
):
    monkeypatch.setattr(watchdog, "_branch_commit_count", lambda cwd: commit_count)
    rows = [
        Row("cccc3333-0012", "t-sandbox", "working", "x-sandbox", "/tmp/w", "codex")
    ]
    [v] = _run(
        rows,
        {
            "cccc3333-0012": _facts(
                '<help reason="Codex sandbox blocks Git writes" '
                'evidence=".git/refs/heads/feature/x-sandbox.lock: '
                'Operation not permitted">'
            )
        },
        claims=claims,
    )
    assert v.verdict != SANDBOX_BLOCKED
    assert v.action == "none"
    assert guard_text in v.basis.lower()


@pytest.mark.parametrize(
    "claim,commit_count,guard_text",
    [
        ({"state": "live", "holder": "target-session:new"}, 0, "claim"),
        ({"state": "free"}, 1, "commit"),
    ],
)
def test_sandbox_reap_rechecks_guards_before_removal(
    monkeypatch, claim, commit_count, guard_text
):
    calls = []
    monkeypatch.setattr(watchdog, "_claim_view", lambda node: claim)
    monkeypatch.setattr(watchdog, "_branch_commit_count", lambda cwd: commit_count)
    v = Verdict(
        "cccc3333-0015",
        "t-sandbox",
        "working",
        SANDBOX_BLOCKED,
        "Codex sandbox blocked Git writes",
        "reap",
    )

    outcome, detail = apply_verdict(
        v,
        lanes="all",
        cwd="/tmp/w",
        node="x-sandbox",
        runner=lambda *args, **kwargs: calls.append((args, kwargs)),
    )

    assert outcome == "held"
    assert guard_text in detail
    assert calls == []


def test_sandbox_reap_requires_current_distress_before_removal(monkeypatch):
    monkeypatch.setattr(watchdog, "_claim_view", lambda node: {"state": "free"})
    monkeypatch.setattr(watchdog, "_branch_commit_count", lambda cwd: 0)
    monkeypatch.setattr(
        watchdog,
        "tail_facts",
        lambda *args, **kwargs: _facts("ordinary assistant completion"),
    )
    calls = []
    v = Verdict(
        "cccc3333-0017",
        "t-sandbox",
        "working",
        SANDBOX_BLOCKED,
        "Codex sandbox blocked Git writes",
        "reap",
    )

    outcome, detail = apply_verdict(
        v,
        lanes="all",
        cwd="/tmp/w",
        node="x-sandbox",
        runner=lambda *args, **kwargs: calls.append((args, kwargs)),
    )

    assert outcome == "refused"
    assert "current transcript" in detail
    assert calls == []


# ---------------------------------------------------------------------------
# The unfinished-work report: the operator surface, distinct from the verdict
# classifier above. The classifier stays the internal recovery engine; the
# report answers the outcome question (was work started and never finished?)
# over four dimensions, each row naming the one command that clears it.
# ---------------------------------------------------------------------------

from pathlib import Path  # noqa: E402

from fno.agents import unfinished_work as uw  # noqa: E402


def _probe(
    handle: str = "sess-0001",
    *,
    pid_alive=None,
    transcript_age_s=None,
    last_activity_basis=None,
    claim_state=None,
    stored_exited=False,
):
    return uw.OwnerProbe(
        handle=handle,
        pid_alive=pid_alive,
        transcript_age_s=transcript_age_s,
        last_activity_basis=last_activity_basis,
        claim_state=claim_state,
        stored_exited=stored_exited,
    )


def _node_obs(
    node_id: str,
    *,
    status: str = "in_progress",
    claim_state: str = "free",
    touched_epoch=None,
    worktree=None,
    ahead=None,
    probes=(),
    cwd=None,
    is_container=False,
):
    return uw.NodeObs(
        node_id=node_id,
        status=status,
        touched_at_epoch=touched_epoch,
        cwd=cwd,
        worktree_path=worktree,
        ahead_count=ahead,
        claim={"state": claim_state} if claim_state is not None else None,
        owner_probes=tuple(probes),
        is_container=is_container,
    )


def _wt_obs(
    path: str,
    *,
    dirty=0,
    ahead=None,
    node_id=None,
    probes=(),
    repo="/repo",
):
    return uw.WorktreeObs(
        path=path,
        repo_root=repo,
        branch=None,
        dirty_count=dirty,
        ahead_count=ahead,
        node_id=node_id,
        owner_probes=tuple(probes),
    )


def _pr_obs(
    number: int,
    *,
    state="OPEN",
    opened_epoch=None,
    node="x-prnode",
    probes=(),
    url=None,
):
    return uw.PrObs(
        pr_number=number,
        pr_url=url or f"https://github.com/o/r/pull/{number}",
        node_id=node,
        state=state,
        opened_at_epoch=opened_epoch,
        owner_probes=tuple(probes),
    )


def _uw_obs(
    nodes=(),
    worktrees=(),
    prs=(),
    *,
    graph_ok=True,
    claims_ok=True,
    registry_ok=True,
    github_ok=True,
    unscanned_roots=(),
    warnings=(),
    now_s=NOW_1840,
):
    return uw.Observations(
        now_epoch=now_s,
        graph_ok=graph_ok,
        claims_ok=claims_ok,
        registry_ok=registry_ok,
        github_ok=github_ok,
        nodes=tuple(nodes),
        worktrees=tuple(worktrees),
        prs=tuple(prs),
        unscanned_roots=tuple(unscanned_roots),
        warnings=tuple(warnings),
    )


# --- AC1: the report answers the outcome question ------------------------


def test_ac1_clean_scan_renders_four_measured_zero_dimensions():
    snap = uw.classify(_uw_obs())
    payload = uw.snapshot_payload(snap)
    text = uw.snapshot_digest(snap)

    assert snap.complete is True
    assert payload["findings"] == []
    assert payload["counts"] == {dim: 0 for dim in uw.DIMENSIONS}
    assert text.splitlines()[0] == "unfinished work: 0 finding(s)"
    for dim in uw.DIMENSIONS:
        assert f"{dim}=0 measured" in text


# --- AC2: liveness is read from pid/transcript, never a stored word -------


def test_ac2_live_pid_defeats_any_stored_terminal_word():
    probe = _probe(pid_alive=True, transcript_age_s=99999.0)
    assert uw.owner_verdict(probe) == "live"

    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/fix-scoreboard-clock", dirty=82, probes=[probe])])
    )
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.MEASURED


# --- x-dead: the owner verdict routes through the shared predicate ---------


def test_xdead_mtime_basis_is_never_positive_evidence():
    # Measured 2026-09-11: a transcript mtime read 2h33m past its newest
    # record because the file is touched with no record appended. An
    # active-looking age taken from a file stamp reads UNKNOWN.
    assert uw.owner_verdict(_probe(transcript_age_s=30.0, last_activity_basis="mtime")) == "unknown"


def test_xdead_true_positive_keeps_its_clearing_command():
    # x-77db, measured: the one genuinely ownerless dirty tree of the five
    # the watchdog reported that hour. The fix must not suppress it; the
    # finding carries the target command because the GONE was positive.
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/x-77db", dirty=4, node_id="x-77db")])
    )
    [finding] = snap.findings
    assert finding.kind == uw.KIND_DIRTY
    assert "no live owner" in finding.basis
    assert "/fno:target x-77db" in finding.clear_command


def test_ac2_unreadable_liveness_is_unknown_not_ownerless():
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/x", dirty=5, probes=[_probe()])])
    )
    assert snap.findings == ()
    dim = snap.dimensions[uw.KIND_DIRTY]
    assert dim.state == uw.UNKNOWN_DIM
    assert snap.complete is False


def test_ac2_no_candidates_is_gone_only_when_the_store_read():
    """A worktree nobody registered on reads ownerless only when the registry
    read succeeded; a failed read must not manufacture an ownerless verdict."""
    snap = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=5)]))
    assert [f.kind for f in snap.findings] == [uw.KIND_DIRTY]

    snap2 = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=5)], registry_ok=False))
    assert snap2.findings == ()
    assert snap2.dimensions[uw.KIND_DIRTY].state == uw.UNKNOWN_DIM


# --- AC3: started nodes with free claims ---------------------------------


def test_ac3_started_free_claim_nodes_carry_target_commands_and_idle_order():
    older = _node_obs(
        "x-7d02", touched_epoch=NOW_1840 - 116 * 3600, worktree="/w/x-7d02", ahead=8
    )
    newer = _node_obs(
        "x-3b05", touched_epoch=NOW_1840 - 10 * 3600, worktree="/w/x-3b05", ahead=6
    )
    snap = uw.classify(_uw_obs(nodes=[newer, older]))

    assert [f.subject for f in snap.findings] == ["x-7d02", "x-3b05"]
    assert all(f.kind == uw.KIND_STARTED for f in snap.findings)
    assert all(f.clear_command == f"/fno:target {f.subject}" for f in snap.findings)
    assert snap.findings[0].age_s == pytest.approx(116 * 3600)
    assert snap.findings[0].ahead_count == 8
    assert "origin/main" in snap.findings[0].basis
    assert snap.findings[1].basis.count("origin/main") == 1


def test_collect_observations_marks_graph_parents_as_containers():
    observations = uw.collect_observations(
        [],
        graph_entries=[
            {"id": "x-parent", "status": "in_progress"},
            {"id": "x-child", "status": "done", "parent": "x-parent"},
        ],
        registry_rows=({}, True),
        claim_status_fn=lambda _node_id: {"state": "free"},
        pr_candidates=[],
    )

    containers = {node.node_id: node.is_container for node in observations.nodes}
    assert containers == {"x-parent": True, "x-child": False}


def test_ac3_non_free_claim_or_live_owner_excludes_the_node():
    held = _node_obs("x-held", claim_state="live")
    suspect = _node_obs("x-susp", claim_state="suspect")
    stale_lease = _node_obs("x-lease", claim_state="stale")
    corrupted = _node_obs("x-corr", claim_state="corrupted")
    owned = _node_obs("x-owned", probes=[_probe(pid_alive=True)])

    snap = uw.classify(
        _uw_obs(nodes=[held, suspect, stale_lease, corrupted, owned])
    )
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_STARTED].state == uw.MEASURED


# --- AC4: done nodes whose branch never reached main ----------------------


def test_ac4_done_ahead_of_main_names_count_and_squash_reading():
    snap = uw.classify(
        _uw_obs(
            nodes=[
                _node_obs("x-aaae", status="done", worktree="/w/x-aaae"),
            ],
            worktrees=[_wt_obs("/w/x-aaae", dirty=0, ahead=66, node_id="x-aaae")],
        )
    )
    [finding] = snap.findings
    assert finding.kind == uw.KIND_DONE_AHEAD
    assert finding.subject == "x-aaae"
    assert finding.ahead_count == 66
    assert "66" in finding.basis
    assert "squash" in finding.basis
    assert "stranded" in finding.clear_command
    assert "/w/x-aaae" in finding.basis


def test_ac4_metric_reads_fresh_origin_main_not_the_tracking_ref(tmp_path):
    """The measured defect: a stale remote-tracking ref inflated the ahead
    count to 936 against a true 8. The fixture builds a real bare origin,
    rebases a feature branch onto an origin advance, then rewinds the local
    remote-tracking ref to the pre-advance commit: the tracking-ref count
    reads 11, the true origin/main..HEAD count is 8, and only a fresh fetch
    separates them."""
    import subprocess as _sp

    def _git(cwd, *argv, check=True):
        return _sp.run(
            ["git", "-C", str(cwd), *argv],
            capture_output=True, text=True, check=check,
        )

    origin = tmp_path / "origin.git"
    _sp.run(["git", "init", "-q", "--bare", str(origin)], check=True)
    clone = tmp_path / "clone"
    _sp.run(["git", "clone", "-q", str(origin), str(clone)], check=True)
    _git(clone, "config", "user.email", "t@t.t")
    _git(clone, "config", "user.name", "t")
    _git(clone, "checkout", "-q", "-b", "main")
    (clone / "a.txt").write_text("base\n", encoding="utf-8")
    _git(clone, "add", "a.txt")
    _git(clone, "commit", "-q", "-m", "base")
    _git(clone, "push", "-q", "-u", "origin", "main")
    base_sha = _git(clone, "rev-parse", "HEAD").stdout.strip()

    _git(clone, "checkout", "-q", "-b", "feature")
    for i in range(8):
        (clone / "a.txt").write_text(f"base\n{i}\n", encoding="utf-8")
        _git(clone, "add", "a.txt")
        _git(clone, "commit", "-q", "-m", f"f{i}")
    _git(clone, "branch", "--set-upstream-to=origin/main", "feature")

    # Advance origin/main from a second clone, rebase onto it, then rewind
    # the local remote-tracking ref: exactly the stale-ref condition.
    other = tmp_path / "other"
    _sp.run(["git", "clone", "-q", str(origin), str(other)], check=True)
    _git(other, "config", "user.email", "t@t.t")
    _git(other, "config", "user.name", "t")
    for i in range(3):
        (other / "b.txt").write_text(f"{i}\n", encoding="utf-8")
        _git(other, "add", "b.txt")
        _git(other, "commit", "-q", "-m", f"m{i}")
    _git(other, "push", "-q", "origin", "main")

    _git(clone, "fetch", "-q", "origin")
    _git(clone, "rebase", "-q", "origin/main")
    _git(clone, "update-ref", "refs/remotes/origin/main", base_sha)

    # The control: against the stale tracking ref the two-dot count lies
    # (3 main commits + 8 feature commits). This is the 936-equivalent
    # inflation the metric exists to refuse.
    inflated = _git(
        clone, "rev-list", "--count", "feature@{upstream}..feature"
    ).stdout.strip()
    assert int(inflated) == 11

    assert uw.fetch_origin_main(clone) is True
    assert uw.ahead_of_main(clone) == 8


# --- AC5: dirty worktrees with no owner -----------------------------------


def test_ac5_failed_git_status_marks_dimension_unknown():
    snap = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=None)]))
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.UNKNOWN_DIM


# --- AC6: ownerless open PRs (an unknown owner read waits 24h) -------------


def test_ac6_pr_age_boundary_and_command():
    # The owner question comes first: a PR whose owner set reads gone emits
    # at any age, so the young gone-owner PR lands beside the old one and the
    # 24h boundary now governs only the unknown-owner wait.
    old = _pr_obs(101, opened_epoch=NOW_1840 - 24 * 3600 - 1)
    young = _pr_obs(103, opened_epoch=NOW_1840 - 3600)
    snap = uw.classify(_uw_obs(prs=[old, young]))

    assert {finding.pr_number for finding in snap.findings} == {101, 103}
    by_number = {finding.pr_number: finding for finding in snap.findings}
    assert all(f.kind == uw.KIND_PR for f in snap.findings)
    assert all(
        f.clear_command == f"/fno:ship pr check {f.pr_number}" for f in snap.findings
    )
    assert "pull/101" in by_number[101].basis
    assert by_number[101].age_s == pytest.approx(24 * 3600 + 1)


def test_ac6_a_young_unknown_owner_waits_and_an_old_one_goes_unknown():
    young_unknown = _pr_obs(110, opened_epoch=NOW_1840 - 3600, probes=[_probe()])
    snap = uw.classify(_uw_obs(prs=[young_unknown]))
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_PR].state == uw.MEASURED

    old_unknown = _pr_obs(111, opened_epoch=NOW_1840 - 25 * 3600, probes=[_probe()])
    snap2 = uw.classify(_uw_obs(prs=[old_unknown]))
    assert snap2.findings == ()
    assert snap2.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM
    assert "owner liveness unreadable for pr 111" in snap2.dimensions[uw.KIND_PR].warning


def test_ac6_merged_live_owned_and_unreadable_prs_do_not_emit():
    merged = _pr_obs(104, state="MERGED", opened_epoch=NOW_1840 - 48 * 3600)
    live_owned = _pr_obs(
        105, opened_epoch=NOW_1840 - 48 * 3600, probes=[_probe(pid_alive=True)]
    )
    snap = uw.classify(_uw_obs(prs=[merged, live_owned]))
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_PR].state == uw.MEASURED

    unreadable = _uw_obs(prs=[_pr_obs(106, state=None, opened_epoch=NOW_1840 - 48 * 3600)])
    snap2 = uw.classify(unreadable)
    assert snap2.findings == ()
    assert snap2.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM

    snap3 = uw.classify(
        _uw_obs(
            prs=[_pr_obs(107, opened_epoch=NOW_1840 - 48 * 3600, probes=[_probe()])],
        )
    )
    assert snap3.findings == ()
    assert snap3.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM

    snap4 = uw.classify(_uw_obs(prs=[_pr_obs(108, opened_epoch=NOW_1840 - 48 * 3600)], github_ok=False))
    assert snap4.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM


# --- AC7: every finding names its clearing command ---------------------------


def test_ac7_a_finding_without_a_command_is_not_emitted(monkeypatch):
    monkeypatch.setattr(uw, "_dirty_clear_command", lambda w: "")
    snap = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=7)]))
    assert snap.findings == ()
    assert any("clearing command" in w for w in snap.warnings)


# --- AC8: a clean dimension is positive evidence --------------------------


def test_ac8_any_failed_read_blocks_complete():
    for kwargs in (
        {"graph_ok": False},
        {"claims_ok": False},
        {"registry_ok": False},
        {"github_ok": False},
        {"unscanned_roots": ("/repo/other",)},
    ):
        snap = uw.classify(_uw_obs(**kwargs))
        assert snap.complete is False, kwargs
        assert any(
            d.state == uw.UNKNOWN_DIM for d in snap.dimensions.values()
        ), kwargs


def test_ac8_ordering_is_deterministic_severity_then_age_then_subject():
    a = _node_obs("x-b", touched_epoch=NOW_1840 - 5 * 3600)
    b = _node_obs("x-a", touched_epoch=NOW_1840 - 5 * 3600)
    c = _node_obs("x-c", touched_epoch=NOW_1840 - 50 * 3600)
    snap = uw.classify(_uw_obs(nodes=[a, b, c], worktrees=[_wt_obs("/w/z", dirty=1)]))
    assert [f.subject for f in snap.findings] == ["x-c", "x-a", "x-b", "/w/z"]


# --- the digest and the signature -----------------------------------------


def test_digest_is_house_style_blocks_and_names_commands():
    snap = uw.classify(
        _uw_obs(nodes=[_node_obs("x-7d02", touched_epoch=NOW_1840 - 116 * 3600)])
    )
    text = uw.snapshot_digest(snap)
    lines = text.splitlines()
    assert lines[0] == "unfinished work: 1 finding(s)"
    assert any(ln.startswith("- ") for ln in lines)
    assert "/fno:target x-7d02" in text
    # One physical line per paragraph: exactly one blank separator before
    # the dimension block, every content line whole.
    assert lines.count("") == 1
    assert all(ln.strip() for ln in lines if ln != "")


def test_signature_keys_on_outcome_identity_not_rows():
    s1 = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 99 * 3600)]))
    s2 = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 1 * 3600)]))
    assert uw.snapshot_signature(s1) == uw.snapshot_signature(s2)

    s3 = uw.classify(_uw_obs(nodes=[_node_obs("x-2", touched_epoch=NOW_1840 - 99 * 3600)]))
    assert uw.snapshot_signature(s1) != uw.snapshot_signature(s3)


# --- AC9: one producer, two callers; budget refusal ------------------------


def test_ac9_budget_spent_before_scanning_reads_unknown_never_clean(tmp_path):
    """The tick's fatal deadline: roots and PR states that did not fit stay
    unread, their dimensions say unknown, and the snapshot is incomplete, so
    nothing partial is stamped or mailed as complete."""
    import time as _time

    snap = uw.build_report(
        [tmp_path],
        now_s=NOW_1840,
        graph_entries=[],
        registry_rows=({}, True),
        claim_status_fn=lambda node: {"state": "free"},
        truth_resolver=lambda handle: None,
        pr_candidates=[SimpleNamespace(node_id="x-1", pr_number=9, pr_url=None)],
        deadline_monotonic=_time.monotonic() - 1.0,
    )
    assert snap.complete is False
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.UNKNOWN_DIM
    assert snap.dimensions[uw.KIND_DONE_AHEAD].state == uw.UNKNOWN_DIM
    assert snap.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM


def test_report_event_gate_advances_without_a_mail_recipient(tmp_path, monkeypatch):
    """The event gate is its own stamp: with watchdog_mail_to empty the mail
    stamp never moves, and a gate chained to it would re-emit every finding
    event on every cadence. The second publish of an unchanged finding set
    emits no finding events."""
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    events = []
    monkeypatch.setattr(watchdog, "emit_event", lambda kind, data: events.append(kind))
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    from fno.agents import stale_escalate as _se

    monkeypatch.setattr(_se, "escalate_unfinished", lambda f, **kw: ("none", ""))

    snap = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)]))
    uw.publish_report(snap, source="manual", now_s=NOW_1840, mail_to="")
    first = [e for e in events if e == "watchdog_unfinished_work_finding"]
    assert len(first) == 1

    events.clear()
    uw.publish_report(snap, source="manual", now_s=NOW_1840 + 600, mail_to="")
    assert not [e for e in events if e == "watchdog_unfinished_work_finding"]


def test_report_write_preserves_the_verdict_lanes_stamps(tmp_path, monkeypatch):
    """The report and the verdict lane share one sweep file. A report write
    must carry the verdict lane's stamps through untouched, or the next
    --apply run re-mails an unchanged verdict digest."""
    import fno.paths as paths_mod

    sweep_file = tmp_path / "state" / "watchdog-sweep.json"
    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    from fno.agents import stale_escalate as _se

    monkeypatch.setattr(_se, "escalate_unfinished", lambda f, **kw: ("none", ""))

    watchdog.write_sweep_file(
        "manual", {"wake": 1}, NOW_1840, "row-a:wake", events_signature="row-a:wake",
        terminal_harness_rows=3,
    )
    snap = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)]))
    uw.publish_report(snap, source="manual", now_s=NOW_1840, mail_to="")

    after = json.loads(sweep_file.read_text())
    assert after["counts"] == {"wake": 1}
    assert after["signature"] == "row-a:wake"
    assert after["events_signature"] == "row-a:wake"
    assert after["terminal_harness_rows"] == 3
    assert after["unfinished_work_complete"] is True


# --- two defects the first live sweep caught -------------------------------


def test_pid_probe_answers_with_a_live_process():
    """Positive control: the pid probe must actually answer. The first live
    run read every claim-holder pid as unreadable (a wrong arity on the
    spawn-gate probe raised inside its own try/except), which made every
    held worktree unknown-liveness."""
    import os

    assert uw._default_pid_alive(os.getpid()) is True
    assert uw._default_pid_alive(None) is None


def test_clearing_commands_name_the_main_worktree_not_the_reporting_one(tmp_path):
    """A report run from inside a linked worktree must still scope its
    clearing commands to the repository's MAIN worktree: `git worktree list`
    from any linked tree lists the main tree first, and that is the root a
    human or agent should stand in."""
    import subprocess as _sp

    main = tmp_path / "main"
    main.mkdir()
    for argv in (
        ["init", "-q"],
        ["config", "user.email", "t@t.t"],
        ["config", "user.name", "t"],
    ):
        _sp.run(["git", "-C", str(main), *argv], check=True)
    (main / "a.txt").write_text("base\n", encoding="utf-8")
    _sp.run(["git", "-C", str(main), "add", "a.txt"], check=True)
    _sp.run(["git", "-C", str(main), "commit", "-q", "-m", "base"], check=True)
    linked = tmp_path / "linked"
    _sp.run(
        ["git", "-C", str(main), "worktree", "add", "-q", str(linked), "-b", "feat"],
        check=True,
    )

    obs = uw.collect_observations(
        [linked],
        now_s=NOW_1840,
        graph_entries=[],
        registry_rows=({}, True),
        claim_status_fn=lambda node: {"state": "free"},
        truth_resolver=lambda handle: None,
        pr_candidates=[],
    )
    assert obs.worktrees, "linked worktree must be enumerated"
    for w in obs.worktrees:
        if Path(w.path).name == "linked":
            assert w.repo_root == str(main)


# --- review-round fixes: unknown reads and one shared fleet scope ----------


def test_publish_escalates_what_an_incomplete_scan_reached(tmp_path, monkeypatch):
    """An incomplete scan escalates the findings it DID reach and names the
    dimensions it could not. Withholding the whole ask was permanent, not
    transient: the common cause is a deleted worktree root whose fetch fails
    on every future run, so the lane went mute forever while stdout kept
    printing the findings under exit 0."""
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    from fno.agents import stale_escalate as _se

    called: list[dict] = []

    def _record(findings, **kw):
        called.append({"findings": list(findings), **kw})
        return ("recorded", "q-1")

    monkeypatch.setattr(_se, "escalate_unfinished", _record)

    snap = uw.classify(
        _uw_obs(graph_ok=False, nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)])
    )
    assert snap.complete is False
    notes: list[str] = []
    uw.publish_report(
        snap, source="manual", now_s=NOW_1840, mail_to="", log=notes.append
    )
    assert len(called) == 1
    assert uw.KIND_STARTED in called[0]["unknown_dimensions"]
    assert any("incomplete:" in note for note in notes)


def test_manual_report_and_tick_share_one_fleet_scope(monkeypatch):
    """The manual command and the tick resolve their roots through one shared
    resolver, so a hand-run names the same fleet the tick's digest named."""
    from fno.agents import cli as agents_cli
    from fno.pr_watch import cli as prcli

    monkeypatch.setattr(uw, "report_roots", lambda: [Path("/fleet/scope")])
    assert prcli._watchdog_recovery_roots() == [Path("/fleet/scope")]

    class _Snap:
        generated_at = "x"
        findings = ()
        complete = True
        warnings = ()
        dimensions = {
            dim: uw.DimensionState(uw.MEASURED, 0, None) for dim in uw.DIMENSIONS
        }

    captured = {}

    def _fake_build(roots, **kw):
        captured["roots"] = [str(r) for r in roots]
        return _Snap()

    monkeypatch.setattr(uw, "build_report", _fake_build)
    monkeypatch.setattr(
        uw, "publish_report", lambda s, **kw: {"counts": {}, "findings": [], "warnings": []}
    )
    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=None, mail_to=""
    )
    assert captured["roots"] == ["/fleet/scope"]


def test_outcome_event_never_files_a_half_landed_action_as_a_refusal():
    """watchdog_refused reads "declined to act". A partial outcome changed the
    fleet - a rotated provider, a stopped session - so filing it there logs it
    as if nothing happened, and the reader who trusts that leaves it
    unattended. One fold so three emit sites cannot disagree."""
    assert watchdog.outcome_event("applied") == "watchdog_applied"
    assert watchdog.outcome_event(watchdog.PARTIAL) == "watchdog_partial"
    assert watchdog.outcome_event("refused") == "watchdog_refused"
    assert watchdog.outcome_event("held") == "watchdog_refused"


def test_unfinished_mail_gate_mails_an_incomplete_scan(tmp_path, monkeypatch):
    """A withheld incomplete scan reads "retry next sweep", but a deleted
    worktree root fails the same way forever. The digest names every unread
    dimension, so the mail carries its own shortfall."""
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(watchdog, "_last_unfinished_signature", lambda: "")
    sent: list[str] = []
    monkeypatch.setattr(
        watchdog,
        "_send_machine_report",
        lambda to, body, runner=None: sent.append(body) or (True, "sent"),
    )

    # Incomplete AND carrying findings: the dimension that read is the news,
    # the dimension that did not is the caveat.
    finding = uw.Finding(
        kind=uw.KIND_DIRTY,
        subject="/tmp/w1",
        basis="dirty and ownerless",
        clear_command="fno agents workspace worktree cleanup",
    )
    snap = uw.Snapshot(
        generated_at="2026-08-16T18:40:00Z",
        findings=(finding,),
        dimensions={
            uw.KIND_STARTED: uw.DimensionState(uw.UNKNOWN_DIM, 0, "graph unreadable"),
            uw.KIND_DONE_AHEAD: uw.DimensionState(uw.UNKNOWN_DIM, 0, "graph unreadable"),
            uw.KIND_DIRTY: uw.DimensionState(uw.MEASURED, 1, None),
            uw.KIND_PR: uw.DimensionState(uw.UNKNOWN_DIM, 0, "graph unreadable"),
        },
        warnings=("started_free_claim: graph unreadable",),
        complete=False,
    )
    assert snap.complete is False and snap.findings
    ok, receipt, stamp = watchdog.unfinished_mail_gate(snap, "someone")
    assert ok, receipt
    assert len(sent) == 1
    assert f"{uw.KIND_STARTED}={uw.UNKNOWN_DIM}" in sent[0]
    assert stamp == uw.snapshot_signature(snap)


def test_an_attributable_no_sid_row_is_advisory_and_named(monkeypatch):
    """A live registry row with no harness session id but a node attribution
    rides an advisory line naming the row, so the worked overlay can skip the
    one row instead of refusing the measure for every node (x-ae54)."""
    from types import SimpleNamespace

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents import registry as registry_mod

    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **k: ([], []))
    row = SimpleNamespace(
        harness="codex", status="live", harness_session_id=None,
        session_id=None, short_id=None, name="bp-a238-lead-brief",
        node="x-a238", cwd="/tmp/nowhere",
    )
    monkeypatch.setattr(registry_mod, "load_registry", lambda: [row])
    rows, warnings = watchdog.fleet_rows()
    assert rows == []
    advisory = [w for w in warnings if "unmeasurable-row: " in w]
    assert len(advisory) == 1
    assert "node=x-a238" in advisory[0]
    assert "name=bp-a238-lead-brief" in advisory[0]
    assert not any("carried no session id" in w for w in warnings)


def test_an_unattributable_no_sid_row_skips_advisory_and_stays_consulted(monkeypatch):
    """Without a node attribution the row is unknowable liveness: it skips,
    the summary warns advisory-classified, and the reading stays consulted.
    The summary once read as blocking, so one stuck no-provenance pane
    blanked the unplanned queue for every lead."""
    from types import SimpleNamespace

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents import registry as registry_mod
    from fno.claims import roster

    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **k: ([], []))
    row = SimpleNamespace(
        harness="codex", status="live", harness_session_id=None,
        session_id=None, short_id=None, name="bp-ghost", node=None,
        cwd="/tmp/nowhere",
    )
    monkeypatch.setattr(registry_mod, "load_registry", lambda: [row])
    rows, warnings = watchdog.fleet_rows()
    assert rows == []
    summary = next(w for w in warnings if "carried no session id" in w)
    assert summary.startswith(watchdog.ADVISORY_WARNING_PREFIX)

    # The classifier half: an advisory-classified summary never refuses the
    # reading.
    monkeypatch.setattr(watchdog, "fleet_rows", lambda timeout=10.0: (rows, warnings))
    reading = roster.read_roster(require_live_probe=False)
    assert reading.consulted is True


def test_stale_reset_default_report_carries_provider_outages(monkeypatch, capsys):
    """The default -J surface carries the outage instrument - open
    breakers and every refusal reason with a count - without reaching for
    --only. A failed measurement lands as a named unknown, never a missing
    key, and the human lines carry the same numbers."""
    import json as _json
    from collections import defaultdict
    from pathlib import Path as _Path
    from types import SimpleNamespace

    from fno.agents import cli as agents_cli
    from fno.agents import keeper_lane as keeper_lane_mod
    from fno.agents import unfinished_work as uw_mod

    measured = {
        "instrument": "measured",
        "breakers": [{
            "provider": "zai", "account": "zai", "kind": "fair_usage_policy",
            "outage_epoch": 1.0, "reset_at": 2.0,
        }],
        "counts": {"accepted": 2},
        "refusals": [{"reason": "evidence_stale", "age_s": 900, "count": 1}] * 7,
    }
    monkeypatch.setattr(watchdog, "fleet_rows", lambda **kw: ([], []))
    monkeypatch.setattr(
        watchdog, "measure_provider_outages",
        lambda rows, *, now_s, **kw: measured,
    )
    monkeypatch.setattr(uw_mod, "report_roots", lambda: [_Path("/w")])
    monkeypatch.setattr(
        uw_mod, "build_report",
        lambda roots, *, now_s, **kw: SimpleNamespace(
            findings=[],
            dimensions=defaultdict(
                lambda: SimpleNamespace(state="unreadable", count=None, warning=None)
            ),
        ),
    )
    monkeypatch.setattr(
        uw_mod, "publish_report",
        lambda snapshot, *, source, now_s, mail_to, log=None: {
            "warnings": [], "counts": {},
        },
    )

    class _Lane:
        broken = False

        def render(self):
            return "keeper lane: ok"

        def to_json(self):
            return {}

    monkeypatch.setattr(keeper_lane_mod, "discover", lambda: _Lane())

    agents_cli._run_unfinished_report(now=0.0, json_out=True, mail_to="")
    payload = _json.loads(capsys.readouterr().out)
    assert payload["provider_outages"] == measured

    agents_cli._run_unfinished_report(now=0.0, json_out=False, mail_to="")
    captured = capsys.readouterr()
    text = captured.out + captured.err
    assert "provider outage open: zai/zai kind=fair_usage_policy reset_at=2.0" in text
    assert "provider-outage refusals (evidence_stale=7)" in text

    def _boom(*_a, **_k):
        raise RuntimeError("no registry")

    monkeypatch.setattr(watchdog, "measure_provider_outages", _boom)
    agents_cli._run_unfinished_report(now=0.0, json_out=True, mail_to="")
    payload = _json.loads(capsys.readouterr().out)
    assert payload["provider_outages"]["instrument"] == "unknown"
    assert (
        payload["provider_outages"]["refusals"][0]["reason"]
        == "provider_outage_measure_failed"
    )
    assert "no registry" in payload["provider_outages"]["refusals"][0]["detail"]
