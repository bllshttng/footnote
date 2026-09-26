# The Rust/Python seam

Where the boundary between the Rust runtime and the Python CLI belongs, what a crossing is, and how to score moving one. The companion page [dual-implementation-inventory.md](dual-implementation-inventory.md) holds the dual set and the port protocol. This page holds the boundary rule and the budget a port must satisfy.

## The FFI ruling

**Locked Decision. The seam stays process-only. No in-process channel comes on the table.**

The measured evidence: the tree contains zero FFI. No `pyo3`, no `maturin`, no `cpython`, no `cbindgen` appears in either crate manifest or in `cli/pyproject.toml`. Every crossing in both directions is a process boundary today, and this ruling keeps it that way.

The reasoning. A `maturin` build step couples the Python wheel to a compiled artifact on every platform. The install contract of a pure-Python package becomes a build matrix. The repo already ships one generated cross-language artifact, `harness_capabilities.toml`, and its freshness needs its own CI tripwire. Adding an ABI to that surface buys fine-grained ownership at the price of a per-platform build. The seam's traffic does not pay for it: the crossing sites are low-frequency verb shells and bounded reads, not hot in-process calls.

The socket of the store keeper does not weaken this ruling. It is a process boundary. A separate `fno-agents-worker` process serves one graph file over a local AF_UNIX socket. This is the pane-keeper model applied to the graph store. No library link, no shared address space, and no ABI exist between the sides. The socket is the one transport that does not spawn a command. The dependency direction requires it: `fno-agents` depends on `fno`, never the reverse. A consumer that lives in `fno` cannot link the store, so it must speak the socket protocol. That dependency is a dev-dependency only. Production code in `fno-agents` reaches the mux through the `fno` binary, never by linking the crate. That is why the attach verb's portal reach is a crossing and not a call.

The consequence, and it is arithmetic, not preference. A subprocess has no cheap call, so a port must carry a decision's whole fact set or it adds a spawn. Moving half a decision across a process boundary trades one crossing for another and splits the fact set in two. Every later port proposal that splits a decision from its facts is refused by this paragraph, without re-arguing the build matrix.

## The measurement

Reproduce every number below on a clean checkout with `cd cli && uv run fno-py doctor lint seam-crossings`. It prints, for each baselined set, the measured count beside the baseline count.

Rust reaches the `fno` porcelain through **80 baselined crossing sites in both crates**. The lint also ratchets **27 resolver functions**, every function whose body resolves the porcelain path. The Python direction runs through one door. `cli/src/fno/rust_binary.py` is the only production Python file allowed to exec the literal `fno-agents` binary. 15 production files import it.

The counting rule, in words, so a reader can audit it without reading the lint. A crossing site is a production Rust line that launches `Command::new("fno")` or calls a baselined resolver helper name. A resolver function is a production Rust function with a line reading the `FNO_BIN` or `FNO_LOOPCHECK_FNO_BIN` env key, or constructing the porcelain path with `join("fno")`. Inline test modules, `crates/*/tests/`, and comment lines are out of scope. The baseline keys on `(rule, path, line content)` as a multiset, never on the line number. A moved line does not churn it. A removed site still fails.

A resolver is detected by shape, never by name. Four helpers carry obvious spellings: `fno_bin` in `crates/fno/src/server.rs:2247`, `fno_bin` in `crates/fno/src/yard_overlay.rs:49`, `fno_bin` in `crates/fno-agents/src/scrape.rs:174`, and `loopcheck_fno_bin` in `crates/fno-agents/src/loopcheck.rs:3091`. Twenty-three more functions resolve the porcelain inline, among them `fno_cmd` in `loop_dispatch.rs:136`, `durable_session_pid` in `claims.rs:2670`, and `best_effort_notify` in `loopcheck.rs:3109`. A historical first count that grepped one literal and four helper names reported 29 sites across 10 files. The full scan then found 56 across 18. That 48 percent shortfall shows why the resolver rule matches the env-key shape instead of a name list.

The write path is the asymmetry that matters, and the store port moved where it lives. The durable store `graph.db` has one writer: the store publish pipeline in `crates/fno-agents/src/graph_store.rs`. The pipeline runs inside the keeper process and serializes every publish under the store's bounded lock. It writes the backup file together with the bytes. The Python porcelain verbs stay the only mutation surface on the CLI side. They reach the pipeline over the keeper socket (`cli/src/fno/graph/store.py` is the client). The mux used to shell the porcelain for its reorder verbs and read the verdict. Now the native store client (`crates/fno/src/store_client.rs`) speaks the same socket protocol, and the write itself adds no seam crossing. The one crossing a native write pays is the detached `fno backlog render-views` replay: the views are Python-owned, so the write crosses back once. It is baselined with the rest. Reads keep one deliberately duplicated leg. `crates/fno/src/backlog_view.rs` parses the file itself for its read-only snapshots. The direction law forbids linking the store, and a socket round-trip per snapshot read costs more than the duplicate. The seam is no longer duplicated reads on both sides with writes held by Python. It is one store, two socket clients, and one native read mirror.

Two interpreter spawns retired with the plan-doc writer port (2026-09-22). `finalize` used to shell `python3 -m` twice per ship, once to stamp the plan and once to graduate it. The writer's whole fact set is the graph rows plus the doc bytes. So the port moved the codec, the projection, and the stamp commands into `crates/fno-agents/src/plan_doc` behind the keeper's `plan_docs` method. `finalize` stamps in process, the Python verbs are clients of the same method, and the crossings are gone rather than rebaselined. The plan-doc sidecar lock stays shared vocabulary with the surviving Python body-append writer. The two still serialize on one path scheme.

## The ownership rule

Rust owns what must not stop: the daemon, the PTY, the loop, liveness, and the claim protocol its own decisions read. Python owns what a user types and what reads the graph: the CLI verb surface, the graph read verbs, and config resolution.

When the caller does not own the decision the answer feeds, the crossing is legitimate. When the caller owns it, the crossing is not. `finalize` shelling `fno backlog` to record an outcome is legitimate: it writes a Python-owned record through the single writer. The daemon's former `claim list` shell-out was not: the daemon owned the sweep decision, so the claim fact set now lives natively beside it.

## The classification

The table classifies the crossing sites, one line of reason each. The pass that produced it validated the rule. Line numbers drift with every edit. The baseline keys on content, never on them. One conforming row is now retired: the graph reorder crossing. Its verb rides the store socket as a native client today. The row stays to say so. Two sites came back violating and five are infrastructure, the resolver definitions and their delegation line. That count is under a third unclassifiable or violating, so the rule stands as law.

| Site | Verdict | Reason |
|---|---|---|
| `bin/client.rs:581` | conforming | process-replacement delegation of the spawn call, whose decision Python owns |
| `bin/client.rs:602` | conforming | process-replacement delegation so account resolution stays one implementation |
| `bin/client.rs:2889` | conforming | session discovery read through its Python owner, fail-open on a miss |
| `bin/client.rs:1346 place_thread_portal_after_spawn` | conforming | post-spawn portal placement through the same mux thread door as `attach.rs:28`; the server owns the thread pane, the fno crate is dev-only here, and a native ThreadPane client would carry a second PROTO_VERSION pin |
| `bin/client.rs:1399 exec_python_front` | conforming | process-replacement delegation of the spawn call to the Python front door, whose decision Python owns |
| `bin/client.rs:2433 run_mux_sweep` | conforming | the reap sweep executes through the verb that owns the buckets and guards |
| `bin/client.rs:3819 discovered-session read` | conforming | session discovery read through its Python owner, fail-open on a miss |
| `claude_ask.rs:139` | conforming | transcript truth probe, single implementation kept in Python on purpose |
| `claude_ask.rs:545` | conforming | batch spelling of the same probe, one interpreter for N handles |
| `client_verbs.rs:1287` | conforming | ask-token mint through the Python resolver, one implementation |
| `client_verbs.rs:3087` | conforming | resume delegation by exec, exit code and signals carried by the child |
| `client_verbs.rs:3155` | conforming | pane launch through the one front door to the mux server |
| `client_verbs.rs:3463` | conforming | recovery relaunch through the same single door |
| `attach.rs:28` | conforming | attach reaches the mux thread portal through the same door; the server owns the thread pane, and the fno crate is a dev-only dependency here |
| `daemon.rs:1468` | conforming | reapable predicate read from its one implementation, shared by three callers |
| `daemon.rs:3486` | conforming | the cleanup sweep executes through the verb that owns the buckets and guards |
| `daemon.rs:3530` | conforming | stale-question reconcile routed through the verb that owns it, no apply form |
| `daemon.rs:7876` | conforming | pane kill through the only path to the server that owns pane state |
| `daemon.rs:7935` | conforming | pane read probe, absence proved by the pane owner's own vocabulary |
| `spawn_gate_lanes.rs:95` | conforming | pane liveness for the provider count, read through the pane owner's own wait verb like `daemon.rs:7935` |
| `daemon.rs:9010` | conforming | codex rollout walk reused from Python rather than reimplemented |
| `finalize.rs:472` | conforming | run summary pushed to the parent through the event registry owner |
| `finalize.rs:1858` | conforming | PR metadata read through the REST wrapper owner |
| `finalize.rs:1914` | conforming | PR stamp written to the graph through the only writer |
| `finalize.rs:2251` | conforming | merge-hold predicate read from the verb that owns the policy, fail-closed |
| `finalize.rs:2298` | conforming | stacked-base lineage predicate read from its verb, fail-open by design |
| `finalize.rs:2458` | conforming | session record added through the manifest owner |
| `finalize.rs:2695` | conforming | dedup read through the verb an operator would run, so state is never re-derived |
| `finalize.rs:2715` | conforming | question filed through the durable channel owner |
| `loop_dispatch.rs:364` | conforming | account pick through the one implementation of billing truth |
| `loopcheck.rs:3934` | infrastructure | the resolver helper itself, the seam's plumbing |
| `loopcheck.rs:10956` | conforming | decision record read through `backlog decisions`, its owner |
| `loopcheck.rs:13614` | conforming | gate reads board and law state through the graph's single writer |
| `nudge.rs:35` | conforming | inbox nudge read through the durable channel owner |
| `provider.rs:544` | violating | a Rust-owned sandbox decision fed by a Python-owned plan-path fact, the split the FFI ruling refuses |
| `reentry.rs:120` | conforming | account binding read from the store, never reimplemented |
| `scratch.rs:1300` | conforming | node birth + hidden-leaf probe through the `fno_bin` resolver's porcelain; the sweep does not own the filing decision, `backlog idea` does |
| `scrape.rs:174` | infrastructure | the resolver helper itself |
| `scrape.rs:276` | conforming | pane title sweep read through the pane owner |
| `spawn_gate.rs:378` | conforming | gate-escape telemetry through the event emit path |
| `backlog_view.rs:69` | conforming | snapshot read through the only writer, schema owned by the source |
| `client.rs:2470` | conforming | update probe through the update policy owner, bounded |
| `client.rs:2649` | conforming | workspace prune through the front door, counts owned by the verb; an applied run sends `SquadReload` to every live server so the file and memory agree; an orphaned worker tab (its stored member judged Dead) closes by default, used shells stay opt-in |
| `client.rs:14413` | conforming | config write through the CLI, the same monopoly as the graph |
| `connections_view.rs:1240` | conforming | config and combo reads through the config owner, fail-open |
| `connections_view.rs:1278` | conforming | user-initiated verbs dispatched through the CLI surface |
| `needs_overlay.rs:122` | conforming | open questions read through the durable store owner |
| `needs_overlay.rs:144` | conforming | mine list read through the same store |
| `needs_overlay.rs:209` | conforming | mine mutations written through the single writer |
| `needs_overlay.rs:257` | conforming | answer recorded through the same writer, errors surfaced |
| `server.rs:2843` | infrastructure | the resolver helper itself |
| `server.rs:2884` | conforming | config read bounded through the config owner |
| `server.rs:2954` | conforming | spawn dispatched through the surface that owns provider resolution |
| `server.rs:3083` | conforming | mail sent through the bus owner |
| `server.rs:3111` | retired | graph reorder used to shell the porcelain. `store_client.rs` speaks the keeper socket now; the landed write detaches one `render-views` replay for the Python-owned views |
| `server.rs:3170` | conforming | respawn through the spawn surface owner |
| `server.rs:3202` | conforming | transcript peek read through the transcript reader owner |
| `server.rs:10818` | conforming | touch telemetry through the event emit path |
| `server.rs:10912` | conforming | pane counters through the same emit path |
| `yard_overlay.rs:49` | infrastructure | the delegation helper definition |
| `yard_overlay.rs:50` | infrastructure | the delegation body line, one resolver per crate enforced here |
| `yard_overlay.rs:57` | conforming | yard fold read through the verb owner, fail-open |
| `spawn_gate_lanes.rs:123` | conforming | pane listing fallback for the same probe, the pane owner's authoritative enumeration |

## The refusal of consumer-driven scoping

Before this port, `claims.rs` stated the pattern this seam must stop minting. A consumer-driven Rust subset leaves `list` and the rest of the decision's facts in Python. The native claim list, sweep classification, and batch verdict door now close that gap.

The old scope produced a predictable regression. Today's caller needed today's verbs, so the port moved today's verbs. Tomorrow's caller needed `list`, and `list` was on the far side of the seam, so tomorrow's caller shelled back. The complete fact-set port removes that crossing and leaves Python with one batch door instead of a second classifier.

The replacement rule: port a decision's whole fact set, or do not port the decision. Under the process-only ruling this is arithmetic. A port that carries half a fact set adds a spawn per missing fact.

## The two-axis budget

A port must raise neither axis:

1. the crossing count that `fno doctor lint seam-crossings` ratchets, and
2. the dual-implementation count that the inventory page hand-maintains until the duplicate-discovery sweep lands.

The gaming path is real, and the tree already holds the specimen. `crates/fno/src/backlog_view.rs` is 2140 lines of native graph parsing whose docstrings name their Python oracles. It holds exactly one crossing. Replacing a shell-out with more of that file lowers axis one and raises axis two, and a one-axis budget scores the trade as an improvement. It is a worsening: the tree gained a second implementation of read logic and lost nothing. The budget reads both numbers, and a port that raises either is refused.

The store port is the counter-example, and it names its own costs. It removed the reorder crossing and deleted the Python store leg. Both axes fell. Two prices remain. First, the frame codec is hand-written three times: the Python client, the keeper, and the mux client. The codec is small: five frame tags and one request grammar. The direction law allows no shared crate. The unit tree exercises the Python spelling against the real keeper on every run. Second, the views stay Python-owned. A landed mux write detaches one `fno backlog render-views` subprocess to replay the post-publish pass. The write is native and atomic, but a native writer cannot refresh graph.md without crossing back to the renderer. A budget that reads only counts scores this port as free. The port is not free.

## Sequencing

Order ports topologically over the crossing dependency graph, not by risk. Risk ranks the claim classifier first on the inventory page because nothing pins it. Dependency says which port can land first, and the two orders disagree.

The first real edge landed: `claim list` and the sweep fact set became Rust-owned before the daemon's reap decision moved. The daemon now reads the native list directly, so the crossing count falls instead of rotating.

The remaining violating site is `provider.rs:544`, which waits for the plan-path fact set. The remedy is carrying that fact set, never a native re-implementation that raises axis two.

### Port order

The outside-in order. Ports run leaves-first: a module nothing imports moves first, then low fan-in, then low churn, then low use. A dependency still outranks the score, per this section's opening paragraph. A port whose landing needs another module's fact set waits for that port whatever the score says. The plan-path resolution port gates the violating crossing named above. A port raises neither budget axis, so a row moving up this order never justifies a new crossing or a new dual.

The table is generated by `bash scripts/metrics/port-order.sh` from a checkout, dated 2026-09-24. Regenerate it there whenever a fresh ranking is needed. The numbers are a dated snapshot, not a freshness gate: churn and use move daily by design, and nothing in CI re-checks them.

The counting rule, in words, so a reader can audit a row without reading the script. One row per top-level module under `cli/src/fno`: a subpackage directory, or a top-level `.py` file. Single files named in the script's extras list get their own row. The remainder row holds the parts of `pr/closure.py` the Fixes-line parser port did not take. `lines` sums `wc -l` over the module's `.py` files. `fan-in` counts distinct files outside the module importing it, absolute or relative, at any indentation, so function-level imports count. `churn` is commits in the trailing 30 days on `origin/main` touching the module, then open-PR touched files after the slash. A dash means no open PR touches the module. If `gh` is absent the sweep is skipped with a note. `use` counts `fno <verb-group>` occurrences in this machine's 30-day agent transcripts plus `"verb"` rows in the event journal. It reads zero on a fresh clone. It counts prose mentions, so it is the loosest of the five numbers. `crossing edges` is the module's degree in the import graph: distinct other modules it imports or is imported by. `leaf` marks fan-in zero. Test-only modules and `conftest` rank as leaves but are not port targets on their own. They move with the production surface they exercise, never ahead of it. The floating single-port backlog nodes link to the port-order backlog node as related entries and carry board ranks in this table's order.
| # | module | lines | fan-in | churn (30d, commits/prs) | use | crossing edges |
|---|---|---:|---:|---|---:|---:|
| 1 | test_cost (leaf) | 484 | 0 | 0/- | 0 | 2 |
| 2 | test_dispatch_target (leaf) | 198 | 0 | 0/- | 0 | 1 |
| 3 | test_target_init_blast (leaf) | 276 | 0 | 0/- | 0 | 2 |
| 4 | test_turn_attribution (leaf) | 222 | 0 | 0/- | 0 | 1 |
| 5 | do_cli (leaf) | 74 | 0 | 1/- | 0 | 14 |
| 6 | test_target_init_plan_backfill (leaf) | 353 | 0 | 1/- | 0 | 2 |
| 7 | plugins (leaf) | 2436 | 0 | 1/- | 39 | 4 |
| 8 | executor (leaf) | 658 | 0 | 1/- | 145 | 0 |
| 9 | verify_advise (leaf) | 346 | 0 | 2/- | 0 | 2 |
| 10 | test_dispatch_flags (leaf) | 153 | 0 | 3/- | 0 | 1 |
| 11 | project (leaf) | 182 | 0 | 4/- | 997 | 2 |
| 12 | conftest (leaf) | 85 | 0 | 5/- | 0 | 3 |
| 13 | test_worktree_paths (leaf) | 248 | 0 | 5/- | 0 | 1 |
| 14 | workspace (leaf) | 70 | 0 | 5/- | 5453 | 4 |
| 15 | paths_testing (leaf) | 75 | 0 | 6/- | 0 | 2 |
| 16 | __init__ (leaf) | 279 | 0 | 7/- | 0 | 0 |
| 17 | paths_cli (leaf) | 187 | 0 | 7/- | 0 | 5 |
| 18 | plugin_install_cli (leaf) | 79 | 0 | 7/- | 0 | 2 |
| 19 | worker (leaf) | 795 | 0 | 8/1 | 439 | 6 |
| 20 | autonomy_cli (leaf) | 362 | 0 | 12/- | 0 | 5 |
| 21 | doctor_cli (leaf) | 136 | 0 | 22/1 | 0 | 22 |
| 22 | restart (leaf) | 49 | 0 | 24/- | 3688 | 1 |
| 23 | paths_verify | 38 | 1 | 0/- | 0 | 2 |
| 24 | context_audit | 952 | 1 | 1/- | 0 | 2 |
| 25 | hook_config | 104 | 1 | 1/- | 0 | 1 |
| 26 | lint_shellout_drift | 499 | 1 | 1/- | 0 | 1 |
| 27 | worktree_gate | 62 | 1 | 1/- | 0 | 2 |
| 28 | mcp | 1726 | 1 | 1/- | 17 | 2 |
| 29 | phase | 91 | 1 | 1/- | 128 | 5 |
| 30 | bundle | 159 | 1 | 1/- | 262 | 2 |
| 31 | state_fence | 138 | 1 | 2/- | 0 | 1 |
| 32 | delivery | 1788 | 1 | 2/- | 30 | 5 |
| 33 | yard | 125 | 1 | 2/- | 379 | 1 |
| 34 | doctor_reclaim | 29 | 1 | 3/- | 0 | 3 |
| 35 | lint_verb_ratchet | 739 | 1 | 3/- | 0 | 4 |
| 36 | scratch_cli | 71 | 1 | 3/- | 0 | 3 |
| 37 | test_runner | 110 | 1 | 3/- | 0 | 2 |
| 38 | lint_seam_crossings | 419 | 1 | 4/- | 0 | 1 |
| 39 | review_level | 209 | 1 | 4/- | 0 | 4 |
| 40 | codemap_cli | 1545 | 1 | 5/- | 0 | 3 |
| 41 | post_merge_route | 740 | 1 | 5/- | 0 | 10 |
| 42 | doctor_bash_census | 33 | 1 | 6/- | 0 | 3 |
| 43 | skill_diff | 1360 | 1 | 5/1 | 0 | 6 |
| 44 | context_probe | 85 | 1 | 7/- | 0 | 3 |
| 45 | annotate | 56 | 1 | 10/- | 28 | 2 |
| 46 | status_fanout | 900 | 1 | 11/- | 0 | 9 |
| 47 | done | 967 | 1 | 11/- | 2607 | 4 |
| 48 | agent | 1127 | 1 | 14/- | 247 | 12 |
| 49 | think_inspect | 616 | 1 | 16/1 | 0 | 7 |
| 50 | route_cli | 646 | 1 | 24/- | 0 | 6 |
| 51 | worktree_cli | 1092 | 1 | 29/- | 0 | 9 |
| 52 | test_cmd | 2397 | 1 | 70/- | 0 | 5 |
| 53 | env_file | 33 | 2 | 0/- | 0 | 2 |
| 54 | fleet_state | 220 | 2 | 0/- | 0 | 3 |
| 55 | projects | 359 | 2 | 0/- | 70 | 3 |
| 56 | research | 860 | 2 | 1/- | 27 | 6 |
| 57 | setup_cli | 882 | 2 | 2/- | 0 | 5 |
| 58 | schemas | 406 | 2 | 2/- | 9 | 3 |
| 59 | stub_manifest | 356 | 2 | 3/- | 0 | 3 |
| 60 | wake | 158 | 2 | 3/- | 20 | 3 |
| 61 | health_monitor | 895 | 2 | 4/- | 0 | 3 |
| 62 | control_plane | 47 | 2 | 5/- | 0 | 4 |
| 63 | ledger_show | 178 | 2 | 5/- | 0 | 7 |
| 64 | worktree_status | 165 | 2 | 5/- | 0 | 3 |
| 65 | relay | 1862 | 2 | 5/- | 810 | 9 |
| 66 | runtime | 415 | 2 | 6/- | 439 | 5 |
| 67 | branch_provenance_cache | 84 | 2 | 8/- | 0 | 4 |
| 68 | config_readback | 280 | 2 | 8/- | 0 | 5 |
| 69 | worktree | 596 | 2 | 9/- | 791 | 6 |
| 70 | doctor_graph | 86 | 2 | 14/1 | 0 | 4 |
| 71 | resume | 1075 | 2 | 15/- | 228 | 8 |
| 72 | law | 189 | 2 | 22/- | 2654 | 5 |
| 73 | doctor_lanes | 542 | 2 | 28/- | 0 | 5 |
| 74 | observer | 2574 | 2 | 34/1 | 72 | 12 |
| 75 | lint_cli | 2165 | 2 | 42/- | 0 | 13 |
| 76 | update | 1662 | 2 | 62/- | 7478 | 8 |
| 77 | config_cli | 1723 | 2 | 73/- | 0 | 16 |
| 78 | llm | 72 | 3 | 0/- | 0 | 2 |
| 79 | approvals | 1703 | 3 | 0/- | 65 | 7 |
| 80 | turn_attribution | 153 | 3 | 1/- | 0 | 3 |
| 81 | ledger_join | 122 | 3 | 2/- | 0 | 4 |
| 82 | terminals | 18 | 3 | 2/- | 0 | 3 |
| 83 | drive_authority | 133 | 3 | 3/- | 0 | 5 |
| 84 | verb_moves | 167 | 3 | 4/- | 0 | 3 |
| 85 | active_backlog | 108 | 3 | 11/- | 0 | 5 |
| 86 | target | 2216 | 3 | 15/- | 9616 | 11 |
| 87 | route_slot_client | 30 | 3 | 17/- | 0 | 4 |
| 88 | footprint | 480 | 3 | 23/- | 8 | 3 |
| 89 | doctor_footprint | 1194 | 3 | 68/- | 0 | 8 |
| 90 | _lazy_group | 502 | 4 | 2/- | 0 | 7 |
| 91 | user | 62 | 4 | 3/- | 588 | 5 |
| 92 | loops | 202 | 4 | 4/- | 42 | 8 |
| 93 | worktree_stranded | 419 | 4 | 16/- | 0 | 8 |
| 94 | cli | 933 | 4 | 17/- | 1183 | 13 |
| 95 | recovery | 1438 | 4 | 30/1 | 0 | 8 |
| 96 | evals | 1557 | 4 | 64/- | 97 | 12 |
| 97 | mutex | 245 | 5 | 1/- | 7 | 5 |
| 98 | notify | 223 | 5 | 7/- | 260 | 7 |
| 99 | style | 741 | 5 | 10/- | 18 | 5 |
| 100 | retro | 4236 | 5 | 16/- | 238 | 15 |
| 101 | hermetic | 623 | 5 | 46/- | 6 | 5 |
| 102 | target_cli | 3810 | 5 | 69/- | 0 | 30 |
| 103 | doctor | 4599 | 5 | 111/1 | 275486 | 23 |
| 104 | time_budget | 19 | 6 | 0/- | 0 | 3 |
| 105 | pr/closure.py remainder (branch resolution, claim binding, PR context fetch; Fixes parser already Rust) | 529 | 6 | 10/- | 0 | 6 |
| 106 | outstanding | 1794 | 6 | 47/- | 6451 | 18 |
| 107 | text_or_file | 34 | 7 | 4/- | 0 | 5 |
| 108 | review_capability | 1096 | 7 | 18/- | 0 | 9 |
| 109 | handoff | 153 | 8 | 1/- | 2 | 8 |
| 110 | cost | 2774 | 8 | 16/1 | 58 | 15 |
| 111 | harness_names | 76 | 8 | 18/- | 0 | 5 |
| 112 | review | 2373 | 8 | 49/- | 31086 | 14 |
| 113 | roles | 2025 | 9 | 0/- | 7 | 6 |
| 114 | state | 922 | 9 | 12/- | 3396 | 13 |
| 115 | worktree_paths | 487 | 9 | 17/- | 0 | 11 |
| 116 | scoreboard | 3116 | 9 | 29/1 | 68 | 13 |
| 117 | pr_watch | 6345 | 9 | 191/1 | 0 | 26 |
| 118 | dispatch_flags | 203 | 10 | 10/- | 0 | 7 |
| 119 | inbox | 2950 | 10 | 24/- | 156599 | 19 |
| 120 | king | 2302 | 10 | 143/1 | 1342 | 14 |
| 121 | tombstones | 145 | 11 | 2/- | 0 | 10 |
| 122 | _flag_aliases | 123 | 11 | 3/- | 0 | 9 |
| 123 | decide | 3080 | 11 | 60/- | 3062 | 21 |
| 124 | config_io | 310 | 12 | 9/- | 0 | 7 |
| 125 | route_resolve | 792 | 12 | 71/- | 0 | 11 |
| 126 | bus | 918 | 13 | 15/- | 16 | 9 |
| 127 | carveout | 1235 | 14 | 5/- | 338 | 14 |
| 128 | setup | 4816 | 14 | 55/1 | 253 | 20 |
| 129 | mail | 8480 | 14 | 147/1 | 14932 | 24 |
| 130 | pr | 15558 | 15 | 283/2 | 5861 | 25 |
| 131 | backlog | 10985 | 15 | 335/2 | 581738 | 30 |
| 132 | company | 1148 | 16 | 1/- | 0 | 7 |
| 133 | adapters | 28642 | 18 | 96/- | 0 | 15 |
| 134 | plan | 6935 | 19 | 60/1 | 10200 | 19 |
| 135 | provenance | 2984 | 23 | 26/- | 18 | 23 |
| 136 | _subprocess_util | 89 | 33 | 4/- | 0 | 25 |
| 137 | tracker | 582 | 36 | 8/- | 27 | 23 |
| 138 | claims | 7393 | 58 | 188/1 | 84 | 34 |
| 139 | harness_identity | 1307 | 61 | 43/- | 0 | 27 |
| 140 | events | 4519 | 66 | 268/3 | 374 | 42 |
| 141 | graph | 37715 | 83 | 689/4 | 364 | 59 |
| 142 | agents | 67376 | 84 | 1388/1 | 579514 | 74 |
| 143 | config | 8388 | 102 | 325/1 | 106736 | 57 |
| 144 | rust_binary | 300 | 103 | 13/- | 0 | 42 |
| 145 | paths | 1844 | 190 | 45/2 | 442 | 74 |
