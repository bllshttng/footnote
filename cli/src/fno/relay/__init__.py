"""Cross-session agent relay (epic x-908b -> inside-out E4).

After the E4.3 relay-unification capstone, the relay is a transcript-reader + RPC
client: the Rust daemon owns the interactive-claude PTY, and the relay injects via
the daemon ``worker.submit`` RPC (routed behind the daemon-held ``session:`` claim)
and reads replies from the transcript jsonl. The persistent registry/router (G2)
and the always-on daemon + envelope (G3) are unchanged; the retired PTY vehicle
(``spawn_peer`` / ``os.openpty`` / ``peer.buf``) is gone.
"""
from fno.relay.envelope import (
    frame, frame_envelope, is_framed, make_relay_envelope, parse,
)
from fno.relay.registry import (
    RegistryEntry, index, register, transcript_path_for, unregister,
)
from fno.relay.roundtrip import (
    deliver_session, resolve_worker_short_id, submit_via_worker,
)
from fno.relay.router import Address, Resolution, Unroutable, parse_address, resolve

# daemon_deliver lives in fno.relay.daemon and is intentionally NOT re-exported
# here: importing it pulls the bus/claims/events graph onto every `import
# fno.relay`. Consumers (the daemon entrypoint, tests) import it directly.
__all__ = [
    "deliver_session", "resolve_worker_short_id", "submit_via_worker",
    "RegistryEntry", "index", "register", "unregister", "transcript_path_for",
    "Address", "Resolution", "Unroutable", "parse_address", "resolve",
    "frame", "parse", "is_framed", "frame_envelope", "make_relay_envelope",
]
