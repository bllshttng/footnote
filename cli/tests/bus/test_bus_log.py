"""Task 3.1 - canonical JSONL bus log substrate (US5).

Covers AC5-HP (ordered thread scan, correlation ids, provider mix),
AC5-ERR (corrupt line skipped with warning), AC5-EDGE (rotation boundary),
plus the locked write discipline (flock sidecar + O_APPEND whole-line writes
at any body size) and the versioned envelope.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


@pytest.fixture
def bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    return paths.bus_dir()


# ---------------------------------------------------------------------------
# Envelope shape + serialization
# ---------------------------------------------------------------------------

def test_envelope_round_trips_with_version_and_from_key(bus):
    from fno.bus.log import Envelope, to_json_line, from_json_line

    env = Envelope.new(
        from_="alice", to="bob", kind="send", body="hi there",
        provider_from="claude", provider_to="codex",
    )
    line = to_json_line(env)
    obj = json.loads(line)
    # Envelope is versioned and addresses via the canonical 'from'/'to' keys.
    assert obj["v"] == 1
    assert obj["from"] == "alice"
    assert obj["to"] == "bob"
    assert obj["kind"] == "send"
    assert obj["body"] == "hi there"
    assert obj["id"].startswith("msg-")
    # Round-trips back to an equal envelope.
    back = from_json_line(line)
    assert back.from_ == "alice"
    assert back.to == "bob"
    assert back.id == env.id
    assert back.body == "hi there"


def test_append_then_iter_returns_in_order(bus):
    from fno.bus.log import Envelope, append, iter_messages

    for i in range(3):
        append(Envelope.new(from_="a", to="b", kind="send", body=f"m{i}"))
    bodies = [e.body for e in iter_messages()]
    assert bodies == ["m0", "m1", "m2"]


# ---------------------------------------------------------------------------
# AC5-HP: ordered thread scan, correlation intact, provider mix
# ---------------------------------------------------------------------------

def test_ac5_hp_thread_scan_in_order_with_correlation(bus):
    from fno.bus.log import Envelope, append, iter_thread

    ask = Envelope.new(
        from_="a", to="b", kind="ask", body="status?",
        provider_from="claude", provider_to="codex", request_id="rq-1",
    )
    append(ask)
    reply = Envelope.new(
        from_="b", to="a", kind="reply", body="all good",
        provider_from="codex", provider_to="claude", in_reply_to="rq-1",
        thread=ask.thread,
    )
    append(reply)
    send = Envelope.new(
        from_="a", to="b", kind="send", body="thanks",
        provider_from="claude", provider_to="codex", thread=ask.thread,
    )
    append(send)

    convo = list(iter_thread(ask.thread))
    assert [m.kind for m in convo] == ["ask", "reply", "send"]
    assert convo[1].in_reply_to == "rq-1"
    # provider mix preserved
    assert convo[0].provider_to == "codex"
    assert convo[1].provider_from == "codex"


# ---------------------------------------------------------------------------
# AC5-ERR: corrupt line skipped with a warning, valid lines still read
# ---------------------------------------------------------------------------

def test_ac5_err_corrupt_line_skipped_with_warning(bus, capsys):
    from fno.bus.log import Envelope, append, iter_messages, bus_log_path

    append(Envelope.new(from_="a", to="b", kind="send", body="good1"))
    # Inject a corrupt line in the middle.
    with bus_log_path().open("a", encoding="utf-8") as f:
        f.write("this is not json{{{\n")
    append(Envelope.new(from_="a", to="b", kind="send", body="good2"))

    msgs = list(iter_messages())
    bodies = [m.body for m in msgs]
    assert bodies == ["good1", "good2"]
    err = capsys.readouterr().err
    assert "skip" in err.lower() or "corrupt" in err.lower() or "malformed" in err.lower()


# ---------------------------------------------------------------------------
# Write discipline: large body lands whole (no truncation/interleave)
# ---------------------------------------------------------------------------

def test_type_mismatched_line_skipped_not_crashing(bus, capsys):
    # A line with a null `v` makes from_json_line raise TypeError (int(None)),
    # which must be skipped like any other malformed line, not crash the reader.
    from fno.bus.log import Envelope, append, iter_messages, bus_log_path

    append(Envelope.new(from_="a", to="b", kind="send", body="ok1"))
    with bus_log_path().open("a", encoding="utf-8") as f:
        f.write('{"v":null,"id":"msg-bad","from":"a","to":"b","kind":"send","body":"x"}\n')
    append(Envelope.new(from_="a", to="b", kind="send", body="ok2"))

    bodies = [m.body for m in iter_messages()]
    assert bodies == ["ok1", "ok2"]


def test_large_body_lands_as_one_line(bus):
    from fno.bus.log import Envelope, append, iter_messages

    big = "x" * (256 * 1024)  # 256KB, well past the macOS small-write threshold
    append(Envelope.new(from_="a", to="b", kind="send", body=big))
    msgs = list(iter_messages())
    assert len(msgs) == 1
    assert msgs[0].body == big


def test_concurrent_appends_all_land_unintercalated(bus):
    import threading
    from fno.bus.log import Envelope, append, iter_messages

    big = "y" * (64 * 1024)
    n = 12

    def worker(i):
        append(Envelope.new(from_="a", to="b", kind="send", body=f"{i}:{big}"))

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(n)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    msgs = list(iter_messages())
    assert len(msgs) == n
    # Every body is intact (prefix + full big chunk), none truncated or merged.
    for m in msgs:
        idx, _, payload = m.body.partition(":")
        assert payload == big, f"body for {idx} was corrupted/interleaved"
    assert sorted(int(m.body.split(":", 1)[0]) for m in msgs) == list(range(n))


# ---------------------------------------------------------------------------
# AC5-EDGE: rotation boundary - messages in a rotated segment still read
# ---------------------------------------------------------------------------

def test_ac5_edge_rotation_preserves_read_across_segments(bus, monkeypatch):
    # Force a tiny rotation threshold so a few messages roll a segment.
    monkeypatch.setenv("FNO_BUS_MAX_BYTES", "300")
    from fno.bus.log import Envelope, append, iter_messages, bus_log_path

    for i in range(8):
        append(Envelope.new(from_="a", to="b", kind="send", body=f"msg-body-{i}"))

    # A rotation must have happened (a .1 segment exists).
    seg1 = Path(str(bus_log_path()) + ".1")
    assert seg1.exists(), "expected at least one rotated segment"

    bodies = [m.body for m in iter_messages()]
    assert bodies == [f"msg-body-{i}" for i in range(8)], "rotation lost ordering/messages"
