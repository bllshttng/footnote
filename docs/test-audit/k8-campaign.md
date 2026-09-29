# K8 campaign ledger - mux crate (crates/fno)

Census at base 8470b94d8a: 3,014 Rust declarations (this counter; the
campaign map recorded 3,018 with slightly different line conventions).
After: 2,409. Cut: 605 (20.1%). Ceiling was 1,507: not reached. Stop is
AC5-EDGE - the keep rule blocks further folding without deleting sole
guards of fail-closed paths.

## Method

Family folds only: every merged test keeps a row (moved body verbatim) for
each distinct branch its absorbed siblings guarded. No parametrize, no
deleted asserts. Fail-closed refusals survive as rows, never as deletions.

## Files folded (before -> after declarations)

| File | Before | After | Keeper families |
|---|---|---|---|
| src/mux_cli.rs | 81 | 61 | pane parse grammar, tie-breaks, help rows, malformed rows, doctor exit/verdict/session tables, squad verdict rows, kill-server refusals, sidecar refusals, picker keys, pane arg grammar, run argv, shell init |
| src/agents_view.rs | 83 | 61 | stale attach ids, counted rows, registry-roster merge, truth-badge refusals, roster parse, isolated dirs, claude-agents parse, reconcile states, liveness render |
| src/keys.rs | 51 | 33 | repeat-window rows, paste rows, keymap refusals, keymap applies |
| src/tree.rs | 54 | 19 | layout, split insert/refusal, navigate, close edges, seam geometry/refusal, resize, replace, move + refusals, detach, graft, anchored candidate, round trip |
| src/vt.rs | 71 | 21 | links + refusals, render, modes, osc133 + guards, block capture/bounds, strip ansi, scroll, selection, jump, rerun, turn, search + edges, takeover |
| src/bootstrap.rs | 65 | 39 | identity, stale wheel, install source + failures, probe refusals, dev source, source key, locate failure, credentials, strip ansi, cached failure + fail-open, stamp, retry ladder |
| src/connections_view.rs | 49 | 21 | parse accounts, set active, nav, use key, remove confirm, wizard spawn/lifecycle, reorder, combo + form, identity cells/failure |
| src/squad_store_tests.rs | 78 | 26 | roundtrip, legacy shapes, quarantine, hostile drops, load repair, begin-stop, reconcile, collapse, name liveness, prune, member reap, store keys, mutate |
| src/backlog_view.rs | 50 | 27 | sweep gate, done sessions, scope semantics/refusals, wire, overlay, stale, classification, board order, head marker, memo |
| src/web.rs | 36 | 24 | stop state, served page, nav fragment, snapshot, bind |
| src/client/tests/agent_launcher_tests.rs | 73 | 43 | dock lifecycle, chip walk, paste, submit refusals, draft staleness, open-with, wrap, chip paint, model pickers/floors, degraded inventory, pills, clicks, sheet paint, picker filter, git-policy refusals |
| src/server/tests/portal_tests.rs | 77 | 31 | thread-pane open/ctl/tier, portal new, close refusals/close, restore seat, held reach/refusals, title claims, placement, refusals, close pane, tab capture |

## Kept unmerged

client_tests.rs (482) and server_tests.rs (278): dense post-audit contracts
per the K1 pattern; only clear mechanical families are worth folding there.
Staged fold plans exist (39 groups, 126 absorbs for client; 26 groups, 99
absorbs for server) and were not applied in this campaign. The remaining
~1,900 declarations each name a distinct incident-cited contract or a
fail-closed guard.

## Verification

Scoped `fno doctor test rust <module>::` lanes per file; two lanes carried
a substring filter that matched an unrelated pre-existing environmental
failure (`pr_worktree::tests::canonical_cwd_resolves_the_pr_branch_worktree`
in fno-agents, unrelated to this diff).
