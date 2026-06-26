"""Cross-session agent relay (epic x-908b).

Group 1 (this module set): a proven human-out-of-loop round-trip between two
autonomous interactive `claude` sessions over PTY send-keys. Later groups add
the persistent registry/router (G2), the always-on daemon + envelope (G3), and
cross-provider PTY injection (G4).
"""
from fno.relay.roundtrip import Peer, RoundTrip, close_peer, round_trip, spawn_peer

__all__ = ["Peer", "RoundTrip", "round_trip", "spawn_peer", "close_peer"]
