"""x-f76e: spawn argv normalization (autogen name, -r/--resume widening,
positional substrate token). Pure argv -> argv, covered against the plan ACs."""
from __future__ import annotations

import io
import random

import pytest

from fno.agents.spawn_defaults import normalize_spawn_args

UUID = "6501096a-1111-2222-3333-444455556666"
SHORT = "6501096a"


def _norm(args, **kw):
    kw.setdefault("existing_names", set())
    kw.setdefault("resolver", lambda s: UUID if s == SHORT else None)
    kw.setdefault("rng", random.Random(0))
    kw.setdefault("stderr", io.StringIO())
    return normalize_spawn_args(args, **kw)


# --- Pass-through / non-spawn -------------------------------------------------

def test_non_spawn_verb_untouched():
    assert _norm(["ask", "w", "hi"]) == ["ask", "w", "hi"]


def test_help_untouched():
    assert _norm(["spawn", "--help"]) == ["spawn", "--help"]


def test_ac1_edge_explicit_passthrough_is_byte_identical():
    argv = ["spawn", "myname", "--resume", UUID, "--substrate", "bg"]
    assert _norm(argv) == argv


# --- AC1-HP / AC2-EDGE: autogen name -----------------------------------------

def test_ac1_hp_nameless_spawn_mints_slug():
    out = _norm(["spawn"])
    assert out[0] == "spawn"
    assert len(out) == 2 and "-" in out[1]  # adjective-noun slug as NAME


def test_ac2_edge_lone_bg_positional_is_substrate_and_name_autogens():
    out = _norm(["spawn", "bg"])
    assert out[:1] == ["spawn"]
    assert out[-2:] == ["--substrate", "bg"]
    # a NAME slug was inserted, not "bg"
    assert out[1] not in ("bg", "--substrate")
    assert "-" in out[1]


def test_autogen_avoids_registry_collision():
    rng = random.Random(0)
    first = normalize_spawn_args(["spawn"], existing_names=set(), rng=random.Random(0))[1]
    # Seed the same rng path but mark the first pick taken -> must differ.
    out = normalize_spawn_args(["spawn"], existing_names={first}, rng=rng)
    assert out[1] != first


def test_autogen_exhaustion_exits_2():
    # Every possible slug taken -> bounded exit 2, never an infinite loop.
    from fno.agents import spawn_defaults as sd

    everything = {f"{a}-{n}" for a in sd._SLUG_ADJ for n in sd._SLUG_NOUN}
    err = io.StringIO()
    with pytest.raises(SystemExit) as e:
        normalize_spawn_args(
            ["spawn"], existing_names=everything, rng=random.Random(1), stderr=err
        )
    assert e.value.code == 2
    assert "free auto-name" in err.getvalue()


# --- AC2-HP / AC3-HP / AC3-EDGE: -r widening ---------------------------------

def test_ac2_hp_short_form_revival_normalizes_fully():
    out = _norm(["spawn", "foo", "-r", SHORT, "bg"])
    assert out == ["spawn", "foo", "--resume", UUID, "--substrate", "bg"]


def test_ac3_hp_resume_implies_bg():
    out = _norm(["spawn", "foo", "-r", UUID])
    assert out == ["spawn", "foo", "--resume", UUID, "--substrate", "bg"]


def test_implied_bg_is_announced_on_stderr():
    err = io.StringIO()
    _norm(["spawn", "foo", "-r", UUID], stderr=err)
    assert "implied by --resume" in err.getvalue()


def test_resume_does_not_override_explicit_substrate():
    out = _norm(["spawn", "foo", "-r", UUID, "--substrate", "headless"])
    # implied-bg must not fire when a substrate is already pinned
    assert out.count("--substrate") == 1
    assert "bg" not in out


def test_ac3_edge_uppercase_uuid_tolerated():
    upper = UUID.upper()
    out = _norm(["spawn", "foo", "--resume", upper])
    assert out == ["spawn", "foo", "--resume", UUID, "--substrate", "bg"]


# --- AC1-ERR / AC2-ERR / refusals --------------------------------------------

def test_ac1_err_unresolvable_short_id_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "foo", "-r", "deadbeef"], resolver=lambda s: None)
    assert e.value.code == 2


def test_ac2_err_malformed_resume_value_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "foo", "-r", "not-an-id"])
    assert e.value.code == 2


def test_resume_at_argv_end_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "foo", "-r"])
    assert e.value.code == 2


def test_resume_given_twice_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "foo", "-r", UUID, "--resume", UUID])
    assert e.value.code == 2


def test_substrate_given_twice_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "foo", "--substrate", "pane", "bg"])
    assert e.value.code == 2


def test_headless_flag_plus_trailing_substrate_token_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "foo", "-H", "bg"])
    assert e.value.code == 2


# --- non-substrate trailing positional stays a message ------------------------

def test_ordinary_message_positional_is_not_a_substrate():
    out = _norm(["spawn", "foo", "hello"])
    assert out == ["spawn", "foo", "hello"]  # untouched; hello is the message


def test_message_option_value_is_not_a_substrate_token():
    # `--message` is a Rust-path value flag; its value must not be misread as a
    # trailing substrate token (codex P2).
    argv = ["spawn", "foo", "--message", "bg"]
    assert _norm(argv) == argv


def test_message_option_value_does_not_block_autogen_name():
    out = _norm(["spawn", "--message", "hello", "--substrate", "bg"])
    assert out[1] not in ("--message", "--substrate", "hello", "bg")  # slug minted
    assert "--message" in out and "hello" in out


# --- --argv payload boundary (codex finding) ---------------------------------

def test_argv_payload_resume_flag_is_not_scanned():
    # A --resume inside the provider payload must pass through untouched: it is
    # the child command's flag, not an fno resume request (no --substrate bg added).
    argv = ["spawn", "w", "--argv", "--", "tool", "--resume", "deadbeef"]
    assert _norm(argv) == argv


def test_argv_payload_substrate_word_is_not_a_substrate_token():
    argv = ["spawn", "w", "--argv", "--", "tool", "bg"]
    assert _norm(argv) == argv


def test_nameless_spawn_with_payload_autogens_before_payload():
    out = _norm(["spawn", "--argv", "--", "tool", "run"])
    assert out[1] not in ("--argv",) and "-" in out[1]  # slug inserted as NAME
    assert out[2:] == ["--argv", "--", "tool", "run"]  # payload untouched, after name
