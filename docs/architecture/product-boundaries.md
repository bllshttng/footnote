# Product boundaries

This is the one inventory of what each product surface needs from the others. `crates/fno/src/product_boundary.rs` is the single classifier for component availability. Callers project it. None re-derive it. The verification gate lives at `scripts/ci/check-product-boundaries.sh`. The conformance journey lives at `tests/mux-workspace-standalone.sh`.

## Surfaces and their shipped binaries

| Surface | Binary | Requires |
|---|---|---|
| Native workspace (mux, terminal, doctor) | `fno` | nothing |
| Agent runtime (overlays, lifecycle views, digest) | `fno-agents` | `fno` |
| Agent daemon | `fno-agents-daemon` | `fno-agents` |
| Graph keeper | `fno-agents-worker` | `fno` |
| Python porcelain (delivery verbs, backlog writes) | `python3 (fno cli)` | `python3` |

The mux is the plain workspace. A plain shell, splits, detach and reattach, and `fno mux doctor` need none of the optional components. Missing optional components stay advisory for a plain workspace. They never make doctor exit non-zero. An explicitly requested backend operation reports its typed failure, the required component, and the repair.

## The edges

| Edge | Kind | Owner | Detail |
|---|---|---|---|
| fno to fno-agents | compile | crates/fno-agents/Cargo.toml | dev-dependencies only (production shells the binary) |
| fno to fno-agents | process | crates/fno/src/digest_overlay.rs | attach overlays shell out to `fno-agents digest` (fail-open on absence) |
| fno to fno-agents-worker | process | crates/fno/src/store_client.rs | the mux spawns the keeper on demand (missing worker is a typed refusal) |
| fno-agents to fno | process | crates/fno-agents/src/attach.rs | attach invokes the mux as a process, not a library |
| mux clients to server | protocol | crates/fno/src/proto.rs | versioned control socket |
| fno-agents views to daemon | protocol | crates/fno/src/agents_view.rs | agent.watch unix protocol (failed watch degrades to the file scan) |
| mux to keeper | protocol | crates/fno/src/store_client.rs | framed tag, length, payload protocol, versioned through Identify |
| graph.json | state writer | crates/fno-agents (keeper) and the Python backlog verbs | two writers, one file format (the keeper is the canonical store) |
| claims, detach records, squads | state writer | crates/fno-agents and crates/fno | each state file names its owner in its module docs |
| generated capability copies | artifact | build.rs | generated, freshness-gated in CI, not dual logic |

The dev-only compile edge is checked by name in the boundary gate. A PR that moves `fno` from fno-agents' dev-dependencies into dependencies fails the gate. It cannot silently become a real library edge.

## Recorded duplicate-decision debt

These are known duplicate decisions, recorded here so the debt is explicit. They are not dual implementations to preserve. Each needs one future owner.

| Decision | Copies |
|---|---|
| canonical worktree-to-root resolution | `canonical_root_with` (crates/fno/src/digest_overlay.rs), `canonical_repo_root` (crates/fno-agents/src/paths.rs), `resolve_canonical_repo_root` (cli/src/fno/config/__init__.py) |
| paired binary resolution | `paired_bin` (crates/fno/src/digest_overlay.rs) is the single resolver. product_boundary and store_client reuse it, never a second one |

## What is not dual logic

Warning prose in doctor checks is per-check wording, not a parity surface. Generated copies from build.rs and harness-matrix are artifacts, not a second implementation. A boundary fix never adds a permanent mirrored implementation. A new shared decision needs one owner or an entry above.

## Behavior with an absent optional component

| Surface class | Result |
|---|---|
| Plain workspace | everything works (doctor reports the absence as advisory) |
| Agent lifecycle overlays | overlays yield nothing (attach stays usable) |
| Requested graph operation | typed refusal naming the component and repair (live panes stay usable) |
| Forwarded porcelain verb | the existing provisioning failure, unchanged |
| Digest | disabled-by-config and backend-unavailable are distinct states |

## Verification

`cargo test --manifest-path crates/fno/Cargo.toml product_boundary` covers the classifier. `bash tests/mux-workspace-standalone.sh` proves the standalone journey. `scripts/ci/check-product-boundaries.sh` gates the inventory. It checks the compile edge by name. It refuses a new permanent mirrored implementation without a debt entry.
