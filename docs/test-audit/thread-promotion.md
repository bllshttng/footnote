# Thread promotion audit

The first pass measured the existing CLI substrate and spawn-gate owners before rework. The second pass checked the migrated decision owners, carrier boundaries, fixtures, and overlapping coverage after rework. Local baseline verification returned 16 passed and 13 skipped. The native cases skipped because this checkout had no built runtime. A subsequent Cargo attempt stopped at fleet admission generation 156 and was cancelled. That attempt supplies no build or test verdict.

## Owners and cuts

`cli/tests/agents/test_crown_bg_substrate.py` owns CLI admission, persistent carrier promotion, grantor provenance, duplicate refusal, one-shot refusal, and persisted lead manifests. Its persistent-carrier case now covers Codex, OpenCode, and pi. The Codex transport must carry promotion fields at mint. The pi seed callback reads the promoted row before submitting its payload. OpenCode uses its actual non-UUID identity shape and expects the existing manifest writer's explicit unarmed receipt.

The obsolete unsupported-carrier test was removed because persistent carriers now support promotion. The duplicate headless wording invocation was folded into the existing headless refusal owner. `TestReignTyped` in `cli/tests/agents/test_spawn_gate.py` and private validator tests in `cli/tests/agents/test_crown.py` were removed with their Python decision legs. The Rust owner retains those contracts through frozen characterization cases. The remaining gate tests protect admission and failure feedback. Invalid writer cases require a worktree runtime so an unavailable owner cannot masquerade as a successful negative control.

`crates/fno-agents/src/team_spawn.rs` owns seed construction, validation, registry effects, journal facts, and receipts. Its characterization reads 26 seed, validation, and journal cases captured by executing the archived Python functions at the fixture's recorded source SHA. The capture uses an in-memory event sink and project facts and writes only the fixture. It uses no live registry, graph, or manifest. Its identity-race test guards a distinct write-boundary risk: a rebound heir must not vacate the predecessor. Existing settlement coverage in `cli/tests/agents/test_crown.py` retains holder races, terminal cleanup, child reownership, and unavailable-runtime refusal.

`cli/tests/agents/test_dispatch.py` retains public result fields, availability, provider selection, mismatch refusal, and legacy-row reconciliation. `cli/src/fno/adapters/providers/test_dispatch.py` retains credential resolution, staging, concurrency, and route isolation. Those contracts do not duplicate promotion.

## Authoring gate

The observable contracts are promotion through each persistent carrier, promotion before keeper seed submission, correct receiving-harness seed spelling, payload preservation, and identity-bound settlement. Credible regressions include a dropped promotion field, premature seed submission, an incorrect sigil, payload trimming, and a reused name receiving another session's role. Previous coverage exercised Codex alone and did not protect keeper ordering or the heir identity pair. Tests use existing launch and submission boundaries. The port adds no production seam solely for testing.

## Verification

Run `cargo test --manifest-path crates/fno-agents/Cargo.toml --lib team_spawn::tests`, build the worktree runtime, and run `fno doctor test cli/tests/agents/test_crown_bg_substrate.py cli/tests/agents/test_spawn_gate.py cli/tests/agents/test_crown.py`. Cargo and the native CLI cases remain unverified locally while the fleet hold is active. Changed-file Ruff, mypy, Rust formatting, whitespace, and seam checks passed before delivery preparation.
