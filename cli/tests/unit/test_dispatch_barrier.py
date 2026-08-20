"""Claim-visibility barrier at the spawn-guard choke point (x-a7ab 1.2 / x-b44e).

spawn-guard is the single source of truth for the dispatch:<id> reservation,
called by both spawn.sh and dispatch-node.sh. After acquiring the reservation it
re-reads the claims dir to confirm THIS holder is the one on disk before
returning dispatchable; a peer that won a visibility-lagged race surfaces as a
different holder and this dispatcher skips with duplicate-claim so exactly one
worker launches.
"""
from concurrent.futures import ThreadPoolExecutor
import json
import threading
from types import SimpleNamespace

from typer.testing import CliRunner

from fno.agents.cli import _spawn_guard_decision, agents_app
from fno.agents.harness_map import resolve_dispatch

runner = CliRunner()


def _last_json(out: str) -> dict:
    for line in reversed(out.strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    raise AssertionError(f"no JSON verdict in output: {out!r}")


def _route_to(monkeypatch, root):
    # Route every claim key (node:/dispatch:) to an isolated tmp root so the real
    # acquire/claim_status machinery runs hermetically.
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: root)


def test_spawn_guard_serializes_two_callers(monkeypatch, tmp_path):
    # AC2-HP: two dispatch attempts for one node -> exactly one dispatchable.
    _route_to(monkeypatch, tmp_path)
    r1 = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "A", "--ttl", "3m", "--json"])
    assert r1.exit_code == 0, r1.output
    assert _last_json(r1.output)["verdict"] == "dispatchable"
    r2 = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "B", "--ttl", "3m", "--json"])
    v2 = _last_json(r2.output)
    assert v2["verdict"] == "already-running"
    assert v2["reason"] in ("reservation-held", "duplicate-claim")


def test_allowed_codex_panes_deduplicate_simultaneous_callers(monkeypatch, tmp_path):
    """Two allowed pane callers that observe a free node produce one launch."""
    _route_to(monkeypatch, tmp_path)

    from fno.claims import core as claims_core

    original_acquire = claims_core.acquire_claim
    simultaneous_acquire = threading.Barrier(2)

    def racing_acquire(key, *args, **kwargs):
        if key == "dispatch:N":
            simultaneous_acquire.wait(timeout=5)
        return original_acquire(key, *args, **kwargs)

    monkeypatch.setattr(claims_core, "acquire_claim", racing_acquire)

    def decide(holder):
        dispatch = resolve_dispatch(
            harness="codex",
            substrate="pane",
            node_id="N",
            trigger="autonomous",
        )
        assert dispatch["substrate"] == "pane"
        verdict, exit_code = _spawn_guard_decision(
            "N", holder, ttl="3m", handover_holder=f"spawn-handover:t-{holder}"
        )
        assert exit_code == 0
        return verdict

    with ThreadPoolExecutor(max_workers=2) as executor:
        verdicts = list(executor.map(decide, ("A", "B")))

    assert [item["verdict"] for item in verdicts].count("dispatchable") == 1
    loser = next(item for item in verdicts if item["verdict"] != "dispatchable")
    assert loser == {"verdict": "already-running", "reason": "reservation-held"}


def test_barrier_catches_peer_after_acquire(monkeypatch, tmp_path):
    # The barrier's distinct value: acquire succeeds, but the post-acquire re-read
    # sees a peer (a visibility-lagged race resolved against us) -> duplicate-claim,
    # never dispatchable.
    _route_to(monkeypatch, tmp_path)

    def status(key, root=None):
        if key == "node:N":
            return {"key": key, "state": "free"}
        return {"key": key, "state": "live", "holder": "target-session:PEER"}

    monkeypatch.setattr("fno.claims.core.claim_status", status)
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: SimpleNamespace(holder="ME"))
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--ttl", "3m", "--json"])
    v = _last_json(r.output)
    assert v["verdict"] == "already-running"
    assert v["reason"] == "duplicate-claim"
    assert v["holder"] == "target-session:PEER"


def test_barrier_passes_when_holder_is_ours(monkeypatch, tmp_path):
    _route_to(monkeypatch, tmp_path)
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, root=None: (
            {"key": key, "state": "free"}
            if key == "node:N"
            else {"key": key, "state": "live", "holder": "ME"}
        ),
    )
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: SimpleNamespace(holder="ME"))
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--ttl", "3m", "--json"])
    assert _last_json(r.output)["verdict"] == "dispatchable"


def test_no_reserve_skips_barrier(monkeypatch, tmp_path):
    # --no-reserve runs Guard 1 only and never acquires (read-only verdict).
    _route_to(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: {"key": key, "state": "free"})
    acq = []
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: acq.append(1))
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--no-reserve", "--json"])
    assert _last_json(r.output)["verdict"] == "dispatchable"
    assert acq == []


def test_reservation_precedes_dispatchable(monkeypatch, tmp_path):
    # Ordering invariant: the reservation is acquired before the dispatchable
    # verdict - no observable work precedes the on-disk reservation.
    _route_to(monkeypatch, tmp_path)
    order = []

    def status(key, root=None):
        order.append(("probe", key))
        return (
            {"key": key, "state": "free"}
            if key == "node:N"
            else {"key": key, "state": "live", "holder": "ME"}
        )

    def acq(*a, **k):
        order.append(("acquire", a[0] if a else k.get("key")))
        return SimpleNamespace(holder="ME")

    monkeypatch.setattr("fno.claims.core.claim_status", status)
    monkeypatch.setattr("fno.claims.core.acquire_claim", acq)
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--ttl", "3m", "--json"])
    assert _last_json(r.output)["verdict"] == "dispatchable"
    assert order[0] == ("probe", "node:N")  # Guard 1 first
    assert ("acquire", "dispatch:N") in order  # reservation on disk


# ---------------------------------------------------------------------------
# the guard clears what it can prove, and refuses loudly otherwise (x-05be)
# ---------------------------------------------------------------------------


def _dead_pid():
    import psutil

    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    return dead


def _fake_roster(monkeypatch, rows, warnings=()):
    monkeypatch.setattr(
        "fno.agents.watchdog.fleet_rows", lambda *_a, **_kw: (list(rows), list(warnings))
    )


def _row(name, state, node):
    from fno.agents.watchdog import Row

    return Row(row_id=name, name=name, state=state, node=node, cwd="")


def _row_for(session_id, name, state, node):
    """A row keyed on a SESSION id, so a claim holder can resolve to it."""
    from fno.agents.watchdog import Row

    return Row(row_id=session_id, name=name, state=state, node=node, cwd="")


def test_a_dead_spawners_reservation_blocks_nothing(monkeypatch, tmp_path):
    """x-05be case 8. A queued spawn that never got a slot left a reservation
    that refused the relaunch for its whole TTL. spawn-cli:<pid> launches and
    exits, so it cannot come back and its TTL protects an empty slot."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim

    acquire_claim(
        "dispatch:N", "spawn-cli:99", ttl_ms=180_000, pid=_dead_pid(), root=tmp_path
    )
    # The handover holder is what makes the clear legitimate: this caller
    # replaces the reservation it removes with the node claim itself. The real
    # dispatch path (`fno agents spawn --node`) always passes one.
    verdict, exit_code = _spawn_guard_decision(
        "N", "spawn-cli:me", ttl="3m", handover_holder="spawn-handover:t-N"
    )
    assert verdict["verdict"] == "dispatchable", verdict
    assert exit_code == 0


def test_a_live_spawners_reservation_is_benign_dedup_with_no_remedy(
    monkeypatch, tmp_path
):
    """Somebody is mid-launch right now. Naming a force-release here would be
    worse advice than none."""
    _route_to(monkeypatch, tmp_path)
    import os

    from fno.claims.core import acquire_claim

    acquire_claim(
        "dispatch:N", "spawn-cli:other", ttl_ms=180_000, pid=os.getpid(), root=tmp_path
    )
    verdict, exit_code = _spawn_guard_decision(
        "N", "spawn-cli:me", ttl="3m", handover_holder="spawn-handover:t-N"
    )
    assert verdict == {"verdict": "already-running", "reason": "reservation-held"}
    assert exit_code == 0


def test_an_abandoned_node_claim_is_cleared_and_the_node_dispatches(
    monkeypatch, tmp_path
):
    """x-05be case 1. A worker died on a 429 and its claim refused its own
    respawn. Recovery took three manual steps; now it takes none."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:sid-dead", ttl_ms=3_600_000,
        pid=_dead_pid(), root=tmp_path,
    )
    assert claim_status("node:N", root=tmp_path)["state"] == "suspect"
    # The holder's OWN row, found and finished. Abandonment is proven by finding
    # the holder, never by failing to find it - and the row narrows the
    # candidates while the transcript decides, because the row state alone is
    # documented to call a working session done.
    _fake_roster(monkeypatch, rows=[_row_for("sid-dead", "t-dead", "done", "N")])
    monkeypatch.setattr(
        "fno.claims.cli._transcript_says_finished", lambda *_a, **_kw: True
    )

    verdict, exit_code = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["verdict"] == "dispatchable", verdict
    assert exit_code == 0


def test_a_holder_absent_from_the_roster_never_clears(monkeypatch, tmp_path):
    """The P1 regression guard at the dispatch site. A codex or opencode worker
    has no row in `claude agents --json`, so its absence proves nothing."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:sid-codex", ttl_ms=3_600_000,
        pid=_dead_pid(), root=tmp_path,
    )
    _fake_roster(monkeypatch, rows=[_row("t-someone-else", "working", "x-other")])

    verdict, _exit = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["verdict"] == "already-running"
    assert claim_status("node:N", root=tmp_path)["state"] == "suspect"


def test_a_live_worker_on_the_node_is_never_cleared(monkeypatch, tmp_path):
    """The x-ba4b regression guard at the dispatch site. Clearing this claim
    launches a second worker into a live session's worktree."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:sid-respawned", ttl_ms=3_600_000,
        pid=_dead_pid(), root=tmp_path,
    )
    _fake_roster(
        monkeypatch, rows=[_row_for("sid-respawned", "t-N-worker", "working", "N")]
    )

    verdict, exit_code = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["verdict"] == "already-running"
    assert verdict["reason"] == "suspect-claim"
    assert claim_status("node:N", root=tmp_path)["state"] == "suspect"
    assert exit_code == 0


def test_an_unprovable_wedge_refuses_and_names_the_way_out(monkeypatch, tmp_path):
    """x-05be case 3. Assert the command string, never the absence of a launch.
    When the roster cannot be read the operator is on their own, which is
    exactly when the refusal has to carry the remedy."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim

    acquire_claim(
        "node:N", "target-session:dead", ttl_ms=3_600_000, pid=_dead_pid(), root=tmp_path
    )
    _fake_roster(monkeypatch, rows=[], warnings=["claude not on PATH"])

    verdict, _exit = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["verdict"] == "already-running"
    assert "fno claim release node:N --force" in verdict["remedy"]
    assert "fno claim reap --apply" in verdict["remedy"]


def test_a_launch_window_holder_is_never_reported_as_a_wedge(monkeypatch, tmp_path):
    """The spawn-side claim carries the pid of `fno agents spawn`, which exits
    the moment it has forked the worker. So it reads SUSPECT for its whole TTL
    by construction, and a second dispatcher used to call a healthy in-flight
    launch a wedge and hand back force-release advice for it."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.cli import HANDOVER_HOLDER_PREFIX
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", f"{HANDOVER_HOLDER_PREFIX}t-N-worker", ttl_ms=900_000,
        pid=_dead_pid(), root=tmp_path,
    )
    assert claim_status("node:N", root=tmp_path)["state"] == "suspect"
    _fake_roster(monkeypatch, rows=[], warnings=["claude not on PATH"])

    verdict, exit_code = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["reason"] == "live-claim"
    assert "remedy" not in verdict
    assert exit_code == 0


def test_a_probe_reports_the_wedge_as_untried_and_offers_no_remedy(monkeypatch, tmp_path):
    """A probe takes no recovery, so it has nothing to say about whether the
    wedge clears. It says untried, and the shell callers read that and go on to
    the real spawn - which is the only path that can clear the claim. Handing
    back force-release advice here sent an operator to fix by hand a claim the
    launch would have cleared by itself."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim

    acquire_claim(
        "node:N", "target-session:dead", ttl_ms=3_600_000, pid=_dead_pid(), root=tmp_path
    )
    _fake_roster(monkeypatch, rows=[], warnings=["claude not on PATH"])

    verdict, _exit = _spawn_guard_decision(
        "N", "probe:me", ttl="3m", no_reserve=True
    )
    assert verdict["reason"] == "suspect-claim"
    assert verdict["recovery"] == "not-attempted"
    assert "remedy" not in verdict


def test_the_launch_path_still_names_the_way_out(monkeypatch, tmp_path):
    """The other half of the pair: on the path that DID try to recover and
    could not, the remedy is earned and must still be there."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim

    acquire_claim(
        "node:N", "target-session:dead", ttl_ms=3_600_000, pid=_dead_pid(), root=tmp_path
    )
    _fake_roster(monkeypatch, rows=[], warnings=["claude not on PATH"])

    verdict, _exit = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["reason"] == "suspect-claim"
    assert "recovery" not in verdict
    assert "fno claim reap --apply" in verdict["remedy"]


def test_a_blind_roster_never_clears_a_node_claim(monkeypatch, tmp_path):
    """The same run as above, stated as the safety property: an instrument that
    did not run must never authorize a clear."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:dead", ttl_ms=3_600_000, pid=_dead_pid(), root=tmp_path
    )
    _fake_roster(monkeypatch, rows=[], warnings=["registry unreadable"])
    _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert claim_status("node:N", root=tmp_path)["state"] == "suspect"


# ---------------------------------------------------------------------------
# spawn --node takes THE node claim, and the worker inherits it (x-cd1e)
# ---------------------------------------------------------------------------


def test_the_guard_claims_the_node_key_not_just_the_reservation(monkeypatch, tmp_path):
    """The measured defect: five workers spawned with an explicit --node and
    `fno claim status node:<id>` read free for every one of them."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import claim_status

    verdict, _exit = _spawn_guard_decision(
        "N", "spawn-cli:1", ttl="3m", handover_holder="spawn-handover:t-N"
    )
    assert verdict["verdict"] == "dispatchable"
    info = claim_status("node:N", root=tmp_path)
    assert info["state"] in ("live", "suspect"), info
    assert info["holder"] == "spawn-handover:t-N"


def test_a_probe_takes_no_claim(monkeypatch, tmp_path):
    """spawn.sh probes with --no-reserve before the real spawn. A probe that
    claimed would hand the node to a dispatcher that never launched."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import claim_status

    _spawn_guard_decision("N", "spawn-cli:1", no_reserve=True)
    assert claim_status("node:N", root=tmp_path)["state"] == "free"


def test_the_worker_inherits_the_claim_without_a_refusal(monkeypatch, tmp_path):
    """x-cd1e case 6. The handover must not abort the worker, and the key must
    never read free in between - the gap is the whole point of claiming early."""
    _route_to(monkeypatch, tmp_path)
    import os

    from fno.claims.cli import cli as claims_cli
    from fno.claims.core import claim_status

    _spawn_guard_decision(
        "N", "spawn-cli:1", ttl="3m", handover_holder="spawn-handover:t-N"
    )
    r = runner.invoke(
        claims_cli,
        [
            "acquire", "node:N",
            "--holder", "target-session:sid-1",
            "--ttl", "2h",
            "--pid", str(os.getpid()),
            "--handover-from", "spawn-handover:t-N",
        ],
    )
    assert r.exit_code == 0, r.output
    info = claim_status("node:N", root=tmp_path)
    assert info["holder"] == "target-session:sid-1"
    assert info["state"] == "live"


def test_a_wrong_handover_holder_cannot_take_a_live_claim(monkeypatch, tmp_path):
    """Naming the prior holder is the proof. Guessing wrong falls through to an
    ordinary acquire, which refuses a live foreign claim exactly as today."""
    _route_to(monkeypatch, tmp_path)
    import os

    from fno.claims.cli import cli as claims_cli
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:incumbent", ttl_ms=3_600_000,
        pid=os.getpid(), root=tmp_path,
    )
    r = runner.invoke(
        claims_cli,
        [
            "acquire", "node:N",
            "--holder", "target-session:intruder",
            "--ttl", "2h",
            "--handover-from", "spawn-handover:t-guessed",
        ],
    )
    assert r.exit_code == 1, r.output
    assert claim_status("node:N", root=tmp_path)["holder"] == "target-session:incumbent"


def test_handover_from_with_no_claim_on_disk_acquires_normally(monkeypatch, tmp_path):
    """Every hand-started run and every spawn whose claim failed lands here."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.cli import cli as claims_cli
    from fno.claims.core import claim_status

    r = runner.invoke(
        claims_cli,
        [
            "acquire", "node:N",
            "--holder", "target-session:sid-2",
            "--ttl", "2h",
            "--handover-from", "spawn-handover:t-never-was",
        ],
    )
    assert r.exit_code == 0, r.output
    assert claim_status("node:N", root=tmp_path)["holder"] == "target-session:sid-2"


def test_a_launch_window_claim_is_never_probed_as_abandoned(monkeypatch, tmp_path):
    """The collision between claiming early and reaping the abandoned: between
    spawn and target init the worker has no manifest and no ledger row, so the
    roster CANNOT see it. Probing anyway would clear the claim out from under
    the worker it was taken for."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.cli import _abandonment_probe
    from fno.claims.core import claim_status
    from fno.claims.io import claim_path, read_claim_file

    _spawn_guard_decision(
        "N", "spawn-cli:1", ttl="3m", handover_holder="spawn-handover:t-N"
    )

    def _boom(*_a, **_kw):
        raise AssertionError("roster consulted for a launch-window claim")

    monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _boom)
    claim = read_claim_file(claim_path("node:N", root=tmp_path))
    assert _abandonment_probe()(claim) is None

    # And the guard therefore refuses to clear it.
    verdict, _exit = _spawn_guard_decision("N", "spawn-cli:2", ttl="3m")
    assert verdict["verdict"] == "already-running"
    assert claim_status("node:N", root=tmp_path)["holder"] == "spawn-handover:t-N"


def test_a_handover_succeeds_even_while_the_spawner_is_still_alive(monkeypatch, tmp_path):
    """`--substrate headless` and `--once` block in dispatch_spawn for the
    worker's whole run, so the spawn-side pid is LIVE when the worker inits.
    Refusing there left the worker unclaimed for the full lease, which is the
    free read this change exists to close, on the one substrate that blocks."""
    _route_to(monkeypatch, tmp_path)
    import os

    from fno.claims.cli import cli as claims_cli
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "spawn-handover:t-N", ttl_ms=900_000,
        pid=os.getpid(), root=tmp_path,
    )
    assert claim_status("node:N", root=tmp_path)["state"] == "live"

    r = runner.invoke(
        claims_cli,
        [
            "acquire", "node:N",
            "--holder", "target-session:sid-h",
            "--ttl", "2h",
            "--pid", str(os.getpid()),
            "--handover-from", "spawn-handover:t-N",
            "--reason", "target dispatch",
        ],
    )
    assert r.exit_code == 0, r.output
    info = claim_status("node:N", root=tmp_path)
    assert info["holder"] == "target-session:sid-h"
    assert info["reason"] == "target dispatch"


def test_a_live_claim_still_refuses_a_same_holder_concurrent_writer(monkeypatch, tmp_path):
    """The handover relaxation must not weaken the concurrent-writer rule it
    sits beside: two processes of ONE symbolic owner still refuse."""
    _route_to(monkeypatch, tmp_path)
    import os

    from fno.claims.core import RebindRefused, acquire_claim, compare_and_rebind

    acquire_claim(
        "node:N", "target-session:same", ttl_ms=900_000,
        pid=os.getpid(), root=tmp_path,
    )
    try:
        compare_and_rebind(
            "node:N", "target-session:same", new_pid=os.getpid() + 1,
            ttl_ms=900_000, root=tmp_path,
        )
    except RebindRefused as exc:
        assert "concurrent writer" in str(exc)
    else:
        raise AssertionError("a live same-holder claim must refuse a second pid")


def test_a_no_reserve_probe_performs_no_recovery(monkeypatch, tmp_path):
    """dispatch-node.sh probes once per node across a whole batch. A probe that
    archives claims and emits events is a side effect nobody expects."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:sid-dead", ttl_ms=3_600_000,
        pid=_dead_pid(), root=tmp_path,
    )
    _fake_roster(monkeypatch, rows=[_row_for("sid-dead", "t-dead", "done", "N")])

    verdict, _exit = _spawn_guard_decision("N", "spawn-cli:me", no_reserve=True)
    assert verdict["verdict"] == "already-running"
    assert claim_status("node:N", root=tmp_path)["state"] == "suspect"


def test_the_bare_guard_verb_never_clears_a_barrier_it_cannot_replace(
    monkeypatch, tmp_path
):
    """`fno agents spawn-guard` takes no handover holder, so it never takes the
    node claim. Clearing a dead spawner's reservation there removed a booting
    worker's only barrier and put nothing in its place. The clear is legitimate
    only for the caller that replaces it."""
    _route_to(monkeypatch, tmp_path)
    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "dispatch:N", "spawn-cli:99", ttl_ms=180_000, pid=_dead_pid(), root=tmp_path
    )
    verdict, exit_code = _spawn_guard_decision("N", "spawn-cli:me", ttl="3m")
    assert verdict["verdict"] == "already-running"
    assert verdict["reason"] == "reservation-held"
    # The positive marker: the reservation is still on disk, holder unchanged.
    assert claim_status("dispatch:N", root=tmp_path)["holder"] == "spawn-cli:99"
    assert exit_code == 0


def test_a_held_node_refuses_and_hands_back_its_own_reservation(monkeypatch, tmp_path):
    """A session that claimed the node through its own `fno target init` is
    invisible to the reservation, which only dedups other DISPATCHERS. Swallowing
    that as a best-effort hiccup put a second worker on a live session's node.

    And the refusal must not keep the reservation it just took: nothing
    downstream releases it, and a `dispatch:` key is unreapable inside its TTL,
    so it would block every dispatcher for the full three minutes."""
    _route_to(monkeypatch, tmp_path)
    import os

    from fno.claims.core import acquire_claim, claim_status

    acquire_claim(
        "node:N", "target-session:someone-else", ttl_ms=3_600_000,
        pid=os.getpid(), root=tmp_path,
    )
    verdict, exit_code = _spawn_guard_decision(
        "N", "spawn-cli:me", ttl="3m", handover_holder="spawn-handover:t-N"
    )
    assert verdict["verdict"] == "already-running"
    assert verdict["reason"] == "live-claim"
    assert exit_code == 0
    assert (
        claim_status("node:N", root=tmp_path)["holder"] == "target-session:someone-else"
    )
    assert claim_status("dispatch:N", root=tmp_path)["state"] == "free"
