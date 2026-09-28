"""Golden receipts: blueprint session open / close (the lease lifecycle)."""
from __future__ import annotations

from tests.goldens._door import SESSION_ID, door, make_sandbox, seed_node, warm


def test_session_open_holds_the_node_and_prints_the_holder(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "design")])
    warm(root, "x-ccc33000")
    code, out, err = door(root, ["session", "open", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID])
    assert code == 0, err
    assert out.startswith("opened x-ccc33000 holder=blueprint-session:"), out
    # The holder names the acting session, so only its shape is stable.
    assert SESSION_ID in out


def test_session_open_twice_for_the_same_session_refuses(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "design")])
    warm(root, "x-ccc33000")
    door(root, ["session", "open", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID])
    code, out, err = door(root, ["session", "open", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID])
    assert code == 1, err
    assert out == "", out
    assert err.startswith(
        "session open: node:x-ccc33000 is already open for this session"
    ), err
    assert f"(blueprint-session:{SESSION_ID})" in err


def test_session_open_an_unknown_node_names_it(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "design")])
    warm(root, "x-ccc33000")
    code, out, err = door(root, ["session", "open", "x-dead4321", "--harness", "claude", "--session-id", SESSION_ID])
    assert code == 2, err
    assert err == "session open: no exact node matches 'x-dead4321'.\n", err


def test_session_close_needs_summary_and_launch(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "design")])
    warm(root, "x-ccc33000")
    door(root, ["session", "open", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID])
    code, out, err = door(root, ["session", "close", "x-ccc33000"])
    assert code == 2, err
    assert "Missing option '--summary'." in err
    code, out, err = door(
        root, ["session", "close", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID, "--summary", "s"]
    )
    assert code == 2, err
    assert "Missing option '--launch'." in err


def test_session_close_prints_the_completion_receipt_and_launch_line(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "design")])
    warm(root, "x-ccc33000")
    door(root, ["session", "open", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID])
    code, out, err = door(
        root,
        [
            "session", "close", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID,
            "--summary", "blueprint done",
            "--launch", "claude /fno:target x-ccc33000",
        ],
    )
    assert code == 0, err
    assert out.startswith(f"blueprint closed x-ccc33000 (claude:{SESSION_ID})"), out
    assert "summary: blueprint done\n" in out
    assert "launch: claude /fno:target x-ccc33000\n" in out


def test_session_close_warns_when_the_launch_is_not_a_plugin_qualified_verb(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "design")])
    warm(root, "x-ccc33000")
    door(root, ["session", "open", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID])
    code, out, err = door(
        root,
        [
            "session", "close", "x-ccc33000", "--harness", "claude", "--session-id", SESSION_ID,
            "--summary", "blueprint done",
            "--launch", "fno do target start x-ccc33000",
        ],
    )
    assert code == 0, err
    assert (
        "session close: dispatch_verb not written:"
        " launch token 'fno' is not a plugin-qualified verb.\n"
    ) in err
