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
    argv = ["spawn", "--name", "myname", "--resume", UUID, "--substrate", "bg"]
    assert _norm(argv) == argv


# --- AC1-HP / AC2-EDGE: autogen name -----------------------------------------

def test_ac1_hp_nameless_spawn_mints_slug():
    out = _norm(["spawn"])
    assert out == ["spawn", "--name", out[2]]
    assert "-" in out[2]  # adjective-noun slug


def test_ac2_edge_lone_bg_positional_is_substrate_and_name_autogens():
    out = _norm(["spawn", "bg"])
    assert out[:2] == ["spawn", "--name"]
    assert out[-2:] == ["--substrate", "bg"]
    # a NAME slug was minted, not the substrate word
    assert out[2] not in ("bg", "--substrate")
    assert "-" in out[2]


def test_workspace_value_flag_not_misread_as_name_or_substrate():
    # Codex P2 (x-8317 US2): --workspace is a value flag, so its value is never
    # the agent name (a nameless spawn still mints a slug), and a substrate-shaped
    # workspace name is not rewritten into --substrate.
    out = _norm(["spawn", "--workspace", "review"])
    assert out[0] == "spawn"
    assert "-" in out[2] and out[2] not in ("--workspace", "review")  # slug minted
    i = out.index("--workspace")
    assert out[i + 1] == "review"  # value preserved, not consumed as the name

    out2 = _norm(["spawn", "--workspace", "bg"])
    assert "--substrate" not in out2  # not rewritten to a substrate token
    j = out2.index("--workspace")
    assert out2[j + 1] == "bg"
    assert "-" in out2[2] and out2[2] not in ("--workspace", "bg")


def test_autogen_avoids_registry_collision():
    rng = random.Random(0)
    first = normalize_spawn_args(["spawn"], existing_names=set(), rng=random.Random(0))[2]
    # Seed the same rng path but mark the first pick taken -> must differ.
    out = normalize_spawn_args(["spawn"], existing_names={first}, rng=rng)
    assert out[2] != first


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
    out = _norm(["spawn", "--name", "foo", "-r", SHORT, "bg"])
    assert out == ["spawn", "--name", "foo", "--resume", UUID, "--substrate", "bg"]


def test_ac3_hp_resume_implies_bg():
    out = _norm(["spawn", "--name", "foo", "-r", UUID])
    assert out == ["spawn", "--name", "foo", "--resume", UUID, "--substrate", "bg"]


def test_implied_bg_is_announced_on_stderr():
    err = io.StringIO()
    _norm(["spawn", "--name", "foo", "-r", UUID], stderr=err)
    assert "implied by --resume" in err.getvalue()


def test_resume_does_not_override_explicit_substrate():
    out = _norm(["spawn", "--name", "foo", "-r", UUID, "--substrate", "headless"])
    # implied-bg must not fire when a substrate is already pinned
    assert out.count("--substrate") == 1
    assert "bg" not in out


def test_ac3_edge_uppercase_uuid_tolerated():
    upper = UUID.upper()
    out = _norm(["spawn", "--name", "foo", "--resume", upper])
    assert out == ["spawn", "--name", "foo", "--resume", UUID, "--substrate", "bg"]


# --- AC1-ERR / AC2-ERR / refusals --------------------------------------------

def test_ac1_err_unresolvable_short_id_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "--name", "foo", "-r", "deadbeef"], resolver=lambda s: None)
    assert e.value.code == 2


def test_ac2_err_malformed_resume_value_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "--name", "foo", "-r", "not-an-id"])
    assert e.value.code == 2


def test_resume_at_argv_end_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "--name", "foo", "-r"])
    assert e.value.code == 2


def test_resume_given_twice_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "--name", "foo", "-r", UUID, "--resume", UUID])
    assert e.value.code == 2


def test_substrate_given_twice_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "--name", "foo", "--substrate", "pane", "bg"])
    assert e.value.code == 2


def test_headless_flag_plus_trailing_substrate_token_exits_2():
    with pytest.raises(SystemExit) as e:
        _norm(["spawn", "--name", "foo", "--headless", "bg"])
    assert e.value.code == 2


def test_harness_short_H_consumes_value_not_a_substrate_pin():
    # x-6de8: -H is now --harness (a value flag), so `-H codex` consumes `codex`
    # and does NOT pin headless; the trailing `bg` is then the substrate word.
    out = _norm(["spawn", "--name", "foo", "-H", "codex", "bg"])
    assert out == ["spawn", "--name", "foo", "-H", "codex", "--substrate", "bg"]


# --- non-substrate trailing positional stays a message ------------------------

def test_ordinary_message_positional_is_not_a_substrate():
    out = _norm(["spawn", "--name", "foo", "hello"])
    assert out == ["spawn", "--name", "foo", "hello"]  # untouched; hello is the message


def test_message_option_value_is_not_a_substrate_token():
    # `--message` is a Rust-path value flag; its value must not be misread as a
    # trailing substrate token (codex P2).
    argv = ["spawn", "--name", "foo", "--message", "bg"]
    assert _norm(argv) == argv


def test_message_option_value_does_not_block_autogen_name():
    out = _norm(["spawn", "--message", "hello", "--substrate", "bg"])
    assert out[1] not in ("--message", "--substrate", "hello", "bg")  # slug minted
    assert "--message" in out and "hello" in out


# --- --argv payload boundary (codex finding) ---------------------------------

def test_argv_payload_resume_flag_is_not_scanned():
    # A --resume inside the provider payload must pass through untouched: it is
    # the child command's flag, not an fno resume request (no --substrate bg added).
    argv = ["spawn", "--name", "w", "--argv", "--", "tool", "--resume", "deadbeef"]
    assert _norm(argv) == argv


def test_argv_payload_substrate_word_is_not_a_substrate_token():
    argv = ["spawn", "--name", "w", "--argv", "--", "tool", "bg"]
    assert _norm(argv) == argv


def test_nameless_spawn_with_payload_autogens_before_payload():
    out = _norm(["spawn", "--argv", "--", "tool", "run"])
    assert out[1] == "--name" and "-" in out[2]  # slug minted
    assert out[3:] == ["--argv", "--", "tool", "run"]  # payload untouched, after name


# --- x-6de8: the name axis moved off the positional --------------------------

def test_single_positional_is_the_message_and_the_name_is_minted():
    # The prompt is what a caller actually has; the agent name is a handle they
    # rarely pick. So one positional means the MESSAGE, with a slug minted.
    out = _norm(["spawn", "/target x-1234"])
    assert out[1] == "--name" and "-" in out[2]
    assert out[3] == "/target x-1234"


def test_explicit_name_flag_suppresses_minting():
    out = _norm(["spawn", "--name", "worker", "/target x-1234"])
    assert out == ["spawn", "--name", "worker", "/target x-1234"]


def test_two_positionals_are_refused_not_guessed():
    # The old grammar was `spawn <name> <message>`. Accepting two positionals
    # would silently register an agent named after the prompt, so refuse and
    # name the fix instead.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _norm(["spawn", "worker", "/target x-1234"], stderr=err)
    assert exc.value.code == 2
    assert "one positional" in err.getvalue()
    assert "--name" in err.getvalue()


def test_trailing_substrate_word_still_leaves_one_positional():
    # `spawn "<prompt>" bg` is the canonical shape: pass 1 lifts the substrate
    # word off the tail, so the prompt is the only positional left.
    out = _norm(["spawn", "review this diff", "bg"])
    assert out[1] == "--name" and "-" in out[2]
    assert out[3] == "review this diff"
    assert out[4:] == ["--substrate", "bg"]


def test_value_flag_values_are_never_counted_as_positionals():
    # A missing entry in the value-flag table made a flag's VALUE look like a
    # positional: `--route zai,glm-5.2` suppressed name minting (x-6de8), and
    # under this grammar it would trip the two-positional refusal instead.
    for flag, value in (
        ("--route", "zai,glm-5.2"),
        ("--account", "readyrule"),
        ("--crown", "level=1,scope=x-1234"),
    ):
        out = _norm(["spawn", flag, value, "do the thing"])
        assert out[1] == "--name", f"{flag}: no name minted"
        assert out[3:] == [flag, value, "do the thing"], f"{flag}: argv mangled"


def test_spawn_value_flags_cover_every_cmd_spawn_value_option():
    """The normalizer's value-flag table is a hand-maintained mirror of
    cmd_spawn's option list; a missing entry silently turns that flag's value
    into a positional. Pin the two together so adding an option to cmd_spawn
    without teaching the scanner fails here instead of in the field."""
    import ast
    import inspect

    from fno.agents import cli as agents_cli
    from fno.agents.spawn_defaults import _SPAWN_VALUE_FLAGS

    tree = ast.parse(inspect.getsource(agents_cli.cmd_spawn.__wrapped__
                                       if hasattr(agents_cli.cmd_spawn, "__wrapped__")
                                       else agents_cli.cmd_spawn))
    fn = tree.body[0]
    missing = []
    defaults = fn.args.defaults
    for arg, default in zip(fn.args.args[len(fn.args.args) - len(defaults):], defaults):
        if not isinstance(default, ast.Call) or getattr(default.func, "attr", "") != "Option":
            continue
        annotation = ast.unparse(arg.annotation) if arg.annotation else ""
        if annotation.strip() == "bool":
            continue  # boolean flags consume no value
        for spelling in (e.value for e in default.args[1:] if isinstance(e, ast.Constant)):
            if spelling not in _SPAWN_VALUE_FLAGS:
                missing.append(f"{arg.arg} ({spelling})")
    assert not missing, (
        "cmd_spawn value options absent from _SPAWN_VALUE_FLAGS (their values "
        f"would be read as positionals): {', '.join(missing)}"
    )
