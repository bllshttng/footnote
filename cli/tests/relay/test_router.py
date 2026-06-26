"""Group 2 (x-e4ac / US2 / AC2): the address router (parseAddress clone)."""
from __future__ import annotations

import pytest

from fno.relay.registry import RegistryEntry
from fno.relay.router import (
    Address, Resolution, Unroutable, parse_address, resolve,
)


def _idx():
    return {
        "B": RegistryEntry(session_id="B", provider="claude", pid=42,
                           inject_handle="pty:42", name="bob"),
    }


# ---- parse_address ----------------------------------------------------------

def test_parse_session_node_name():
    assert parse_address("session:B") == Address("session", "B")
    assert parse_address("node:fno-a3f9") == Address("node", "fno-a3f9")
    assert parse_address("bob") == Address("name", "bob")
    assert parse_address("  session:B  ") == Address("session", "B")


def test_parse_rejects_empty_and_dangling_prefix():
    with pytest.raises(ValueError):
        parse_address("")
    with pytest.raises(ValueError):
        parse_address("session:")  # a typo must not become a bare-name lookup


# ---- resolve ----------------------------------------------------------------

def test_resolve_session_to_handle_and_provider():
    # AC2-HP: resolve session:<B> to its inject handle + provider.
    r = resolve("session:B", index=_idx())
    assert r == Resolution(session_id="B", provider="claude", inject_handle="pty:42")


def test_resolve_unknown_session_is_unroutable():
    with pytest.raises(Unroutable, match="relay_unroutable"):
        resolve("session:ghost", index=_idx())


def test_resolve_node_via_resolver():
    r = resolve("node:fno-a3f9", index=_idx(), node_resolver=lambda nid: "B")
    assert r.session_id == "B" and r.inject_handle == "pty:42"


def test_resolve_node_unresolved_is_unroutable():
    with pytest.raises(Unroutable, match=r"relay_unroutable\{node:fno-x\}"):
        resolve("node:fno-x", index=_idx(), node_resolver=lambda nid: None)


def test_resolve_name():
    assert resolve("bob", index=_idx()).session_id == "B"


def test_resolve_unknown_name_is_unroutable():
    with pytest.raises(Unroutable):
        resolve("nobody", index=_idx())
