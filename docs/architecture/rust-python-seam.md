# The Rust/Python seam

Where the boundary between the Rust runtime and the Python CLI belongs, what a crossing is, and how to score moving one. The companion page [dual-implementation-inventory.md](dual-implementation-inventory.md) holds the dual set and the port protocol. This page holds the boundary rule and the budget a port must satisfy.

## The FFI ruling

**Locked Decision. The seam stays process-only. No in-process channel comes on the table.**

The measured evidence: the tree contains zero FFI. No `pyo3`, no `maturin`, no `cpython`, no `cbindgen` appears in either crate manifest or in `cli/pyproject.toml`. Every crossing in both directions is a process boundary today, and this ruling keeps it that way.

The reasoning. A `maturin` build step couples the Python wheel to a compiled artifact on every platform. The install contract of a pure-Python package becomes a build matrix. The repo already ships one generated cross-language artifact, `harness_capabilities.toml`, and its freshness needs its own CI tripwire. Adding an ABI to that surface buys fine-grained ownership at the price of a per-platform build. The seam's traffic does not pay for it: the crossing sites are low-frequency verb shells and bounded reads, not hot in-process calls.

The socket of the store keeper does not weaken this ruling. It is a process boundary. A separate `fno-agents-worker` process serves one graph file over a local AF_UNIX socket. This is the pane-keeper model applied to the graph store. No library link, no shared address space, and no ABI exist between the sides. The socket is the one transport that does not spawn a command. The dependency direction requires it: `fno-agents` depends on `fno`, never the reverse. A consumer that lives in `fno` cannot link the store, so it must speak the socket protocol.

The consequence, and it is arithmetic, not preference. A subprocess has no cheap call, so a port must carry a decision's whole fact set or it adds a spawn. Moving half a decision across a process boundary trades one crossing for another and splits the fact set in two. Every later port proposal that splits a decision from its facts is refused by this paragraph, without re-arguing the build matrix.

## The measurement

Reproduce every number below on a clean checkout with `cd cli && uv run fno-py doctor lint seam-crossings`. It prints, for each baselined set, the measured count beside the baseline count.

Rust reaches the `fno` porcelain through **56 crossing sites across 18 files in both crates**. The lint also ratchets **16 resolver functions**, every function whose body resolves the porcelain path. The Python direction runs through one door. `cli/src/fno/rust_binary.py` is the only production Python file allowed to exec the literal `fno-agents` binary. 15 production files import it.

The counting rule, in words, so a reader can audit it without reading the lint. A crossing site is a production Rust line that launches `Command::new("fno")` or calls a baselined resolver helper name. A resolver function is a production Rust function with a line reading the `FNO_BIN` or `FNO_LOOPCHECK_FNO_BIN` env key, or constructing the porcelain path with `join("fno")`. Inline test modules, `crates/*/tests/`, and comment lines are out of scope. The baseline keys on `(rule, path, line content)` as a multiset, never on the line number. A moved line does not churn it. A removed site still fails.

A resolver is detected by shape, never by name. Four helpers carry obvious spellings: `fno_bin` in `crates/fno/src/server.rs:2843`, `fno_bin` in `crates/fno/src/yard_overlay.rs:49`, `fno_bin` in `crates/fno-agents/src/scrape.rs:174`, and `loopcheck_fno_bin` in `crates/fno-agents/src/loopcheck.rs:3934`. Twelve more functions resolve the porcelain inline, among them `fno_cmd` in `loop_dispatch.rs:136`, `durable_session_pid` in `claims.rs:2138`, and `best_effort_notify` in `loopcheck.rs:3942`. A first count that grepped one literal and four helper names reported 29 sites across 10 files. The real number is 56 across 18. That 48 percent shortfall is the worked example of a single-literal grep. It is why the resolver rule matches the env-key shape instead of a name list.

The write path is the asymmetry that matters, and the store port moved where it lives. The bytes of `graph.json` have one writer: the store publish pipeline in `crates/fno-agents/src/graph_store.rs`. The pipeline runs inside the keeper process and serializes every publish under the store's bounded lock. It writes the backup file and the hash sidecar together with the bytes. The Python porcelain verbs stay the only mutation surface on the CLI side. They reach the pipeline over the keeper socket (`cli/src/fno/graph/store.py` is the client). The mux used to shell the porcelain for its reorder verbs and read the verdict. Now the native store client (`crates/fno/src/store_client.rs`) speaks the same socket protocol. It adds no seam crossing. Reads keep one deliberately duplicated leg. `crates/fno/src/backlog_view.rs` parses the file itself for its read-only snapshots. The direction law forbids linking the store, and a socket round-trip per snapshot read costs more than the duplicate. The seam is no longer duplicated reads on both sides with writes held by Python. It is one store, two socket clients, and one native read mirror.

## The ownership rule

Rust owns what must not stop: the daemon, the PTY, the loop, liveness, and the claim protocol its own decisions read. Python owns what a user types and what reads the graph: the CLI verb surface, `graph.json`, and config resolution.

When the caller does not own the decision the answer feeds, the crossing is legitimate. When the caller owns it, the crossing is not. `finalize` shelling `fno backlog` to record an outcome is legitimate: it writes a Python-owned record through the single writer. The daemon shelling `claim list` to decide its own sweep is not. The caller owns the decision. The fact set belongs on the caller's side of the seam.

## The classification

The table classifies the crossing sites, one line of reason each. The pass that produced it validated the rule. Line numbers drift with every edit. The baseline keys on content, never on them. One conforming row is now retired: the graph reorder crossing. Its verb rides the store socket as a native client today. The row stays to say so. Two sites came back violating and five are infrastructure, the resolver definitions and their delegation line. That count is under a third unclassifiable or violating, so the rule stands as law.

| Site | Verdict | Reason |
|---|---|---|
| `bin/client.rs:581` | conforming | process-replacement delegation of the spawn call, whose decision Python owns |
| `bin/client.rs:602` | conforming | process-replacement delegation so account resolution stays one implementation |
| `bin/client.rs:2889` | conforming | session discovery read through its Python owner, fail-open on a miss |
| `claude_ask.rs:139` | conforming | transcript truth probe, single implementation kept in Python on purpose |
| `claude_ask.rs:545` | conforming | batch spelling of the same probe, one interpreter for N handles |
| `client_verbs.rs:1287` | conforming | ask-token mint through the Python resolver, one implementation |
| `client_verbs.rs:3087` | conforming | resume delegation by exec, exit code and signals carried by the child |
| `client_verbs.rs:3155` | conforming | pane launch through the one front door to the mux server |
| `client_verbs.rs:3463` | conforming | recovery relaunch through the same single door |
| `daemon.rs:1468` | conforming | reapable predicate read from its one implementation, shared by three callers |
| `daemon.rs:3477` | violating | the daemon reads `claim list` to decide its own worktree sweep, a caller-owned decision fed across the seam |
| `daemon.rs:3486` | conforming | the cleanup sweep executes through the verb that owns the buckets and guards |
| `daemon.rs:3530` | conforming | stale-question reconcile routed through the verb that owns it, no apply form |
| `daemon.rs:7876` | conforming | pane kill through the only path to the server that owns pane state |
| `daemon.rs:7935` | conforming | pane read probe, absence proved by the pane owner's own vocabulary |
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
| `scrape.rs:174` | infrastructure | the resolver helper itself |
| `scrape.rs:276` | conforming | pane title sweep read through the pane owner |
| `spawn_gate.rs:378` | conforming | gate-escape telemetry through the event emit path |
| `backlog_view.rs:69` | conforming | snapshot read through the only writer, schema owned by the source |
| `client.rs:2470` | conforming | update probe through the update policy owner, bounded |
| `client.rs:2649` | conforming | workspace prune through the front door, counts owned by the verb |
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
| `server.rs:3111` | retired | graph reorder used to shell the porcelain. `store_client.rs` speaks the keeper socket now and adds no crossing |
| `server.rs:3170` | conforming | respawn through the spawn surface owner |
| `server.rs:3202` | conforming | transcript peek read through the transcript reader owner |
| `server.rs:10818` | conforming | touch telemetry through the event emit path |
| `server.rs:10912` | conforming | pane counters through the same emit path |
| `yard_overlay.rs:49` | infrastructure | the delegation helper definition |
| `yard_overlay.rs:50` | infrastructure | the delegation body line, one resolver per crate enforced here |
| `yard_overlay.rs:57` | conforming | yard fold read through the verb owner, fail-open |

## The refusal of consumer-driven scoping

`claims.rs:5-8` states the pattern this seam must stop minting: "Scope is consumer-driven: acquire / release / status plus the liveness classifier - exactly what the daemon/adopt/drive/stream-worker call sites need. Everything else (list, refresh, force-release, lane slots) remains Python-only."

What that scope produces is predictable. Today's caller needs today's verbs, so the port moves today's verbs. Tomorrow's caller needs `list`, and `list` is on the far side of the seam, so tomorrow's caller shells back. Each new consumer adds a crossing instead of removing one, and the two implementations drift on the verbs nobody moved.

The replacement rule: port a decision's whole fact set, or do not port the decision. Under the process-only ruling this is arithmetic. A port that carries half a fact set adds a spawn per missing fact.

## The two-axis budget

A port must raise neither axis:

1. the crossing count that `fno doctor lint seam-crossings` ratchets, and
2. the dual-implementation count that the inventory page hand-maintains until the duplicate-discovery sweep lands.

The gaming path is real, and the tree already holds the specimen. `crates/fno/src/backlog_view.rs` is 2140 lines of native graph parsing whose docstrings name their Python oracles. It holds exactly one crossing. Replacing a shell-out with more of that file lowers axis one and raises axis two, and a one-axis budget scores the trade as an improvement. It is a worsening: the tree gained a second implementation of read logic and lost nothing. The budget reads both numbers, and a port that raises either is refused.

The store port is the counter-example, and it names its own costs. It removed the reorder crossing and deleted the Python store leg. Both axes fell. Two prices remain. First, the frame codec is hand-written three times: the Python client, the keeper, and the mux client. The codec is small: five frame tags and one request grammar. The direction law allows no shared crate. The unit tree exercises the Python spelling against the real keeper on every run. Second, a mux-side write skips the Python post-publish pass. The graph.md render and the active-backlog nudge then land on the next CLI-driven write, not on the mux write. The graph bytes are never partial because the store publishes atomically. A budget that reads only counts scores this port as free. The port is not free.

## Sequencing

Order ports topologically over the crossing dependency graph, not by risk. Risk ranks the claim classifier first on the inventory page because nothing pins it. Dependency says which port can land first, and the two orders disagree.

The first real edge: `claim list` becomes Rust-owned before the daemon's reap decision can be. Porting the decision first makes the daemon shell for the facts it no longer owns, which adds a crossing. Port the fact set, then the decision, in that order, and the crossing count falls instead of rotating.

The two violating sites above are where that order starts. `daemon.rs:3477` waits for the claim fact set. `provider.rs:544` waits for the plan-path fact set, and the remedy is carrying the fact set, never a native re-implementation that raises axis two.
