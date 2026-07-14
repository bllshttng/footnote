"""Group 1 (ab-ba91b807) - addressed envelope on the global bus (Option A, cv-d54ddd45).

Covers AC2-HP / AC2-ERR for the envelope layer: the bus envelope additively
carries the sender's session (for sender-exclusion), the sender's model (for the
render), and a ``to_kind`` addressing discriminator (name|session|project), all
optional and omitted when unset so pre-existing lines serialize byte-identically
and old lines still parse (LD11 additive read).
"""
from __future__ import annotations

import json

import pytest

from fno.bus.log import Envelope, from_json_line, to_json_line
from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# AC2-HP: enriched fields round-trip
# ---------------------------------------------------------------------------

def test_addressed_fields_round_trip():
    env = Envelope.new(
        from_="alice",
        to="bob",
        kind="send",
        body="hi",
        from_session="sess-A",
        from_model="opus-4-8",
        to_kind="name",
        provider_from="claude",
        provider_to="claude",
    )
    line = to_json_line(env)
    back = from_json_line(line)
    assert back.from_session == "sess-A"
    assert back.from_model == "opus-4-8"
    assert back.to_kind == "name"
    assert back.from_ == "alice"
    assert back.to == "bob"


def test_to_kind_project_round_trips():
    env = Envelope.new(from_="a", to="fno", kind="send", body="x", to_kind="project")
    assert from_json_line(to_json_line(env)).to_kind == "project"


# ---------------------------------------------------------------------------
# AC2-ERR / additive: unset fields are omitted; old lines still parse
# ---------------------------------------------------------------------------

def test_unset_addressed_fields_are_omitted_from_line():
    env = Envelope.new(from_="a", to="b", kind="send", body="x")
    obj = json.loads(to_json_line(env))
    assert "from_session" not in obj
    assert "from_model" not in obj
    assert "to_kind" not in obj


def test_old_line_without_new_fields_parses():
    # A line written before this change (no from_session/from_model/to_kind).
    old = json.dumps(
        {"v": 1, "id": "msg-abc", "ts": "2026-01-01T00:00:00Z", "thread": "msg-abc",
         "from": "a", "to": "b", "kind": "send", "body": "legacy"}
    )
    env = from_json_line(old)
    assert env.from_session is None
    assert env.from_model is None
    assert env.to_kind is None
    assert env.body == "legacy"


def test_unset_fields_serialize_byte_identical_to_pre_change():
    # The pre-change serialization for a plain envelope must be unchanged so
    # existing readers/tests and on-disk lines are unaffected.
    env = Envelope(
        id="msg-1", thread="msg-1", from_="a", to="b", kind="send", body="x",
        ts="2026-01-01T00:00:00Z",
    )
    assert to_json_line(env) == (
        '{"v":1,"id":"msg-1","ts":"2026-01-01T00:00:00Z","thread":"msg-1",'
        '"from":"a","to":"b","kind":"send","body":"x"}'
    )


# ---------------------------------------------------------------------------
# AC2-HP: `fno agents send <name>` writes an addressed (to_kind=name) envelope
# carrying the sender's session, so the read side can exclude the sender.
# ---------------------------------------------------------------------------

@pytest.fixture
def env(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "agents"))
    return tmp_path


def _project_cwd(tmp_path, project):
    d = tmp_path / project
    (d / ".fno").mkdir(parents=True, exist_ok=True)
    (d / ".fno" / "settings.yaml").write_text(f"project: {project}\n", encoding="utf-8")
    return d


def _register(name, project_cwd, *, status="live"):
    from fno.agents.registry import AgentEntry, load_registry, write_registry
    try:
        existing = list(load_registry())
    except Exception:
        existing = []
    existing.append(
        AgentEntry(
            name=name, provider="claude", cwd=str(project_cwd),
            log_path=f"/tmp/{name}.log", short_id=f"id-{name}", status=status,
        )
    )
    write_registry(existing)


def test_dispatch_send_writes_addressed_name_envelope(env, tmp_path):
    from fno.agents.dispatch import dispatch_send
    from fno.bus.log import iter_messages

    cwd = _project_cwd(tmp_path, "projA")
    _register("bob", cwd, status="live")     # recipient
    _register("alice", cwd, status="live")   # sender

    dispatch_send(
        name="bob", message="rebase first", provider=None,
        cwd=cwd, from_name="alice",
    )

    addressed = [m for m in iter_messages() if m.to == "bob"]
    assert len(addressed) == 1
    env_ = addressed[0]
    assert env_.to_kind == "name"
    assert env_.from_ == "alice"
    # sender session recorded (best-effort) so a project-broadcast read can exclude it
    assert env_.from_session == "id-alice"
    # The durable bus body is <fno_mail>-wrapped now (node x-1f23): the same
    # envelope the live path injects, so grep <fno_mail> finds durable mail too.
    assert env_.body.startswith("<fno_mail "), env_.body[:40]
    assert env_.body.rstrip().endswith("</fno_mail>")
    assert "rebase first" in env_.body


def test_dispatch_send_to_project_sets_to_kind_project(env, tmp_path):
    from fno.agents.dispatch import dispatch_send_to_project
    from fno.bus.log import iter_messages

    _project_cwd(tmp_path, "projA")  # no live peer -> durable, addressed to project

    dispatch_send_to_project("projA", "broadcast body", cwd=tmp_path, from_name="alice")

    addressed = [m for m in iter_messages() if m.to == "projA"]
    assert len(addressed) == 1
    assert addressed[0].to_kind == "project"


def test_bus_log_is_owner_only(env, tmp_path):
    # Privacy hardening (Option A): the single global bus holds message bodies,
    # so the log file must not be group/other readable.
    import os
    from fno.bus.log import Envelope, append, bus_log_path

    append(Envelope.new(from_="a", to="b", kind="send", body="secret-ish"))
    mode = os.stat(bus_log_path()).st_mode
    assert mode & 0o077 == 0, oct(mode)
