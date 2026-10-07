"""Golden receipts: done, reopen, defer, undefer, queue, queued, unqueue,
contain, supersede."""
from __future__ import annotations

from tests.goldens._door import door, graph_rows, make_sandbox, seed_node, warm


# -- done --

def test_done_with_note_closes_and_prints_the_close_receipt(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaaa1111", "in_progress")])
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["done", "x-aaaa1111", "--note", "wrapped by hand"])
    assert code == 0, err
    assert out == (
        "fno backlog done: x-aaaa1111 -> done  domain=code"
        "  note='wrapped by hand'  session=test-ses\n"
    ), out


def test_done_without_evidence_refuses_and_writes_nothing(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaaa1111", "in_progress")])
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["done", "x-aaaa1111"])
    assert code == 1, err
    assert out == "", out
    assert err.startswith("Error: refused:"), err
    assert "would close done with no record of why" in err
    assert "Nothing was written." in err
    assert "--pr-number <n>" in err
    assert '--note "<why it is done>"' in err
    assert "--link <artifact url>" in err


def test_done_on_a_done_node_updates_metadata_in_place(tmp_path):
    root = make_sandbox(
        tmp_path,
        [seed_node("x-aaaa1111", "done", completion_note="first", completed_at="2026-09-27T10:00:00+00:00")],
    )
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["done", "x-aaaa1111", "--note", "second close"])
    assert code == 0, err
    assert out == "fno backlog done: x-aaaa1111 -> already done (metadata updated)\n", out
    assert err.endswith(
        " already done at 2026-09-27T10:00:00+00:00;"
        " metadata updates applied; collision event emitted\n"
    ), err


def test_done_unknown_id_names_the_unresolved_feature(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaaa1111")])
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["done", "x-dead4321"])
    assert code == 1, err
    assert err == "Error: feature x-dead4321 not found\n", err


def test_done_an_unformable_id_is_a_clean_no_match(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaaa1111")])
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["done", "x-zzzz9999"])
    assert code == 2, err
    assert out == "", out
    assert err == (
        "fno backlog done: no match for 'x-zzzz9999'\n"
        "  (query 'x-zzzz9999': no matches)\n"
    ), err


# -- reopen --

def test_reopen_needs_a_reason(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaaa1111", "done")])
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["reopen", "x-aaaa1111"])
    assert code == 2, err
    assert "Missing option '--reason' / '-R'." in err


def test_reopen_a_done_node_prints_the_reopen_receipt(tmp_path):
    root = make_sandbox(
        tmp_path,
        [seed_node("x-aaaa1111", "done", completed_at="2026-09-27T10:00:00+00:00")],
    )
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["reopen", "x-aaaa1111", "-R", "pr merged wrong"])
    assert code == 0, err
    assert out == "Reopened x-aaaa1111\n", out


def test_reopen_reads_the_completion_not_the_status_field(tmp_path):
    # A row stamped done without a completion timestamp reads as live to
    # reopen: the completion stamp is the evidence of done, not the status.
    root = make_sandbox(tmp_path, [seed_node("x-aaaa1111", "done")])
    warm(root, "x-aaaa1111")
    code, out, err = door(root, ["reopen", "x-aaaa1111", "-R", "not actually done"])
    assert code == 0, err
    assert out == "", out
    assert err == "warning: x-aaaa1111 is not done; nothing to reopen\n", err


# -- defer / undefer --


def test_defer_prints_the_deferred_receipt_and_stamps_the_kind(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-cccc3333")])
    warm(root, "x-cccc3333")
    code, out, err = door(root, ["defer", "x-cccc3333", "-R", "parked until api lands", "-K", "later"])
    assert code == 0, err
    assert out == 'Deferred x-cccc3333: "parked until api lands"\n', out
    rows = {r["id"]: r for r in graph_rows(root)}
    assert rows["x-cccc3333"]["status"] == "deferred"
    assert rows["x-cccc3333"]["deferred_kind"] == "later"


def test_defer_an_unformable_id_refuses_at_the_gate(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-cccc3333")])
    warm(root, "x-cccc3333")
    code, out, err = door(root, ["defer", "x-zzzz9999", "-R", "nope"])
    assert code == 1, err
    assert err.startswith("Error: task_id must be a <prefix>-<4..8 hex> node id, got 'x-zzzz9999'\n")


# -- queue family --


def test_queued_lists_the_queue_as_json(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff30000")])
    warm(root, "x-fff30000")
    door(root, ["queue", "x-fff30000"])
    code, out, err = door(root, ["queued"])
    assert code == 0, err
    import json

    rows = json.loads(out)
    assert len(rows) == 1
    assert rows[0]["id"] == "x-fff30000"
    assert "queued_at" in rows[0]


def test_queue_is_idempotent(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff30000")])
    warm(root, "x-fff30000")
    door(root, ["queue", "x-fff30000"])
    code, out, err = door(root, ["queue", "x-fff30000"])
    assert code == 0, err
    assert out == "Queued x-fff30000\n", out


def test_unqueue_an_unqueued_node_succeeds_with_a_warning(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff30000")])
    warm(root, "x-fff30000")
    code, out, err = door(root, ["unqueue", "x-fff30000"])
    assert code == 0, err
    assert out == "Unqueued x-fff30000\n", out
    assert err == "warning: x-fff30000 was not queued\n", err


# -- contain --

def test_contain_stamps_parent_and_contained_in_and_prints_per_child(tmp_path):
    root = make_sandbox(
        tmp_path,
        [seed_node("x-ccc99000", "in_progress"), seed_node("x-ddd10000"), seed_node("x-eee20000")],
    )
    warm(root, "x-ccc99000")
    code, out, err = door(root, ["contain", "x-ccc99000", "x-ddd10000", "x-eee20000"])
    assert code == 0, err
    assert out == (
        "contained x-ddd10000 into x-ccc99000; it ships inside x-ccc99000's PR\n"
        "contained x-eee20000 into x-ccc99000; it ships inside x-ccc99000's PR\n"
    ), out
    rows = {r["id"]: r for r in graph_rows(root)}
    assert rows["x-ddd10000"]["parent"] == "x-ccc99000"
    assert rows["x-ddd10000"]["contained_in"] == "x-ccc99000"


# -- supersede --

def test_supersede_without_a_cause_prints_the_evidence_trail_refusal(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaa11010"), seed_node("x-bbb22000")])
    warm(root, "x-aaa11010")
    code, out, err = door(root, ["supersede", "x-bbb22000", "--replaces", "x-aaa11010"])
    assert code == 1, err
    assert out == "", out
    assert "--cause is required and cannot be blank." in err
    assert "A supersede carries the evidence trail" in err
    assert "fno backlog supersede <new> --replaces <old>" in err


def test_supersede_a_shipped_target_refuses(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ccc33000", "done"), seed_node("x-bbb22000")])
    warm(root, "x-bbb22000")
    code, out, err = door(
        root,
        ["supersede", "x-bbb22000", "--replaces", "x-ccc33000", "--cause", "old approach", "--surface", "a.py"],
    )
    assert code == 1, err
    assert err == (
        "Error: cannot supersede x-ccc33000: it is already shipped (status=done)."
        " Open a follow-up node instead.\n"
    ), err


def test_supersede_stamps_the_edge_and_superseded_status(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaa11010"), seed_node("x-bbb22000")])
    warm(root, "x-aaa11010")
    code, out, err = door(
        root,
        ["supersede", "x-bbb22000", "--replaces", "x-aaa11010", "--cause", "old approach", "--surface", "a.py"],
    )
    assert code == 0, err
    assert out == "superseded x-aaa11010 with x-bbb22000\n", out
    rows = {r["id"]: r for r in graph_rows(root)}
    assert rows["x-aaa11010"]["status"] == "superseded"
    assert rows["x-aaa11010"]["superseded_by"] == "x-bbb22000"
    assert rows["x-bbb22000"]["supersedes"] == ["x-aaa11010"]


def test_supersede_an_unknown_new_node_refuses(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-aaa11010")])
    warm(root, "x-aaa11010")
    code, out, err = door(
        root,
        ["supersede", "x-dead4321", "--replaces", "x-aaa11010", "--cause", "c", "--surface", "a.py"],
    )
    assert code == 1, err
    assert err == "Error: new node x-dead4321 not found\n", err
