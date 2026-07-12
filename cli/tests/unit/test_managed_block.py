"""Tests for the offered managed block (node x-0960, W4 / US7-US8).

Covers the plan's acceptance criteria:
  AC5-HP    stamp + re-stamp keeps bytes outside the markers byte-identical
  AC2-ERR   exactly one marker -> refuse, touch nothing
  AC2-EDGE  a first-time decline is durable (no re-prompt, no file touched)
  US8       fno doctor flags a stale block version (advisory)

Run: fno test -- -k managed_block
"""
from __future__ import annotations

from pathlib import Path

from fno.setup.managed_block import (
    BLOCK_VERSION,
    marker_state,
    offer_managed_block,
    render_block,
    resolve_target,
    stamp_block,
    stamped_version,
)

_USER = "# My House Rules\n\nDo the thing. Do not break the build.\n"


def test_stamp_and_restamp_preserve_outside_bytes(tmp_path: Path) -> None:
    """AC5-HP: append on a marker-less file, then re-stamp to a new version;
    every byte outside the fences is untouched across both."""
    f = tmp_path / "AGENTS.md"
    f.write_text(_USER, encoding="utf-8")

    first = stamp_block(f, version=1)
    assert first.action == "appended"
    after = f.read_text(encoding="utf-8")
    assert after.startswith(_USER)  # user prose untouched, block appended below
    assert stamped_version(after) == 1

    second = stamp_block(f, version=2)
    assert second.action == "restamped"
    restamped = f.read_text(encoding="utf-8")
    assert stamped_version(restamped) == 2
    # Outside-marker bytes are exactly the original prose; only the fence changed.
    before_block = restamped.split("<!-- fno:begin", 1)[0]
    assert before_block == after.split("<!-- fno:begin", 1)[0] == _USER + "\n"


def test_crlf_outside_markers_preserved(tmp_path: Path) -> None:
    """A CRLF host file keeps its \\r\\n line endings outside the fences across
    append + re-stamp (the invariant is bytes, not just visible text)."""
    f = tmp_path / "AGENTS.md"
    crlf = "# House Rules\r\nline one\r\nline two\r\n"
    f.write_bytes(crlf.encode("utf-8"))
    stamp_block(f, version=1)
    stamp_block(f, version=2)
    raw = f.read_bytes().decode("utf-8")
    before_block = raw.split("<!-- fno:begin", 1)[0]
    # The host's own CRLF bytes survive verbatim (not normalized to \n); the
    # separator we add before our fence is our content, not theirs.
    assert before_block.startswith(crlf), before_block
    assert "\r\n" in before_block


def test_never_downgrades_newer_block(tmp_path: Path) -> None:
    """offer leaves a block newer than the template alone (no downgrade prompt)."""
    f = tmp_path / "AGENTS.md"
    f.write_text("# rules\n", encoding="utf-8")
    stamp_block(f, version=BLOCK_VERSION + 1)  # host carries a newer stamp
    calls: list[str] = []
    res = offer_managed_block(
        tmp_path, confirm_fn=lambda m: calls.append(m) or True, version=BLOCK_VERSION
    )
    assert res["status"] == "current"
    assert calls == []  # never prompted to downgrade


def test_restamp_same_version_is_current_noop(tmp_path: Path) -> None:
    f = tmp_path / "AGENTS.md"
    f.write_text(_USER, encoding="utf-8")
    stamp_block(f, version=BLOCK_VERSION)
    snapshot = f.read_text(encoding="utf-8")
    res = stamp_block(f, version=BLOCK_VERSION)
    assert res.action == "current"
    assert f.read_text(encoding="utf-8") == snapshot  # byte-identical no-op


def test_stamp_empty_file_has_no_leading_blanks(tmp_path: Path) -> None:
    """An existing empty/whitespace-only file gets the block with no leading
    blank lines (the block starts at byte 0)."""
    f = tmp_path / "AGENTS.md"
    f.write_text("   \n\n", encoding="utf-8")  # whitespace only
    res = stamp_block(f, version=BLOCK_VERSION)
    assert res.action == "appended"
    assert f.read_text(encoding="utf-8").startswith("<!-- fno:begin")


def test_malformed_single_marker_refuses(tmp_path: Path) -> None:
    """AC2-ERR: a file with fno:begin but no fno:end is refused untouched."""
    f = tmp_path / "AGENTS.md"
    broken = _USER + "\n<!-- fno:begin v=1 -->\nstuff\n"  # no end marker
    f.write_text(broken, encoding="utf-8")
    assert marker_state(broken) == "malformed"

    res = stamp_block(f, version=BLOCK_VERSION)
    assert res.action == "refused-malformed"
    assert f.read_text(encoding="utf-8") == broken  # nothing written


def test_offer_declined_is_durable(tmp_path: Path) -> None:
    """AC2-EDGE: decline once -> no file created, and a second run does not
    re-prompt (a recorded decline, not amnesia)."""
    f = tmp_path / "AGENTS.md"
    f.write_text(_USER, encoding="utf-8")

    first = offer_managed_block(tmp_path, confirm_fn=lambda _m: False)
    assert first["status"] == "declined"
    assert stamped_version(f.read_text(encoding="utf-8")) is None  # untouched

    calls: list[str] = []

    def _confirm(msg: str) -> bool:
        calls.append(msg)
        return True  # would accept - but must never be asked

    second = offer_managed_block(tmp_path, confirm_fn=_confirm)
    assert second["status"] == "declined-remembered"
    assert calls == []  # no re-prompt
    assert stamped_version(f.read_text(encoding="utf-8")) is None


def test_offer_accept_stamps_and_clears_decline(tmp_path: Path) -> None:
    f = tmp_path / "AGENTS.md"
    f.write_text(_USER, encoding="utf-8")
    res = offer_managed_block(tmp_path, confirm_fn=lambda _m: True)
    assert res["status"] == "appended"
    assert stamped_version(f.read_text(encoding="utf-8")) == BLOCK_VERSION


def test_resolve_target_prefers_existing_agents_then_claude(tmp_path: Path) -> None:
    assert resolve_target(tmp_path).name == "AGENTS.md"  # neither exists -> AGENTS
    (tmp_path / "CLAUDE.md").write_text("x", encoding="utf-8")
    assert resolve_target(tmp_path).name == "CLAUDE.md"  # only CLAUDE exists
    (tmp_path / "AGENTS.md").write_text("x", encoding="utf-8")
    assert resolve_target(tmp_path).name == "AGENTS.md"  # AGENTS wins when both


def test_render_block_is_a_wellformed_pair() -> None:
    assert marker_state(render_block()) == "both"
    assert stamped_version(render_block(7)) == 7


def test_doctor_managed_block_flags_stale(tmp_path: Path, monkeypatch) -> None:
    """US8: doctor reports stale iff the stamped version < current template."""
    from fno import doctor

    monkeypatch.chdir(tmp_path)
    # No block -> silent (empty report).
    (tmp_path / "AGENTS.md").write_text(_USER, encoding="utf-8")
    assert doctor._managed_block_report() == {}

    # Current version -> present but not stale.
    stamp_block(tmp_path / "AGENTS.md", version=BLOCK_VERSION)
    rep = doctor._managed_block_report()
    assert rep["stamped"] == BLOCK_VERSION and rep["stale"] is False

    # An older stamp -> stale, advisory only.
    stamp_block(tmp_path / "AGENTS.md", version=BLOCK_VERSION - 1)
    stale = doctor._managed_block_report()
    assert stale["stale"] is True
    assert stale["file"] == "AGENTS.md" and stale["current"] == BLOCK_VERSION
