"""Group 3 (x-a2c9 / US5): the relay envelope + provenance wire format."""
from __future__ import annotations

from fno.relay import envelope as env


# ---- wire format: frame / parse round-trip ---------------------------------

def test_frame_produces_single_line_attribute_tag():
    line = env.frame("sid-abc", "claude", "opus", "hello there")
    assert line == '<fno from="sid-abc" provider="claude" model="opus"> hello there'
    assert "\n" not in line  # one physical line (Enter submits the TUI turn)


def test_frame_collapses_multiline_body():
    line = env.frame("A", "claude", None, "line one\nline two\t  three")
    assert line == '<fno from="A" provider="claude"> line one line two three'


def test_parse_round_trips_frame():
    line = env.frame("uuid-with-dashes-1234", "codex", "gpt", "the body")
    got = env.parse(line)
    assert got == {
        "from_session": "uuid-with-dashes-1234",
        "provider": "codex",
        "model": "gpt",
        "body": "the body",
    }


def test_parse_model_optional():
    got = env.parse('<fno from="A" provider="claude"> hi')
    assert got is not None and got["model"] is None and got["body"] == "hi"


def test_parse_unframed_is_none():
    assert env.parse("just a raw human message") is None
    assert env.parse("") is None
    assert not env.is_framed("RELAY from peer alice: hi")  # G1's prose form is NOT the tag


# ---- hop_count / ttl over the bus meta -------------------------------------

def test_hop_and_ttl_defaults_and_meta():
    e = env.make_relay_envelope(from_session="A", to="B", body="x", provider_from="claude")
    assert env.hop_count(e) == 0
    assert env.ttl(e) == env.DEFAULT_TTL

    e2 = env.make_relay_envelope(from_session="A", to="B", body="x",
                                 provider_from="claude", hop_count=3, ttl=5)
    assert env.hop_count(e2) == 3 and env.ttl(e2) == 5


def test_meta_junk_degrades_to_default():
    e = env.make_relay_envelope(from_session="A", to="B", body="x", provider_from="claude")
    e.meta[env.META_HOP] = "not-an-int"
    assert env.hop_count(e) == 0  # never raises on a junk meta value


# ---- frame_envelope: the unframeable signal --------------------------------

def test_frame_envelope_uses_provenance_fields():
    e = env.make_relay_envelope(from_session="A", to="B", body="ping",
                                provider_from="claude", from_model="opus")
    assert env.frame_envelope(e) == '<fno from="A" provider="claude" model="opus"> ping'


def test_frame_envelope_none_when_provenance_missing():
    # No provider_from -> cannot frame -> None (the AC5-FR refusal signal).
    from fno.bus.log import Envelope
    bare = Envelope.new(from_="A", to="B", kind="relay", body="x", from_session="A")
    assert env.frame_envelope(bare) is None
