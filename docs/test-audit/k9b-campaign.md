# K9b campaign ledger - the never-folded Rust files

The next bounded packet in the Rust-suite cut (kestrel-successor msg-566190,
keep rule d-a1b9af5a). Base origin/main post-K9: 2,433 declarations
crate-wide. Eight files that no earlier campaign touched.

## Method

Same family folds as K8/K9: every merged test keeps a row (body moved
verbatim) for each distinct branch its absorbed siblings guarded. No
parametrize, no deleted asserts. Fail-closed refusals survive as rows.

New hazard class found and honored this packet: serializing test gates.
`pty.rs` holds a tokio `PTY_GATE` across twelve tests; merging two
gate-taking tests deadlocks the merged fn, so those twelve stay
standalone. `server_restore_tests.rs` orders each family so guard-free
bodies run before policy-setting bodies, so an inherited override can
never shift a row's semantics (the lifecycle keeper failure taught
this: a Hold policy leaked into an absorbed body that expected the
ambient default).

## Files folded

| File | Before | After | Keeper families |
|---|---|---|---|
| src/server/tests/server_restore_tests.rs | 39 | 12 | member cwd, refusal, tombstone policy, retire/prune lifecycle, hold seating, degenerate stores, workspace-restore members, workspace-restore portals, unnamed lanes |
| src/client/tests/backlog_board_tests.rs | 37 | 16 | board render, wide layout, t key, sideline toggles, chords, panel cells, compose, ux shots, edit keys, pickers, facets |
| src/team_overlay.rs | 33 | 11 | parse, glance, expanded fleet, census, refusal, census degrade, fold failure, ttl, panel lifecycle |
| src/client/questions.rs | 33 | 11 | block layout, page, detail nav, answer keys, submit, detail keys nav, toggle toast, apply result, block hit |
| src/proto.rs | 29 | 8 | backlog verb baseline, codec roundtrip, agent-row backcompat, placement backcompat, wire skew, reader, socket |
| src/digest_overlay.rs | 29 | 14 | theme overrides, config reader, config layer, key reader, json lines, user theme |
| src/pty.rs | 29 | 18 | keeper shape, keeper argv, fd limit, fd ceiling, shell integration, zsh hop (the 12 PTY_GATE tests stay standalone) |
| src/client_tests/feed_view_tests.rs | 29 | 8 | render, hit, click, width, header, detail fields |

Packet total: -139 (5.7% of the 2,433 suite).

## Kept unmerged

The pty wedge/cooldown tests (gate serialization), the incident-cited
singles in each file, the ignored locate-tier portal test, and the two
server-restore scenario tests that already merge their own branches.
The keep rule stops each file at its honest floor.

## Verification

Scoped green lanes per file on the file's own module path (the
`server::tests::server_restore_tests::` lesson: filename-based filters
match zero tests when the mod is nested). Lane counts match the table:
restore 11+1 ignored, board 16, org 11, questions 11, pty 18,
proto 29 with its nested file modules, digest 14, feed 8. Assert
proofs per family: 564 of 564 asserts kept across the six later files,
133 of 133 across restore, 89 of 89 across board. Duplicate body-local
`use` lines removed where a family merged (E0252 class). Three
questions keepers hold tokio families: the merged fn is async and the
keeper's own `#[tokio::test]` line heads it.
