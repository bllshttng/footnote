"""Provider adapters for fno agents (Phase 1 substrate).

Each provider module (claude, codex, gemini) lands in its own file in
Phase 2 onward. Phase 1 only ships the shared ``ProviderResult``
dataclass plus the dispatch-layer availability detection.
"""

# Providers Python can DISPATCH (select_provider + availability checks).
# Phase 2 provider modules slot into this tuple as they land.
KNOWN_PROVIDERS: tuple[str, ...] = ("claude", "codex", "gemini")

# Providers the registry loader will ACCEPT ON READ. Superset of the
# dispatchable set: `agy` (Antigravity, binary `ln`) is spawned Rust-side
# only (Phase C stateless, no Python adapter), but Rust writes provider="agy"
# rows, so load_registry must tolerate them or every Python consumer
# (spawn collision check, mail send, discuss dispatch) hard-fails rc=12 on
# a single agy row. Mirrors Rust's KNOWN_PROVIDERS in client_verbs.rs. This
# stays an enumeration (not blanket tolerance) to keep the corruption guard.
READABLE_PROVIDERS: tuple[str, ...] = KNOWN_PROVIDERS + ("agy",)
