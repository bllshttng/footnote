"""The rolling 24h wake ledger: the window rolls, the 22nd wake lands, the 33rd refuses.

The ledger lives on the king manifest as ``wake_times``, a comma-joined list of
RFC3339 UTC stamps pruned to the trailing 24 hours at every read and write. The
tests assert POSITIVE markers: an allow verdict with a count, a refusal naming
its word, a rewritten line with every other line byte-identical. An emptied
list and an unreadable store read differently here (empty list vs absent) only
because read_wakes returns a list and the tests pin which is which.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

from fno.king.wake import admit_wake, bill_wake, read_wakes, should_wake

NOW = datetime(2026, 8, 29, 12, 0, 0, tzinfo=timezone.utc)

CEILING = 32
DEBOUNCE_S = 900


def _stamp(minutes_ago: float) -> datetime:
    return NOW - timedelta(minutes=minutes_ago)


def _manifest(path, stamps: list[datetime], *, extra: str = "") -> None:
    joined = ",".join(s.strftime("%Y-%m-%dT%H:%M:%SZ") for s in stamps)
    body = (
        "---\n"
        'fno_id: "20260829T120000Z-kg1-abc123"\n'
        "scope: epic-x\n"
        "respawn_count: 3\n"
        "respawn_ceiling: 4\n"
        f"wake_times: {joined}\n"
        "---\n"
        "A body line the rewrite must not touch.\n"
    )
    if extra:
        body += extra
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


def test_the_22nd_wake_inside_24h_is_allowed(tmp_path):
    # 21 billed stamps spread over the window at or beyond debounce spacing.
    stamps = [_stamp(30 + 60 * i) for i in range(21)]
    assert len(stamps) == 21
    path = tmp_path / "kings" / "epic-x.md"
    _manifest(path, stamps)

    verdict = should_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)

    assert verdict.allowed, f"the 22nd wake inside 24h must land: {verdict}"
    assert verdict.count == 21


def _thirty_two_at_the_ceiling() -> list[datetime]:
    """32 stamps, oldest 23h30m ago, newest 45m ago: a full, fresh window."""
    return [_stamp(1410 - 45 * i) for i in range(32)]


def test_wake_33_in_the_same_window_is_refused_naming_ceiling(tmp_path):
    stamps = _thirty_two_at_the_ceiling()
    assert len(stamps) == 32
    path = tmp_path / "kings" / "epic-x.md"
    _manifest(path, stamps)

    verdict = should_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)

    assert not verdict.allowed, "a ceiling that never refuses is not a ceiling"
    assert verdict.refusal == "ceiling"
    assert verdict.count == 32


def test_the_window_rolls_it_does_not_tumble(tmp_path):
    # The half that separates a rolling window from an anchored one: 32 stamps
    # fill the window now, and the oldest ages out WITHOUT the anchor moving.
    # A window keyed on the first-ever wake would keep refusing; a rolling one
    # drops the aged stamp and the next trigger is allowed.
    stamps = _thirty_two_at_the_ceiling()
    path = tmp_path / "kings" / "epic-x.md"
    _manifest(path, stamps)
    refused = should_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)
    assert refused.refusal == "ceiling", "fixture must start at the ceiling"

    # 45 minutes later the oldest stamp (23h30m old) is past 24h and aged
    # out; 31 remain, which is back under the ceiling of 32.
    later = NOW + timedelta(minutes=45)

    verdict = should_wake(path, now=later, ceiling=CEILING, debounce_s=DEBOUNCE_S)

    assert verdict.allowed, (
        f"the oldest stamp is past 24h; the window must roll: {verdict}"
    )
    assert verdict.count == 31


def test_a_fresh_wake_is_refused_naming_debounce_and_leaves_the_stamp(tmp_path):
    path = tmp_path / "kings" / "epic-x.md"
    _manifest(path, [_stamp(5)])

    verdict = should_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)

    assert not verdict.allowed
    assert verdict.refusal == "debounce"


def test_bill_wake_appends_prunes_and_touches_no_other_line(tmp_path):
    path = tmp_path / "kings" / "epic-x.md"
    stale = [_stamp(25 * 60)]  # outside the trailing 24h
    fresh = [_stamp(60), _stamp(30)]
    _manifest(path, stale + fresh)
    before = path.read_text(encoding="utf-8")

    count = bill_wake(path, now=NOW)

    after = path.read_text(encoding="utf-8")
    assert count == 3, "the two fresh stamps plus this one"
    kept = read_wakes(path, now=NOW)
    assert len(kept) == 3, "the stale stamp is pruned on the write"
    lines_before = before.splitlines()
    lines_after = after.splitlines()
    assert len(lines_before) == len(lines_after)
    for i, (b, a) in enumerate(zip(lines_before, lines_after)):
        if b.startswith("wake_times:"):
            assert not a == b, "the wake_times line must move"
        else:
            assert a == b, f"line {i} changed: {b!r} -> {a!r}"


def test_bill_wake_on_a_manifest_that_predates_the_field(tmp_path):
    path = tmp_path / "kings" / "epic-x.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "---\nfno_id: k-1\nscope: epic-x\nrespawn_ceiling: 4\n---\nbody\n",
        encoding="utf-8",
    )

    count = bill_wake(path, now=NOW)

    assert count == 1
    assert "wake_times: " in path.read_text(encoding="utf-8")


def test_an_unparseable_stamp_is_dropped_not_fatal(tmp_path):
    path = tmp_path / "kings" / "epic-x.md"
    good = _stamp(60).strftime("%Y-%m-%dT%H:%M:%SZ")
    _manifest(path, [])
    path.write_text(
        path.read_text(encoding="utf-8").replace(
            "wake_times: ", f"wake_times: not-a-stamp,{good}"
        ),
        encoding="utf-8",
    )

    stamps = read_wakes(path, now=NOW)

    assert stamps == [_stamp(60)], "the good stamp survives, the bad one drops"


def test_an_absent_store_reads_as_empty_not_as_an_error(tmp_path):
    assert read_wakes(tmp_path / "kings" / "nope.md", now=NOW) == []


def test_write_manifest_arms_wake_times_empty_and_a_rearm_resets_it(tmp_path):
    from fno.king.state import parse_manifest, write_manifest

    path = tmp_path / "kings" / "epic-x.md"
    write_manifest(path, scope="epic-x", harness_session_id="session-1")
    assert parse_manifest(path)["wake_times"] == ""

    bill_wake(path, now=NOW)
    assert parse_manifest(path)["wake_times"], "the bill landed"

    # A force re-arm is a new reign generation: the wake budget restarts.
    write_manifest(
        path, scope="epic-x", harness_session_id="session-2", force=True
    )
    assert parse_manifest(path)["wake_times"] == ""


def test_admit_wake_bills_on_allow_so_the_second_tick_takes_the_refusal(tmp_path):
    # The two-reader race: two ticks both read an empty ledger. Deciding and
    # billing under one lock means the FIRST call lands its stamp before the
    # second ever reads, so the second answers the same question with the
    # winner's stamp in view.
    path = tmp_path / "kings" / "epic-x.md"
    _manifest(path, [])

    first = admit_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)
    second = admit_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)

    assert first.allowed and first.count == 1
    assert not second.allowed and second.refusal == "debounce"
    assert len(read_wakes(path, now=NOW)) == 1, "exactly one wake was billed"


def test_admit_wake_at_the_ceiling_refuses_without_billing(tmp_path):
    path = tmp_path / "kings" / "epic-x.md"
    _thirty_two = [_stamp(1410 - 45 * i) for i in range(32)]
    _manifest(path, _thirty_two)

    verdict = admit_wake(path, now=NOW, ceiling=CEILING, debounce_s=DEBOUNCE_S)

    assert verdict.refusal == "ceiling"
    assert len(read_wakes(path, now=NOW)) == 32, "a refusal must not bill"


def test_a_ceiling_of_zero_is_unbounded_even_past_the_default(tmp_path):
    path = tmp_path / "kings" / "epic-x.md"
    _manifest(path, [_stamp(20 + 60 * i) for i in range(40)])

    verdict = should_wake(path, now=NOW, ceiling=0, debounce_s=DEBOUNCE_S)

    assert verdict.allowed, "40 stamps pass when the operator disabled the cap"
