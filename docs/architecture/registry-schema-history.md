# Registry schema history: v4 through v16

The registry schema version lives in one place (`crates/fno-agents/src/registry_schema.toml`, projected to `cli/src/fno/agents/registry_schema.toml` by build.rs). Versions from v17 on are documented beside the version constant in `cli/src/fno/agents/registry.py` and beside the `fields` array in the toml. This page keeps the early history so the Python module stays lean.

## v4 - v16

```
# v4 is the host_mode forward-compat bump. v5 (inside-out E3.1) is
# the same kind of bump for the additive `inside_leg` field: structurally
# identical to v4 (inside_leg is additive-optional, an absent key reads as None),
# but stamping v5 makes a pre-inside-leg reader (which accepts only {1,2,3,4})
# reject a v5 store instead of silently dropping the inside-leg report on
# write-back. Reads stay backward-compatible: load_registry accepts
# 1..=SCHEMA_VERSION. v6 (4a-G2) is the mux-ref bump; v7 (screen-manifest
# fallback authority) the same bump for the additive `screen_state` verdict.
# v8 is the canonical-identity bump for `harness` / `harness_session_id`:
# every Python-authored row emits these keys, so a pre-v8 reader must REJECT the
# store (clean "upgrade fno") rather than accept the version and then TypeError on
# the unknown AgentEntry kwargs (the PR #364 brick) or silently drop the fields on
# a Rust read-modify-write. Same forward-compat rationale as the v4-v7 bumps.
# v9 removes `claude_short_id`: the claude jobId (a pure prefix of the session
# UUID) now lives in `short_id`, unifying the transport-key field across
# providers. Legacy rows backfill on load (see load_registry); a pre-v9 reader
# must reject a v9 store rather than drop the jobId on write-back.
# v10 removes the on-disk `provider` field and the legacy per-provider
# session-id trio (`codex_session_id`, `gemini_session_id`, `claude_session_uuid`):
# `harness` is the sole identity axis and `harness_session_id` the sole session
# id. A legacy row's `provider` back-fills `harness`, and each per-provider key
# back-fills `harness_session_id`, at load (the accept-on-read pattern) and the
# key dies there. A pre-v10 reader must reject a v10 store rather than mis-read
# a harness-only row.
# v11 (US9): additive crown fields (crown_level/crown_scope/crown_grantor).
# asdict emits them as null on every written row, so a pre-v11 reader must
# reject the store rather than TypeError on the unknown keys.
# v12: additive `route_settings_path` - the route-settings file a
# routed worker was LAUNCHED with, so a relaunch can re-apply it instead of
# silently coming back on the default account. Same additive-optional shape and
# same forward-compat rationale as v11: asdict emits the key on every written
# row, so a pre-v12 reader must reject the store rather than TypeError on it.
# v13: additive `fno_id` - footnote's own session id, a random UUID minted
# at the row's first write, separate from `harness_session_id`, which names the
# harness conversation beside it. Rows written before the mint keep the value
# they hold (a harness copy, short id, or name). Same additive-optional shape
# and forward-compat rationale as v12.
# v14: additive `delivery_policy` - a recipient's mail delivery
# policy ("bus-only": never prompt-line inject, always durable bus). Same
# additive-optional shape and same forward-compat rationale as v12/v13: asdict
# emits the key on every written row, so a pre-v14 reader must reject the
# store rather than TypeError on the unknown kwarg.
# v15 restores `provider` with its literal model-provider meaning. It is
# intentionally distinct from `harness`: a routed worker can have
# harness="claude" and provider="zai", while "opencode" is valid on either
# axis. Rows from v1..v14 retain the legacy meaning where `provider` was a
# harness alias and are migrated only while reading those schema versions.
# v16: `origin` and `spawn_trigger` gain their Rust counterparts in
# `RegistryEntry`. Python has written both for releases; Rust never modelled
# them, so every Rust write re-serialized the row from its typed struct and
# dropped the keys. Measured 2026-08-20: 0 of 37 live rows carried either. The
# bump is not for a new Python field - it is what turns a pre-v16 binary's
# SILENT erasure into a loud refusal, the same reason v11-v14 bumped.
```