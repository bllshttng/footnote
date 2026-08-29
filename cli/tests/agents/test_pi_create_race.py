"""The pi CREATE race, and the claim that serialises it (x-c198).

The defect these tests pin is one where EVERY COMPONENT SUCCEEDS AND THE ANSWER
IS WRONG. Four simultaneous ``pi --session-id <same>`` creates produced four
session files 49ms apart. All four exited 0. All four printed "creating a new
session with that id". Every file was internally perfect: zero duplicate
parentIds, zero orphans, zero malformed lines. Nothing was contended, so nothing
was damaged. Then a later resume of that id picked the OLDEST, wrote no fifth
file, emitted no warning, and left three sessions holding real work unreachable
by the only handle fno has for them.

So **asserting that no error occurred proves nothing here**: no error occurring
is the whole defect. Every assertion below is a POSITIVE marker - exactly one
winner recorded, three refusals naming that winner, one id every later attach
resolves to.

And the assertion is against **fno's own claim registry, not pi's files**.
During the race pi's files do not exist: a session's file materialises at the
first turn ATTEMPT, so a file count reads zero for any number of racing creates.
A claim is written at acquire, which is why it covers exactly the window the
file instrument cannot see.
"""
from __future__ import annotations

import sys
import threading
from pathlib import Path

import pytest

_TEST_DIR = Path(__file__).resolve().parent
_SRC = _TEST_DIR.parents[1] / "src"
if str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))

from fno.agents.harnesses.pi import (  # noqa: E402
    CREATE_CLAIM_TTL_MS,
    PiCreateHeld,
    create_claim_key,
    create_decision,
    lookup_sessions,
)


@pytest.fixture
def pi_home(tmp_path, monkeypatch):
    """An isolated pi session store."""
    home = tmp_path / "pi-home"
    monkeypatch.setenv("PI_HOME", str(home))
    return home


def _seed_session_file(pi_home: Path, cwd: str, session_id: str, stamp: str) -> Path:
    from fno.agents.harnesses.pi import encode_cwd

    directory = pi_home / "agent" / "sessions" / encode_cwd(cwd)
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{stamp}_{session_id}.jsonl"
    path.write_text('{"role": "user"}\n')
    return path


def test_AC5_HP_four_way_create_yields_exactly_one_winner(tmp_path, pi_home):
    """AC5-HP: four simultaneous creates, one winner, three joiners.

    The winner is the process that took the claim. The other three are told who
    holds it and then JOIN, which is the half that was measured safe: a second
    process on an existing session appends coherently to one parentId chain.
    """
    claims_root = tmp_path / "claims"
    cwd = "/repo/worktrees/pi-race"
    session_id = "fno-race-0001"

    roles: list[str] = []
    barrier = threading.Barrier(4)
    lock = threading.Lock()

    def racer(index: int) -> None:
        barrier.wait()
        with create_decision(
            cwd,
            session_id,
            holder=f"racer-{index}",
            claims_root=claims_root,
            wait_timeout_s=30.0,
            poll_s=0.01,
        ) as decision:
            if decision.role == "create":
                # The winner is the one that creates the session, and only
                # after it exists may anyone else be pointed at that id.
                _seed_session_file(pi_home, cwd, session_id, "2026-08-28T20-58-10-768Z")
            with lock:
                roles.append(decision.role)

    threads = [threading.Thread(target=racer, args=(i,)) for i in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=60)

    assert len(roles) == 4, f"every racer must reach a decision, got {roles}"
    assert roles.count("create") == 1, f"exactly ONE creator, got {roles}"
    assert roles.count("join") == 3, f"three joiners, got {roles}"

    # The positive marker on the store: one session, not four.
    lookup = lookup_sessions(cwd, session_id)
    assert lookup.state == "one", f"one id, one session; got {lookup}"


def test_AC5_HP_the_refusal_names_the_holder(tmp_path, pi_home):
    """A create that cannot wait is refused with the HOLDER named.

    A timeout that reports a clock while the real condition is "another process
    holds this" is the diagnostic seam this lane exists to avoid.
    """
    from fno.claims.core import acquire_claim, release_claim

    claims_root = tmp_path / "claims"
    cwd = "/repo/worktrees/pi-held"
    session_id = "fno-held-0001"
    key = create_claim_key(cwd, session_id)
    acquire_claim(key, "the-winner", ttl_ms=CREATE_CLAIM_TTL_MS, root=claims_root)
    try:
        with pytest.raises(PiCreateHeld) as caught:
            with create_decision(
                cwd,
                session_id,
                holder="the-loser",
                claims_root=claims_root,
                wait_timeout_s=0.0,
                poll_s=0.01,
            ):
                pass
    finally:
        release_claim(key, "the-winner", root=claims_root)

    message = str(caught.value)
    assert "the-winner" in message, message
    assert caught.value.holder == "the-winner"
    assert "pid=" in message, message


def test_AC5_EDGE_the_blind_window_is_covered_by_the_claim_not_by_files(
    tmp_path, pi_home
):
    """AC5-EDGE: zero session files, and fno still knows the session exists.

    A pi session's file appears at the first turn ATTEMPT. A live rpc session
    with no prompt sent leaves the directory EMPTY, so a file count measures
    nothing in exactly the window the race lives in. This asserts the two
    instruments disagree in the expected direction: the store says it cannot
    see a session, and the claim registry says one is being created.
    """
    from fno.claims.core import claim_status

    claims_root = tmp_path / "claims"
    cwd = "/repo/worktrees/pi-blind"
    session_id = "fno-blind-0001"

    with create_decision(
        cwd, session_id, holder="creator", claims_root=claims_root
    ) as decision:
        assert decision.role == "create"
        # No turn attempted, so pi has written nothing at all.
        assert lookup_sessions(cwd, session_id).state in {"none", "unknown"}
        status = claim_status(create_claim_key(cwd, session_id), root=claims_root)
        assert status.get("holder") == "creator", status


def test_the_claim_key_is_a_session_key_carrying_cwd():
    """The key names a SESSION, not a node, and two worktrees never contend.

    Called out because a reviewer meeting a `claim acquire` call rightly asks
    whether this is the forbidden manual NODE claim. It is not: `node:` is a
    convention in the claim key space, not a schema, and this key lives in a
    different space, is taken by the spawn lane rather than by a person, and is
    released in the same operation.
    """
    key = create_claim_key("/repo/worktrees/one", "s-1")
    assert key.startswith("pi-session:"), key
    assert not key.startswith("node:"), key
    assert key != create_claim_key("/repo/worktrees/two", "s-1")


def test_a_failed_create_releases_the_claim_instead_of_leaking_the_ttl(
    tmp_path, pi_home
):
    """A create that raises must not lock the id for the rest of the TTL.

    This is why the scope is the create DECISION and not the session lifetime:
    an explicit-TTL claim survives its holder's death for the whole TTL, so a
    crash inside a long-held claim would make that session id unusable until it
    expired.
    """
    from fno.claims.core import claim_status

    claims_root = tmp_path / "claims"
    cwd = "/repo/worktrees/pi-boom"
    session_id = "fno-boom-0001"
    with pytest.raises(RuntimeError, match="create blew up"):
        with create_decision(
            cwd, session_id, holder="creator", claims_root=claims_root
        ):
            raise RuntimeError("create blew up")
    status = claim_status(create_claim_key(cwd, session_id), root=claims_root)
    assert not status.get("holder"), f"claim leaked after a failed create: {status}"
