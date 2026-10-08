"""Golden receipts: blueprint session open / close (the lease lifecycle)."""
from __future__ import annotations

from tests.goldens._door import SESSION_ID, door, graph_rows, make_sandbox, seed_node, warm


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
    # The sandbox has no harness binary, so the liveness probe may warn first.
    refusal = [line for line in err.splitlines() if not line.startswith("WARN: ")]
    assert refusal[0].startswith(
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


def test_session_add_records_and_stamps_the_row(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaa11010")])
    warm(root, "x-aaa11010")
    code, out, err = door(
        root,
        [
            "session", "add", "x-aaa11010",
            "--phase", "execute",
            "--harness", "claude", "--session-id", SESSION_ID,
        ],
    )
    assert code == 0, err
    assert out == f"recorded execute claude:{SESSION_ID} on x-aaa11010\n", out
    rows = graph_rows(root)
    assert rows[0]["sessions"][0]["phase"] == "execute"


def test_session_add_a_second_stamp_is_already_recorded(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaa11010")])
    warm(root, "x-aaa11010")
    argv = ["session", "add", "x-aaa11010", "--phase", "execute",
            "--harness", "claude", "--session-id", SESSION_ID]
    door(root, argv)
    code, out, err = door(root, argv)
    assert code == 0, err
    assert out == f"already recorded execute claude:{SESSION_ID} on x-aaa11010\n", out


def test_session_add_ending_another_sessions_row_names_the_reap_door(tmp_path, monkeypatch):
    owner = "sess-owner-00000000"
    # The ambient identity is NOT the row owner: only the owning session ends
    # its own row, so this synthesized close refuses and names the reap door.
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-other-000000")
    root = make_sandbox(
        tmp_path,
        [
            seed_node(
                "x-bbb22000",
                sessions=[{
                    "phase": "execute",
                    "harness": "claude",
                    "session_id": owner,
                    "started_at": "2026-09-20T00:00:00Z",
                }],
            )
        ],
    )
    warm(root, "x-bbb22000")
    code, out, err = door(
        root,
        [
            "session", "add", "x-bbb22000",
            "--phase", "execute",
            "--harness", "claude", "--session-id", owner,
            "--ended-at", "2026-09-27T00:00:00Z",
        ],
    )
    assert code == 2, out
    assert "owns an open execute row on x-bbb22000" in err
    assert "only that session ends it here" in err
    assert "fno backlog session reap-open x-bbb22000 --phase execute" in err
