"""Wave 2 tests: the batch policy engine (pure decision functions).

`decide_batch_action` answers "ship this node solo, join the open batch, or
start a new one?"; `should_close` answers "close the open batch now?". Both are
pure over (node dict, open-batch dict, config) so the live selection path can
consult them without behavior risk when the flag is off.
"""
from __future__ import annotations

from pathlib import Path

from fno.backlog import batch as B


def _node(domain="code", size=None, priority="p2", nid="x-1") -> dict:
    return {"id": nid, "domain": domain, "size": size, "priority": priority}


def _open(root: Path, domain="code", max_nodes=3) -> dict:
    return B.open_batch(domain=domain, branch="f", worktree="w",
                        max_nodes=max_nodes, root=root)


# --- decide_batch_action ---------------------------------------------------


def test_disabled_always_ships_solo(tmp_path: Path) -> None:
    d = B.decide_batch_action(_node(), enabled=False, root=tmp_path)
    assert d.action == "ship_solo"


def test_no_open_batch_starts(tmp_path: Path) -> None:
    d = B.decide_batch_action(_node(), enabled=True, root=tmp_path)
    assert d.action == "start"
    assert d.domain == "code"


def test_open_joinable_batch_joins(tmp_path: Path) -> None:
    _open(tmp_path)
    d = B.decide_batch_action(_node(), enabled=True, root=tmp_path)
    assert d.action == "join"


def test_full_batch_starts_new(tmp_path: Path) -> None:
    _open(tmp_path, max_nodes=1)
    B.join_batch(domain="code", node_id="x-0", root=tmp_path)
    d = B.decide_batch_action(_node(), enabled=True, root=tmp_path)
    assert d.action == "start"


def test_size_L_ships_solo(tmp_path: Path) -> None:
    _open(tmp_path)
    d = B.decide_batch_action(_node(size="L"), enabled=True, root=tmp_path)
    assert d.action == "ship_solo"


def test_p0_ships_solo(tmp_path: Path) -> None:
    _open(tmp_path)
    d = B.decide_batch_action(_node(priority="p0"), enabled=True, root=tmp_path)
    assert d.action == "ship_solo"


def test_size_case_insensitive(tmp_path: Path) -> None:
    d = B.decide_batch_action(_node(size="l"), enabled=True, root=tmp_path)
    assert d.action == "ship_solo"


# --- should_close ----------------------------------------------------------


def test_close_when_full(tmp_path: Path) -> None:
    _open(tmp_path, max_nodes=1)
    B.join_batch(domain="code", node_id="x-0", root=tmp_path)
    batch = B.read_batch("code", tmp_path)
    close, _ = B.should_close(batch, _node(nid="x-2"))
    assert close is True


def test_close_on_drain_no_next(tmp_path: Path) -> None:
    _open(tmp_path)
    batch = B.read_batch("code", tmp_path)
    close, reason = B.should_close(batch, None)
    assert close is True
    assert "drain" in reason


def test_close_on_domain_switch(tmp_path: Path) -> None:
    _open(tmp_path, domain="code")
    batch = B.read_batch("code", tmp_path)
    close, _ = B.should_close(batch, _node(domain="research"))
    assert close is True


def test_close_on_next_size_L(tmp_path: Path) -> None:
    _open(tmp_path)
    batch = B.read_batch("code", tmp_path)
    close, _ = B.should_close(batch, _node(size="L"))
    assert close is True


def test_close_on_next_p0(tmp_path: Path) -> None:
    _open(tmp_path)
    batch = B.read_batch("code", tmp_path)
    close, _ = B.should_close(batch, _node(priority="p0"))
    assert close is True


def test_stays_open_same_domain_small_node(tmp_path: Path) -> None:
    _open(tmp_path)
    batch = B.read_batch("code", tmp_path)
    close, _ = B.should_close(batch, _node())
    assert close is False


def test_close_on_max_loc(tmp_path: Path) -> None:
    _open(tmp_path)
    batch = B.read_batch("code", tmp_path)
    close, _ = B.should_close(batch, _node(), max_loc=100, cum_loc=150)
    assert close is True


def test_no_open_batch_never_closes(tmp_path: Path) -> None:
    close, _ = B.should_close(None, _node())
    assert close is False
