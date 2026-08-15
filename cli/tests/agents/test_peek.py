"""Tests for ``fno agents peek`` (x-05da) — the read-only twin of mail send.

Grouped by the plan's verify slices:
  peek_resolve       — US1: resolver parity + exit-13 miss (AC1-HP, AC1-ERR)
  peek_reader        — US2: recent_records claude/codex/unsupported (AC2-*)
  peek_tail_follow   — US3: --lines / --json / no-activity / follow (AC1-EDGE/FR/UI)
  peek_status_stream — US4: dual-envelope status fast-path (AC3-EDGE)
"""
from __future__ import annotations

import io
import json
import os
import time
from pathlib import Path

from fno.agents.peek import (
    ObserveUnsupported,
    Record,
    _codex_rollout_path,
    _emit_record,
    _read_complete_lines,
    peek,
    recent_records,
)


class _Session:
    """Minimal DiscoveredSession stand-in (peek reads attrs by getattr)."""

    def __init__(self, agent="claude", session_id="sid-123", short_id="abc12345", cwd="/tmp/proj"):
        self.agent = agent
        self.session_id = session_id
        self.short_id = short_id
        self.cwd = cwd


def _claude_transcript(root: Path, cwd: str, session_id: str, lines: list[str]) -> Path:
    slug = cwd.replace("/", "-").replace(".", "-")
    d = root / slug
    d.mkdir(parents=True, exist_ok=True)
    p = d / f"{session_id}.jsonl"
    p.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return p


def _user(text):
    return json.dumps({"type": "user", "message": {"role": "user", "content": text}})


def _assistant(text):
    return json.dumps(
        {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}
    )


# --------------------------------------------------------------------------
# peek_resolve — US1
# --------------------------------------------------------------------------


def test_peek_resolve_hit_prints_header_and_records(tmp_path):
    """AC1-HP: a resolvable peer shows a header + its recent records."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("hello"), _assistant("hi there")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "worker-x",
        lines=5,
        stdout=out,
        stderr=err,
        resolve=lambda h: (sess, []),
        projects_root=tmp_path,
    )
    assert rc == 0
    body = out.getvalue()
    assert "peer worker-x: agent=claude short_id=abc12345" in body
    assert "user: hello" in body
    assert "assistant: hi there" in body


def test_peek_resolve_miss_exits_13_with_suggestions(tmp_path):
    """AC1-ERR: an unresolvable handle exits 13, parity with mail send."""
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "nope",
        stdout=out,
        stderr=err,
        resolve=lambda h: (None, ["worker-a", "worker-b"]),
        projects_root=tmp_path,
    )
    assert rc == 13
    assert "peer not found: nope" in err.getvalue()
    assert "worker-a" in err.getvalue()


# --------------------------------------------------------------------------
# peek_mux — pane-substrate workers, the default substrate (x-680d)
# --------------------------------------------------------------------------


def test_peek_mux_pane_worker_read_when_live_resolver_misses(tmp_path):
    """A pane worker resolves via the registry mux ref and is read, even though
    the live-session resolver returns None (its content is a PTY, not a transcript)."""
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "worker-x-5f43",
        lines=5,
        stdout=out,
        stderr=err,
        resolve=lambda h: (None, ["board-survey", "gate-b1c3"]),
        projects_root=tmp_path,
        mux_lookup=lambda h: ("mux-sess", 33, "worker-x-5f43"),
        mux_reader=lambda sess, pane, n: (0, "thinking...\nacting now\n"),
    )
    assert rc == 0
    text = out.getvalue()
    assert "mux pane=33" in text
    assert "acting now" in text
    # The misdirecting "did you mean" suggestions must NOT appear for a peer
    # peek can in fact observe.
    assert "did you mean" not in err.getvalue()


def test_peek_mux_pane_refusal_names_working_surface(tmp_path):
    """When the mux cannot answer, the refusal names `fno mux pane read <id>`
    instead of listing unrelated peers (the 2026-08-04 false-liveness incident
    where an empty resolver result was read as proof three live workers were wedged)."""
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "boardsort-b63a",
        lines=5,
        stdout=out,
        stderr=err,
        resolve=lambda h: (None, ["board-survey", "gate-b1c3", "footnote-56"]),
        projects_root=tmp_path,
        mux_lookup=lambda h: ("mux-sess", 25, "boardsort-b63a"),
        mux_reader=lambda sess, pane, n: (1, ""),
    )
    assert rc == 1
    msg = err.getvalue()
    assert "fno mux pane read 25" in msg
    assert "boardsort-b63a" in msg
    # Unrelated peers must not be listed as "did you mean" for a pane worker.
    assert "board-survey" not in msg


def test_peek_mux_pane_no_row_falls_through_to_not_found(tmp_path):
    """A handle with no mux row still gets the normal peer-not-found path."""
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "ghost",
        stdout=out,
        stderr=err,
        resolve=lambda h: (None, ["someone-else"]),
        projects_root=tmp_path,
        mux_lookup=lambda h: None,
        mux_reader=lambda sess, pane, n: (0, "irrelevant"),
    )
    assert rc == 13
    assert "peer not found: ghost" in err.getvalue()
    assert "someone-else" in err.getvalue()


def test_peek_mux_pane_json_shape(tmp_path):
    """--json on a pane worker emits a parseable row carrying the pane identity."""
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "worker-x-5f43",
        lines=3,
        json_out=True,
        stdout=out,
        stderr=err,
        resolve=lambda h: (None, []),
        projects_root=tmp_path,
        mux_lookup=lambda h: ("mux-sess", 33, "worker-x-5f43"),
        mux_reader=lambda sess, pane, n: (0, "hello pane\n"),
    )
    assert rc == 0
    row = json.loads(out.getvalue().strip())
    assert row["pane_id"] == 33
    assert row["mux_session"] == "mux-sess"
    assert "hello pane" in row["text"]


def test_lookup_mux_pane_finds_registry_row(tmp_path):
    """_lookup_mux_pane resolves a registry mux row directly, no live session needed."""
    from fno.agents.peek import _lookup_mux_pane
    from fno.agents.registry import AgentEntry, write_registry

    # A pane worker carries the mux ref and NO transport short_id (the mux ref
    # is its identity) - the one-live-ref invariant mux XOR worker XOR bg.
    entry = AgentEntry(
        name="worker-x-5f43",
        harness="claude",
        cwd=str(tmp_path),
        # x-7bcd: needs a resolvable handle to pass write_registry's guard;
        # log_path stands in for the fallback file mux_spawn.py now touches
        # for an id-less mux row.
        log_path=str(tmp_path / "worker-x-5f43.log"),
        mux={"session": "mux-sess", "pane_id": 33},
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    assert _lookup_mux_pane("worker-x-5f43", registry_path=registry_path) == (
        "mux-sess",
        33,
        "worker-x-5f43",
    )
    # An unknown handle and a non-mux row both resolve to None.
    assert _lookup_mux_pane("nope", registry_path=registry_path) is None

    plain = AgentEntry(
        name="daemon-worker",
        harness="claude",
        harness_session_id="sess-cafe1234",  # x-7bcd: needs a resolvable handle
        cwd=str(tmp_path),
        log_path="",
        short_id="cafe1234",
    )
    write_registry([plain], path=registry_path)
    assert _lookup_mux_pane("daemon-worker", registry_path=registry_path) is None


def test_peek_requested_name_reads_repaired_codex_thread(tmp_path):
    """The requested name reads the rollout, GIVEN an already-repaired row.

    Scope note: this seeds the repaired state and performs no repair, so it does
    NOT prove AC2-CON; it pins the peek-by-name path only. The end-to-end repair
    claim belongs to test_late_codex_identity_composes_across_every_peer_surface.
    """
    from fno.agents import discover
    from fno.agents.registry import AgentEntry, write_registry

    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    requested_name = "repro-late-id"
    registry = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name=requested_name,
                harness="codex",
                harness_session_id=session_id,
                status="live",
                cwd="/repo",
                log_path="",
                mux={"session": "main", "pane_id": 7},
            )
        ],
        path=registry,
    )
    day = tmp_path / "rollouts" / "2026" / "07" / "29"
    day.mkdir(parents=True)
    rollout = day / f"rollout-2026-07-29T16-00-00-{session_id}.jsonl"
    rollout.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "session_meta",
                        "payload": {"id": session_id, "cwd": "/repo"},
                    }
                ),
                json.dumps(
                    {
                        "type": "response_item",
                        "payload": {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "READY"}],
                        },
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    def resolve(handle):
        return discover.resolve_or_suggest(
            handle,
            registry_path=registry,
            sessions_dir=tmp_path / "no-claude-sessions",
            projects_dir=tmp_path / "no-projects",
            codex_sessions_dir=tmp_path / "rollouts",
            opencode_storage_dir=tmp_path / "no-opencode",
            name_map_path=tmp_path / "no-names.json",
            require_alive=False,
            project_resolver=lambda _cwd: None,
        )

    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        requested_name,
        stdout=out,
        stderr=err,
        resolve=resolve,
        codex_sessions_dir=tmp_path / "rollouts",
    )

    assert rc == 0
    assert err.getvalue() == ""
    assert f"peer {requested_name}: agent=codex short_id={session_id[:8]}" in out.getvalue()
    assert "assistant: READY" in out.getvalue()


# --------------------------------------------------------------------------
# peek_reader — US2
# --------------------------------------------------------------------------


def test_peek_reader_claude_tails_and_skips_torn(tmp_path):
    """AC2-HP + AC2-EDGE: claude arm parses records, skips a torn trailing line."""
    sess = _Session()
    lines = [_user("first"), _assistant("second"), '{"type":"assistant","message":{"role":"assist']
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, lines)
    recs = recent_records("claude", sess.session_id, sess.cwd, 10, projects_root=tmp_path)
    assert [r.text for r in recs] == ["first", "second"]


def test_peek_reader_unsupported_raises(tmp_path):
    """AC2-ERR: an agent with no arm raises ObserveUnsupported."""
    try:
        recent_records("gemini", "sid", "/tmp/x", 5)
    except ObserveUnsupported as exc:
        assert exc.agent == "gemini"
    else:
        raise AssertionError("expected ObserveUnsupported")


def test_peek_unsupported_agent_exit_1(tmp_path):
    """AC2-ERR at the command layer: legible message, exit 1 (not 13)."""
    sess = _Session(agent="gemini")
    out, err = io.StringIO(), io.StringIO()
    rc = peek("g", stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 1
    assert "observe not yet supported for gemini" in err.getvalue()


def test_peek_reader_codex(tmp_path, monkeypatch):
    """AC2-HP: codex arm locates the rollout by session_meta id and parses it."""
    day = tmp_path / "2025" / "10" / "16"
    day.mkdir(parents=True)
    rollout = day / "rollout-2025-10-16T00-00-00-cx-999.jsonl"
    rollout.write_text(
        "\n".join(
            [
                json.dumps({"type": "session_meta", "payload": {"id": "cx-999", "cwd": "/tmp/proj"}}),
                json.dumps(
                    {
                        "type": "response_item",
                        "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "ping"}]},
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    recs = recent_records("codex", "cx-999", "/tmp/proj", 10, codex_sessions_dir=tmp_path)
    assert [(r.role, r.text) for r in recs] == [("user", "ping")]


# --------------------------------------------------------------------------
# peek_tail_follow — US3
# --------------------------------------------------------------------------


def test_peek_lines_zero_header_only(tmp_path):
    """Boundary: --lines 0 prints the header and zero records, exit 0."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("hello")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", lines=0, stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    assert "user: hello" not in out.getvalue()
    assert "peer w:" in out.getvalue()


def test_peek_no_activity_yet(tmp_path):
    """AC1-EDGE: resolved but no transcript file → 'no activity yet', exit 0."""
    sess = _Session()  # no transcript written
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    assert "no activity yet" in out.getvalue()


def test_peek_json_emits_rows(tmp_path):
    """--json emits one JSON object per record (AC1-UI, machine-readable)."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_assistant("done")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", json_out=True, stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    rows = [json.loads(line) for line in out.getvalue().splitlines() if line.strip()]
    assert {"role": "assistant", "text": "done"} in rows


def test_peek_json_idle_is_still_json(tmp_path):
    """P2 (gemini): --json on an idle peer emits a JSON status row, not the
    human 'no activity yet' string that would break a JSONL consumer."""
    sess = _Session()  # no transcript
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", json_out=True, stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    rows = [json.loads(line) for line in out.getvalue().splitlines() if line.strip()]
    assert {"status": "no activity yet"} in rows


def test_emit_record_json_vs_human():
    """P2 (gemini): followed records honor --json — the single emit path both
    the initial tail and the follow loop use stays JSON under json_out."""
    out = io.StringIO()
    _emit_record(out, Record(role="assistant", text="hi"), json_out=True)
    assert json.loads(out.getvalue().strip()) == {"role": "assistant", "text": "hi"}
    out = io.StringIO()
    _emit_record(out, Record(role="assistant", text="hi"), json_out=False)
    assert out.getvalue().strip() == "assistant: hi"


def test_peek_follow_exits_when_peer_not_live(tmp_path):
    """AC1-FR: --follow exits (no spin) once is_live() reports the peer gone."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("hi")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "w",
        follow=True,
        stdout=out,
        stderr=err,
        resolve=lambda h: (sess, []),
        projects_root=tmp_path,
        is_live=lambda: False,
    )
    assert rc == 0
    assert "peer ended" in err.getvalue()


# --------------------------------------------------------------------------
# peek_status_stream — US4
# --------------------------------------------------------------------------


def test_follow_reads_only_complete_lines_no_partial_loss(tmp_path):
    """Concurrency (gemini HIGH): a record split across two writes is emitted
    whole on the next poll, not corrupted into two dropped fragments."""
    p = tmp_path / "t.jsonl"
    p.write_bytes(b'{"a":1}\n{"b":2')  # one complete line + a partial
    with p.open("rb") as fh:
        assert _read_complete_lines(fh) == [b'{"a":1}\n']
        pos = fh.tell()  # positioned at the start of the partial
    with p.open("ab") as fh:
        fh.write(b'}\n')  # writer completes the record
    with p.open("rb") as fh:
        fh.seek(pos)
        assert _read_complete_lines(fh) == [b'{"b":2}\n']


def test_codex_rollout_scan_skips_unstatable_file(tmp_path):
    """Pessimist (gemini MEDIUM): a rollout that vanishes mid-scan is skipped,
    it does not abort the whole scan and lose a resolvable session."""
    day = tmp_path / "2025" / "10" / "16"
    day.mkdir(parents=True)
    good = day / "rollout-good.jsonl"
    good.write_text(
        json.dumps({"type": "session_meta", "payload": {"id": "sid", "cwd": "/x"}}) + "\n",
        encoding="utf-8",
    )
    # A dangling symlink named like a rollout: rglob yields it, stat() raises.
    (day / "rollout-dangling.jsonl").symlink_to(day / "does-not-exist.jsonl")
    assert _codex_rollout_path("sid", tmp_path) == good


def test_codex_rollout_exact_filename_avoids_history_meta_reads(
    tmp_path, monkeypatch
):
    from fno.agents import discover

    day = tmp_path / "2026" / "07" / "23"
    day.mkdir(parents=True)
    for index in range(25):
        sid = f"session-{index:02d}"
        (day / f"rollout-2026-07-23T00-00-{index:02d}-{sid}.jsonl").write_text(
            json.dumps({"type": "session_meta", "payload": {"id": sid, "cwd": "/x"}})
            + "\n",
            encoding="utf-8",
        )

    real_meta = discover._codex_meta
    reads = []

    def counting_meta(path):
        reads.append(path)
        return real_meta(path)

    monkeypatch.setattr(discover, "_codex_meta", counting_meta)

    found = _codex_rollout_path("session-00", tmp_path)

    assert found is not None
    assert found.name.endswith("session-00.jsonl")
    assert len(reads) == 1


def test_codex_rollout_exact_filename_prefers_newest_duplicate(tmp_path):
    paths = []
    for year, mtime in (("2025", 1000), ("2026", 2000)):
        day = tmp_path / year / "07" / "23"
        day.mkdir(parents=True)
        path = day / f"rollout-{year}-07-23T00-00-00-duplicate-session.jsonl"
        path.write_text(
            json.dumps({
                "type": "session_meta",
                "payload": {"id": "duplicate-session", "cwd": "/x"},
            })
            + "\n",
            encoding="utf-8",
        )
        os.utime(path, (mtime, mtime))
        paths.append(path)

    assert _codex_rollout_path("duplicate-session", tmp_path) == paths[1]


def test_peek_status_stream_dual_envelope(tmp_path):
    """AC3-EDGE: both {type,data} and {kind,..flat} status events render; a
    neither-shape record is skipped (fast-path stays present, not partial)."""
    sess = _Session(short_id="abc12345", session_id="sid-123")
    events = tmp_path / "events.jsonl"
    events.write_text(
        "\n".join(
            [
                json.dumps({"type": "task_started", "data": {"short_id": "abc12345"}}),
                json.dumps({"kind": "task_done", "short_id": "abc12345"}),
                json.dumps({"type": "unrelated_event", "data": {"short_id": "abc12345"}}),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "w",
        stdout=out,
        stderr=err,
        resolve=lambda h: (sess, []),
        projects_root=tmp_path,
        events_path=events,
    )
    assert rc == 0
    body = out.getvalue()
    assert "[task_started]" in body
    assert "[task_done]" in body
    assert "unrelated_event" not in body


def test_peek_status_absent_falls_through_to_transcript(tmp_path):
    """US4: no status events → fall through to the transcript, no error."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("via transcript")])
    events = tmp_path / "events.jsonl"
    events.write_text(json.dumps({"type": "some_other_event"}) + "\n", encoding="utf-8")
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path, events_path=events)
    assert rc == 0
    assert "user: via transcript" in out.getvalue()


# --------------------------------------------------------------------------
# peek_reader — US6 opencode arm
# --------------------------------------------------------------------------


def _opencode_message(
    storage: Path,
    session_id: str,
    *,
    msg_id: str,
    role: str,
    created: int,
    parts: list[dict] | None,
) -> None:
    """Write one message + its parts into an opencode storage tree.

    Mirrors the real 1.0.223 layout: the message JSON carries NO text (only
    role + ``time.created``); the text lives in ``part/<msg_id>/``.
    """
    mdir = storage / "message" / session_id
    mdir.mkdir(parents=True, exist_ok=True)
    (mdir / f"{msg_id}.json").write_text(
        json.dumps({"id": msg_id, "role": role, "time": {"created": created}}),
        encoding="utf-8",
    )
    if parts is None:
        return
    pdir = storage / "part" / msg_id
    pdir.mkdir(parents=True, exist_ok=True)
    for i, p in enumerate(parts):
        (pdir / f"prt_{i:03d}.json").write_text(json.dumps(p), encoding="utf-8")


def test_peek_reader_opencode_orders_by_created_and_joins_parts(tmp_path):
    """AC-HP: messages render in time.created order with their parts joined,
    even though the filenames sort the other way."""
    storage = tmp_path / "opencode"
    sid = "ses_abc123"
    # Filenames sort z, m, a; time.created orders them a, m, z.
    _opencode_message(
        storage, sid, msg_id="zzz", role="user", created=1,
        parts=[{"type": "text", "text": "first"}],
    )
    _opencode_message(
        storage, sid, msg_id="mmm", role="assistant", created=2,
        parts=[{"type": "text", "text": "second"}, {"type": "text", "text": "half"}],
    )
    _opencode_message(
        storage, sid, msg_id="aaa", role="user", created=3,
        parts=[{"type": "text", "text": "third"}],
    )
    recs = recent_records("opencode", sid, "/tmp/proj", 10, opencode_storage_dir=storage)
    assert [(r.role, r.text) for r in recs] == [
        ("user", "first"),
        ("assistant", "second half"),
        ("user", "third"),
    ]


def test_peek_reader_opencode_part_policy_matches_claude(tmp_path):
    """reasoning/step bookkeeping is observe-noise; a tool part renders a marker,
    matching _extract_text's policy for claude blocks."""
    storage = tmp_path / "opencode"
    sid = "ses_policy"
    _opencode_message(
        storage, sid, msg_id="m1", role="assistant", created=1,
        parts=[
            {"type": "step-start", "snapshot": "abc"},
            {"type": "reasoning", "text": "internal musing"},
            {"type": "text", "text": "here goes"},
            {"type": "tool", "tool": "read"},
            {"type": "step-finish"},
        ],
    )
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [(r.role, r.text) for r in recs] == [
        ("assistant", "here goes [tool_use: read]")
    ]


def test_peek_reader_opencode_tail_is_last_n_chronological(tmp_path):
    """Tail parity with the jsonl arms: last N, still chronological."""
    storage = tmp_path / "opencode"
    sid = "ses_tail"
    for i in range(5):
        _opencode_message(
            storage, sid, msg_id=f"m{i}", role="user", created=i,
            parts=[{"type": "text", "text": f"turn{i}"}],
        )
    recs = recent_records("opencode", sid, "/x", 2, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["turn3", "turn4"]


def test_peek_reader_opencode_unknown_session_returns_empty(tmp_path):
    """AC-ERR: an unknown ses_ id yields the same empty shape as the codex arm
    (resolved-but-nothing-to-show), never a raise."""
    storage = tmp_path / "opencode"
    (storage / "message").mkdir(parents=True)
    assert recent_records(
        "opencode", "ses_nope", "/x", 10, opencode_storage_dir=storage
    ) == []


def test_peek_reader_opencode_empty_or_missing_parts_skipped(tmp_path):
    """AC-EDGE: a message with no part dir, an all-noise part set, or a torn
    part file yields no crash and no empty turn."""
    storage = tmp_path / "opencode"
    sid = "ses_empty"
    _opencode_message(storage, sid, msg_id="nodir", role="user", created=1, parts=None)
    _opencode_message(
        storage, sid, msg_id="noise", role="assistant", created=2,
        parts=[{"type": "reasoning", "text": "hidden"}],
    )
    _opencode_message(
        storage, sid, msg_id="real", role="user", created=3,
        parts=[{"type": "text", "text": "survives"}],
    )
    (storage / "part" / "real" / "torn.json").write_text("{nope", encoding="utf-8")
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [(r.role, r.text) for r in recs] == [("user", "survives")]


def test_peek_opencode_follow_reports_unsupported(tmp_path):
    """--follow on opencode has no tailable file; say so rather than exiting
    silently (the file's no-blank-exit-0 contract)."""
    storage = tmp_path / "opencode"
    sid = "ses_follow"
    _opencode_message(
        storage, sid, msg_id="m1", role="user", created=1,
        parts=[{"type": "text", "text": "hi"}],
    )
    out, err = io.StringIO(), io.StringIO()
    sess = _Session(agent="opencode", session_id=sid)
    rc = peek(
        "h", follow=True, stdout=out, stderr=err,
        resolve=lambda h: (sess, []), opencode_storage_dir=storage,
    )
    assert rc == 0
    assert "hi" in out.getvalue()
    assert "--follow not supported for opencode" in err.getvalue()


def test_peek_reader_opencode_missing_role_degrades_not_dropped(tmp_path):
    """A message whose `role` has not landed yet still renders (as "?"),
    matching the codex arm. Dropping it would hide the peer's latest word."""
    storage = tmp_path / "opencode"
    sid = "ses_norole"
    mdir = storage / "message" / sid
    mdir.mkdir(parents=True)
    (mdir / "m1.json").write_text(
        json.dumps({"id": "m1", "time": {"created": 1}}), encoding="utf-8"
    )
    pdir = storage / "part" / "m1"
    pdir.mkdir(parents=True)
    (pdir / "p.json").write_text(
        json.dumps({"type": "text", "text": "mid-write"}), encoding="utf-8"
    )
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [(r.role, r.text) for r in recs] == [("?", "mid-write")]


def test_peek_reader_opencode_missing_created_falls_back_to_mtime(tmp_path):
    """A message with no time.created must not collapse to a shared sort key.

    Filenames sort the reverse of true order here, so a constant fallback would
    render the transcript backwards while still calling it chronological.
    """
    storage = tmp_path / "opencode"
    sid = "ses_notime"
    mdir = storage / "message" / sid
    mdir.mkdir(parents=True)
    for name, text, age in (("zzz", "older", 100.0), ("aaa", "newer", 10.0)):
        (mdir / f"{name}.json").write_text(
            json.dumps({"id": name, "role": "user"}), encoding="utf-8"
        )
        pdir = storage / "part" / name
        pdir.mkdir(parents=True)
        (pdir / "p.json").write_text(
            json.dumps({"type": "text", "text": text}), encoding="utf-8"
        )
        mt = time.time() - age
        os.utime(mdir / f"{name}.json", (mt, mt))
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["older", "newer"]


def test_peek_reader_opencode_tail_skips_noise_to_fill_n(tmp_path):
    """The bounded walk must still return N *renderable* turns, so noise-only
    messages at the tail do not shrink the result."""
    storage = tmp_path / "opencode"
    sid = "ses_noisetail"
    _opencode_message(
        storage, sid, msg_id="m1", role="user", created=1,
        parts=[{"type": "text", "text": "keep me"}],
    )
    _opencode_message(
        storage, sid, msg_id="m2", role="assistant", created=2,
        parts=[{"type": "reasoning", "text": "noise"}],
    )
    _opencode_message(
        storage, sid, msg_id="m3", role="assistant", created=3,
        parts=[{"type": "text", "text": "last"}],
    )
    recs = recent_records("opencode", sid, "/x", 2, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["keep me", "last"]


def test_peek_follow_unresolved_transcript_is_not_reported_unsupported(tmp_path):
    """A claude transcript that fails to resolve is a resolution miss, not a
    harness-capability limit; the two must not share a message."""
    out, err = io.StringIO(), io.StringIO()
    sess = _Session(agent="claude", session_id="nope-does-not-exist")
    rc = peek(
        "h", follow=True, stdout=out, stderr=err,
        resolve=lambda h: (sess, []), projects_root=tmp_path / "empty",
    )
    assert rc == 0
    assert "not supported" not in err.getvalue()
    assert "could not resolve a transcript" in err.getvalue()


def test_peek_reader_opencode_message_without_id_is_dropped(tmp_path):
    """The counterpart to the role guard: `id` locates the parts, so a message
    missing it must drop rather than build a path from None."""
    storage = tmp_path / "opencode"
    sid = "ses_noid"
    mdir = storage / "message" / sid
    mdir.mkdir(parents=True)
    (mdir / "m1.json").write_text(
        json.dumps({"role": "user", "time": {"created": 1}}), encoding="utf-8"
    )
    _opencode_message(
        storage, sid, msg_id="m2", role="user", created=2,
        parts=[{"type": "text", "text": "survives"}],
    )
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["survives"]


def test_peek_reader_opencode_torn_message_file_skipped(tmp_path):
    """A torn message JSON is skipped like a torn jsonl line, not fatal."""
    storage = tmp_path / "opencode"
    sid = "ses_tornmsg"
    _opencode_message(
        storage, sid, msg_id="ok", role="user", created=1,
        parts=[{"type": "text", "text": "intact"}],
    )
    (storage / "message" / sid / "torn.json").write_text("{nope", encoding="utf-8")
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["intact"]


# --------------------------------------------------------------------------
# peek_reader — opencode SQLite store (current opencode)
# --------------------------------------------------------------------------


def _opencode_db(storage: Path, session_id: str, turns) -> None:
    """Build an opencode.db transcript. `turns` = [(msg_id, role, created, parts)].

    Message and part payloads are the same JSON shapes the legacy files hold,
    which is why the rendering policy is shared between the two readers.
    """
    import sqlite3

    storage.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(storage.parent / "opencode.db")
    con.execute("CREATE TABLE session (id TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER)")
    con.execute("CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT)")
    con.execute("CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT)")
    for mid, role, created, parts in turns:
        con.execute(
            "INSERT INTO message VALUES (?,?,?,?)",
            (mid, session_id, created, json.dumps({"id": mid, "role": role})),
        )
        for i, p in enumerate(parts):
            con.execute(
                "INSERT INTO part VALUES (?,?,?,?,?)",
                (f"prt_{mid}_{i}", mid, session_id, i, json.dumps(p)),
            )
    con.commit()
    con.close()


def test_peek_reader_opencode_db_orders_and_joins(tmp_path):
    """Ordering comes from the time_created column, not a filename or mtime."""
    storage = tmp_path / "opencode" / "storage"
    sid = "ses_db"
    _opencode_db(
        storage,
        sid,
        [
            ("zzz", "user", 1, [{"type": "text", "text": "first"}]),
            ("mmm", "assistant", 2, [
                {"type": "reasoning", "text": "hidden"},
                {"type": "text", "text": "second"},
                {"type": "tool", "tool": "bash"},
            ]),
            ("aaa", "user", 3, [{"type": "text", "text": "third"}]),
        ],
    )
    recs = recent_records("opencode", sid, "/x", 10, opencode_storage_dir=storage)
    assert [(r.role, r.text) for r in recs] == [
        ("user", "first"),
        ("assistant", "second [tool_use: bash]"),
        ("user", "third"),
    ]


def test_peek_reader_opencode_db_tail_is_last_n(tmp_path):
    """Tail parity: last N, still chronological."""
    storage = tmp_path / "opencode" / "storage"
    sid = "ses_dbtail"
    _opencode_db(
        storage, sid,
        [(f"m{i}", "user", i, [{"type": "text", "text": f"turn{i}"}]) for i in range(5)],
    )
    recs = recent_records("opencode", sid, "/x", 2, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["turn3", "turn4"]


def test_peek_reader_opencode_db_noise_only_turn_skipped(tmp_path):
    """A turn whose parts are all observe-noise renders nothing, so the tail
    still fills to N with real turns."""
    storage = tmp_path / "opencode" / "storage"
    sid = "ses_dbnoise"
    _opencode_db(
        storage, sid,
        [
            ("m1", "user", 1, [{"type": "text", "text": "keep"}]),
            ("m2", "assistant", 2, [{"type": "step-start"}, {"type": "reasoning", "text": "x"}]),
            ("m3", "assistant", 3, [{"type": "text", "text": "last"}]),
        ],
    )
    recs = recent_records("opencode", sid, "/x", 2, opencode_storage_dir=storage)
    assert [r.text for r in recs] == ["keep", "last"]


def test_peek_reader_opencode_db_unknown_session_empty(tmp_path):
    """An unknown ses_ id yields the resolved-but-nothing-to-show shape."""
    storage = tmp_path / "opencode" / "storage"
    _opencode_db(storage, "ses_other", [("m1", "user", 1, [{"type": "text", "text": "hi"}])])
    assert recent_records(
        "opencode", "ses_nope", "/x", 10, opencode_storage_dir=storage
    ) == []


def test_peek_opencode_idle_db_session_does_not_serve_legacy_transcript(tmp_path):
    """A session with no messages yet reads empty from the database. Falling
    back would serve the legacy tree's stale transcript for that same id as if
    it were current."""
    storage = tmp_path / "opencode" / "storage"
    sid = "ses_both"
    _opencode_message(
        storage, sid, msg_id="old", role="user", created=1,
        parts=[{"type": "text", "text": "six months stale"}],
    )
    _opencode_db(storage, "ses_other", [("m1", "user", 1, [{"type": "text", "text": "x"}])])
    assert recent_records(
        "opencode", sid, "/x", 10, opencode_storage_dir=storage
    ) == []


def test_peek_opencode_db_ties_render_deterministically(tmp_path):
    """Identical millisecond timestamps occur in a real store, so ordering must
    not be left to the query planner."""
    storage = tmp_path / "opencode" / "storage"
    sid = "ses_ties"
    _opencode_db(
        storage, sid,
        [
            ("m_a", "user", 5, [{"type": "text", "text": "a"}]),
            ("m_b", "user", 5, [{"type": "text", "text": "b"}]),
            ("m_c", "user", 5, [{"type": "text", "text": "c"}]),
        ],
    )
    seen = {
        tuple(
            r.text
            for r in recent_records(
                "opencode", sid, "/x", 10, opencode_storage_dir=storage
            )
        )
        for _ in range(5)
    }
    assert len(seen) == 1, f"tie ordering not deterministic: {seen}"
    assert seen == {("a", "b", "c")}
