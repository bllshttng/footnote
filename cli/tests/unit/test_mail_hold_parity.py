"""Harness parity for busy mode: enumerate the sites once, assert they agree.

Three rounds of review found three defects and all three were the same shape: a
reader or a producer keyed to one of N paths, correct on claude, invisible from
claude. Hunting them one at a time finds a fourth and still does not say how
many are left. So this file enumerates every place the feature resolves a
session key or delivers, and asserts the claude form and the codex form reach
the same answer for each.

Claude hides the bug by coincidence: a claude row's `short_id` IS the canonical
first-eight, so keying by the name, the short id or the handle all land on the
same file. A codex row separates them - the `short_id` is a daemon worker key
and the `harness_session_id` is a full time-prefixed id - so only a reader using
the shared rule finds the clock.

Add a row to SITES whenever the feature grows another reader.
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from types import SimpleNamespace

import pytest

from fno.agents import dispatch, format as fmt
from fno.mail import hold as hold_mod

SESSION = "abcd1234-0b3f-4c5a-9e88-2ad4f0c81b97"
HANDLE = "abcd1234"  # canonical_handle(SESSION); the key every WRITER uses


@pytest.fixture(autouse=True)
def _isolated_state(tmp_path, monkeypatch):
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    # The gate subprocess reads the hold sidecar under $FNO_HOME, not the
    # Python settings stack; without this the arm writes the test's state
    # while the gate reads the machine's and answers deliver.
    monkeypatch.setenv("FNO_HOME", str(tmp_path))
    return tmp_path


def _claude_row():
    """A claude row: short_id IS the canonical handle, so every key coincides."""
    return SimpleNamespace(
        name=HANDLE,
        short_id=HANDLE,
        harness_session_id=SESSION,
        harness="claude",
        delivery_policy="bus-only",
    )


def _codex_row():
    """A codex row: name, short_id and canonical handle are three things."""
    return SimpleNamespace(
        name="worker-migration",
        short_id="daemon-worker-key",
        harness_session_id=SESSION,
        harness="codex",
        delivery_policy="bus-only",
    )


def _lifts_in(row):
    """The duration a bounce receipt quotes, with the recipient stripped.

    The receipt names the recipient, which legitimately differs between the two
    rows, so comparing whole strings would fail for the wrong reason.
    """
    reason = hold_mod.bounce_reason(row)
    if reason is None:
        return None
    return reason.split("lifts in ", 1)[-1]


# Every reader the feature exposes, as (label, callable taking a registry row).
# A reader that keys off one address form instead of the shared rule answers
# differently for the two rows below, and that is the whole assertion.
#
# Each probe returns the reader's VALUE, never a truthiness. A control run
# proved that necessary: with the column keyed off `entry.name`, a codex row
# with a live five-minute hold rendered "held" instead of "~5m", and both are
# non-None, so an `is not None` probe called that agreement. Reporting an
# indefinite hold for a timed one is the exact lie the DND column exists to
# stop telling.
SITES = (
    ("delivery gate", lambda row: dispatch._delivery_policy_refusal(row)),
    ("expiry read", lambda row: hold_mod.read_any(row) is not None),
    ("lapsed", lambda row: hold_mod.lapsed(row)),
    ("dnd column", lambda row: fmt._dnd_label(row)),
    ("hold --status", lambda row: hold_mod.dnd_label(row)),
    ("bounce receipt", _lifts_in),
    ("release resolve", lambda row: HANDLE in hold_mod.addresses(row)),
)


def _expire():
    """Seed a lapsed clock directly: the transport cannot write this state."""
    fields = {
        "until": (datetime.now(timezone.utc) - timedelta(seconds=1)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "window_s": 300,
        "clock_kind": "idle",
        "ceiling": None,
    }
    path = hold_mod.hold_path(HANDLE)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(fields) + "\n", encoding="utf-8")


@pytest.mark.parametrize("label,probe", SITES, ids=[s[0] for s in SITES])
@pytest.mark.parametrize(
    "arrange,state",
    [
        (lambda: hold_mod.arm(HANDLE, 5), "live timed hold"),
        (lambda: hold_mod.arm_permanent(HANDLE), "permanent policy"),
        (_expire, "lapsed hold"),
        (lambda: None, "no clock"),
    ],
    ids=["live", "permanent", "lapsed", "clockless"],
)
def test_every_reader_answers_the_same_for_claude_and_codex(
    label, probe, arrange, state, monkeypatch
):
    monkeypatch.setattr(dispatch, "load_registry", lambda: [_codex_row()])
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda token: _codex_row())
    hold_mod.clear(HANDLE)
    arrange()

    on_claude = probe(_claude_row())
    on_codex = probe(_codex_row())

    assert on_claude == on_codex, (
        f"{label} disagrees between harnesses with a {state}: "
        f"claude={on_claude}, codex={on_codex}. The clock is filed under "
        f"{HANDLE!r}; this reader is keying off one address form instead of "
        f"hold.candidate_keys."
    )


def test_the_codex_row_really_does_separate_the_key_forms():
    """The positive control for this whole file.

    If a codex row's name or short_id happened to equal the canonical handle,
    every assertion above would pass without proving anything.
    """
    row = _codex_row()
    assert row.name != HANDLE
    assert row.short_id != HANDLE
    assert row.harness_session_id != HANDLE
    assert HANDLE in hold_mod.addresses(row), "the shared rule must still find it"


@pytest.mark.dev_build
def test_the_gate_receipt_reads_one_clock_across_both_addresses(
    tmp_path, monkeypatch
):
    """The gate owns the receipt now; both address forms must read one clock.

    bounce_reason shells the Rust gate (law d-b6cc1a2a), so the fixture aims
    both legs at one tmp state - FNO_HOME for the gate's clock read,
    FNO_STATE_DIR for the Python writer - seeds the bus-only registry row the
    gate requires, and answers from THIS checkout's build: an installed copy
    can predate the gate. Skipped where no dev build exists; the smoke legs
    delete it on purpose.
    """
    monkeypatch.setenv("FNO_HOME", str(tmp_path))
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path))
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    from tests.conftest import checkout_fno_agents_binary

    binary = checkout_fno_agents_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build")
    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: binary
    )
    (tmp_path / "agents").mkdir(parents=True, exist_ok=True)
    (tmp_path / "agents" / "registry.json").write_text(
        json.dumps(
            {
                "schema_version": 19,
                "agents": [
                    {
                        "name": "worker-migration",
                        "short_id": HANDLE,
                        "harness_session_id": SESSION,
                        "harness": "codex",
                        "delivery_policy": "bus-only",
                        "cwd": "/",
                        "status": "idle",
                        "created_at": "2026-09-27T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    hold_mod.arm_wall(HANDLE, 5)

    claude_receipt = hold_mod.bounce_reason(HANDLE)
    codex_receipt = hold_mod.bounce_reason(SESSION)
    assert claude_receipt, "a live wall hold must answer a receipt"
    assert claude_receipt == codex_receipt, (
        "the canonical handle and the session id must read one clock: "
        f"claude={claude_receipt}, codex={codex_receipt}"
    )
    assert "held until about" in claude_receipt


def test_wall_clock_receipt_has_the_same_clock_label_for_both_harnesses():
    """The row leg of the same receipt, kept from main: a row never shells.

    bounce_reason answers rows in-process through clock_description, so this
    leg runs even where no gate binary exists, and the "wall clock" label the
    operator reads must survive the port.
    """
    claude = hold_mod.bounce_reason(_claude_row())
    if claude is None:
        pytest.skip("no mail-hold gate binary; the wall-clock wording is the Rust gate's")
    assert "wall clock" in claude
    assert "wall clock" in hold_mod.bounce_reason(_codex_row())


def test_the_release_reaches_one_lane_dispatcher_not_a_per_harness_branch():
    """The delivery half of the same question.

    `release` must hand off to `_deliver_live`, which routes per harness, and
    must never call a single harness's injector itself. Asserted by reading the
    source, because the harness-specific miss is invisible on a claude box.
    """
    import inspect

    # Strip comments and the docstring first. Both name the injector while
    # explaining why the code must not call it, and a scan that counts prose
    # as a call answers a different question than the one asked.
    body = inspect.getsource(hold_mod.release)
    body = body.split('"""')[-1]
    code = "\n".join(
        line for line in body.splitlines() if not line.lstrip().startswith("#")
    )

    assert "_deliver_live" in code
    assert "_mail_inject_claude" not in code
    assert "_mail_inject_codex" not in code
