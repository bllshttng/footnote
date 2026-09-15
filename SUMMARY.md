# SUMMARY - the answerer gate walks each changed symbol into every language tree

## Deviations from the plan

- The plan said "wire it like kill-check", which is a routable `fno agents` verb with a Python help entry in `cli/src/fno/agents/rust_runtime.py`. Routing surface-check would contradict the plan's own "no new user verb" and "cli/src/fno has no change" constraints, so the verb is binary-first like `canonical-check`: a plain `==` dispatch arm plus one exclusion in `test_rust_client_verbs_match_client_rs` (cli/tests/agents/test_rust_runtime.py) so the parity guard keeps it out of `RUST_CLIENT_VERBS`. The Python CLI surface is unchanged.
- AC3-ERR's graceful path grew one U line the heredoc could not emit (a non-mapping frontmatter document crashed Python and the shell warned "failed to run"); the Rust verb prints `U frontmatter is not a YAML mapping` instead. Same shell outcome, no crash.
- The AC7a semantic fixture gained an `at:` symbol token (`_semantic_validate`) so the new no-symbol warning does not fire on it; the walk's same-language drop makes the fixture walk-clean.
- `git grep -o` prints no line numbers, so the hit parser splits `sha:path:match` (two fields), not `sha:path:line:match`.

## Measured facts for the PR body

- Differential over /Users/bb16/c3po/internal/fno/plans (origin/main heredoc vs surface-check): E/O/U lines identical except U-line YAML-error WORDING on malformed frontmatter (PyYAML vs serde_yaml_ng describe the same failure differently; both are U, both NOT CHECKED). Counts recorded by the differential run (logged in the session).
- AC5-HP replayed: the x-6418 plan at base 2742c122 produces `X is_open_phase_row ... crates/fno-agents/src/graph_keeper.rs, crates/fno-agents/src/graph_store.rs`, exactly the readers the review round found by hand.
- The plan itself validates through the bundle copy with receipt `cross-language walk at ...: 2 symbol(s), readers by tree shell 3; 0 unlisted`.
