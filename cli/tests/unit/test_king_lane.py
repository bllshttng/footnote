"""The operator lane parser: the single implementation two consumers read.

A missing file is an empty lane; an unreadable one sets `error`. Those two
must never collapse into each other, which is why each has its own test.
"""
from __future__ import annotations

from fno.king.lane import open_items, parked_items, read_lane


def _write(tmp_path, text):
    path = tmp_path / "my-priorities.md"
    path.write_text(text, encoding="utf-8")
    return path


def test_two_unchecked_items_are_both_open(tmp_path):
    path = _write(tmp_path, "- [ ] ship 981 and 971 tonight\n- [ ] call the dentist\n")
    read = read_lane(path)
    assert not read.error
    items = open_items(read)
    assert [i.text for i in items] == ["ship 981 and 971 tonight", "call the dentist"]


def test_a_filed_item_carries_its_node_and_is_not_open(tmp_path):
    path = _write(tmp_path, "- [ ] ship it -> x-aaae\n")
    read = read_lane(path)
    item = read.items[0]
    assert item.node == "x-aaae"
    assert item.text == "ship it"
    assert open_items(read) == []


def test_a_parked_item_carries_its_reason_and_is_not_open(tmp_path):
    path = _write(tmp_path, "- [ ] call the dentist -> parked: not node-shaped\n")
    read = read_lane(path)
    item = read.items[0]
    assert item.parked == "not node-shaped"
    assert item.text == "call the dentist"
    assert open_items(read) == []
    assert [i.text for i in parked_items(read)] == ["call the dentist"]


def test_a_bare_park_with_no_reason_is_not_a_park(tmp_path):
    """An unreasoned `-> parked:` stays open; \\S in the reason group enforces it."""
    path = _write(tmp_path, "- [ ] call the dentist -> parked:\n")
    read = read_lane(path)
    item = read.items[0]
    assert item.parked is None
    assert item.node is None
    assert item.text == "call the dentist -> parked:"
    assert [i.text for i in open_items(read)] == ["call the dentist -> parked:"]
    assert parked_items(read) == []


def test_a_ticked_item_is_done_and_not_open(tmp_path):
    path = _write(tmp_path, "- [x] shipped already\n")
    read = read_lane(path)
    assert read.items[0].done is True
    assert open_items(read) == []


def test_blank_lines_and_headings_are_skipped(tmp_path):
    path = _write(tmp_path, "# my lane\n\n- [ ] the one real item\n\nnotes: whatever\n")
    read = read_lane(path)
    assert [i.text for i in read.items] == ["the one real item"]


def test_missing_file_is_an_empty_lane_not_an_error(tmp_path):
    read = read_lane(tmp_path / "does-not-exist.md")
    assert read.error == ""
    assert read.items == []


def test_undecodable_file_sets_error(tmp_path):
    path = tmp_path / "my-priorities.md"
    path.write_bytes(b"\xff\xfe\x00bad")
    read = read_lane(path)
    assert read.error
    assert "cannot read operator lane" in read.error
