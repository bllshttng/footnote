# K9 campaign ledger - the Rust giants (client/server suites)

Re-aimed at the Rust suites per the operator ruling on q-061106b1: the two
largest files in crates/fno. Base origin/main at the branch cut (3,013
declarations crate-wide). Cut: client_tests.rs 481 to 357 (-124),
server_tests.rs 273 to 174 (-99). K9 total: -223 (7.4% of the suite).

## Method

Same family folds as K8: every merged test keeps a row (body moved
verbatim) for each distinct branch its absorbed siblings guarded. No
parametrize, no deleted asserts. Fail-closed refusals survive as rows.

## Files folded

| File | Before | After | Keeper families |
|---|---|---|---|
| src/client_tests.rs | 481 | 357 | seam drag/accents/refusals, xf331 confirm, overlay draw, seam addressing, sideline drag, link hover, hover focus/arm, chrome hit, squad expand, idle fold, cycle, persisted state, status row, esc chips, name modal, carets, bands, tab badge, selector fold/nav, wheel, pull, density, sort, humanize, attention, peek, settings, menu accelerators, row menu, reaper, update modal, overlay chrome |
| src/server_tests.rs | 273 | 174 | tab/pane labels, attach argv, mouse routing, tab close, pane send identity, tombstones, rename tab, placement, templates + refusals/lifecycle, break/join, tab location, capacity, graft, reanchor, move/join, squad rename, depersist, reorder, resume, resume agent, mail, external, unbound |

The tab-menu family had already been split to its own file upstream; that
group was dropped rather than re-folded across files.

## Kept unmerged

~340 client and ~100 server declarations remain as standalone tests: dense
incident-cited contracts per the K1/K8 pattern, each naming a contract no
sibling row guards. The keep rule stops the campaign here (AC5-EDGE).

## Verification

Scoped `fno doctor test rust client::tests::` and `server::tests::` lanes:
client module and server module green (server: 394 passed, 0 failed, 1
ignored). Compile-fix classes found and repaired: duplicated per-body use
lines in merged template tests, and `client` bindings shadowing the
`fn client(` helper in merged reanchor tests.
