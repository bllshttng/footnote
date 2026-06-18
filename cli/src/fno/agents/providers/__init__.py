"""Provider adapters for fno agents (Phase 1 substrate).

Each provider module (claude, codex, gemini) lands in its own file in
Phase 2 onward. Phase 1 only ships the shared ``ProviderResult``
dataclass plus the dispatch-layer availability detection.
"""

# Single source of truth for supported provider names. Imported by both
# registry.load_registry (for load-time validation) and dispatch (for
# select_provider + availability checks). Phase 2 provider modules slot
# into this tuple as they land.
KNOWN_PROVIDERS: tuple[str, ...] = ("claude", "codex", "gemini")
