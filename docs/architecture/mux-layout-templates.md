# Mux declarative layout templates

## Scope

How a coordinator arranges a mux tab by declaring a named layout instead of scripting splits.
Covers the `fno mux layout apply` surface, the template vocabulary, slot bindings, the spec file, the idempotent reconcile, and template-tab restore.
Builds on the layout scripting substrate (the arbitrary-target split, `layout get`, and the `fno_id` to pane registry join).

## Principle

Imperative pane choreography ("go to pane 1, split down, do it again") is focus-relative, so it breaks the instant a human touches the mux mid-script.
A template call declares the shape ("one main pane left, three stacked right") and the server computes the splits, anchored to explicit tab and slot ids.
The result is reproducible and idempotent: re-applying the same spec is a no-op that reuses the existing panes.

## The surface

```
fno mux layout apply --template main-left \
    --slot fno:af4dac55 --slot - --slot - --slot - \
    [--squad <s>] [--tab <sel>] [--focus] [--json]

# or from a persisted spec file (the same LayoutSpec):
fno mux layout apply --spec path/to/layout.toml [--squad <s>] [--tab <sel>] [--json]
```

`--slot fno:<id>` (or a bare `<id>`) binds a session; `--slot -` is an explicit empty shell.
Slot order is template order.
`--tab` addresses the target (default the active tab; `new` forces a fresh one); `--focus` opts into moving the viewer's focus to slot 0 (off by default, so a scripted apply never steals focus).
`--json` emits the per-slot receipt so a script captures the created pane ids from the response rather than predicting them.

The spec file is TOML, matching the `.fno` config idiom:

```toml
template = "main-left"
slots = ["fno:af4dac55", "-", "-", "-"]
```

The flag form and the file form assemble the same `LayoutSpec`, and the file form IS the persisted spec, so apply and restore share one struct and one code path.

## The template vocabulary

Fixed by the epic; a general layout DSL is out of scope.
Every branch splits its children evenly (a human resizes with the existing draggable dividers).

| Template | Arity | Topology (over slot indices) |
|---|---|---|
| `main-left` | k >= 2 | `H[ s0, V[ s1.. ] ]` |
| `main-top` | k >= 2 | `V[ s0, H[ s1.. ] ]` |
| `row-thirds` | k == 3 | `H[ s0, s1, s2 ]` |
| `col-thirds` | k == 3 | `V[ s0, s1, s2 ]` |
| `grid-2x2` | k == 4 | `V[ H[ s0, s1 ], H[ s2, s3 ] ]` |

`topology(name, k)` is a pure function from a name and a slot count to a pane tree of slot-indexed leaves, unit-tested in isolation.
A fixed-arity template with the wrong slot count refuses before any mutation; a variadic `main-*` with `k == 2` is legal (one main, one secondary), and `k < 2` is an arity error.

## Arrange-only

Apply binds a slot to a session that already exists and arranges its pane.
It does not spawn a worker.
An unbound or dead-bound slot becomes a bare shell, never a spawned agent; spawning into slots arrives with the roles group.
Bindings are by `fno_id` only for now; the binding field is shaped to admit a `role` variant later without a wire break.

## The reconcile: idempotence and the never-kill invariant

An apply owns the entire target tab (a template tab is agent-managed by contract, like a generated file; a manual split inside it is reconciled away on the next apply).

Arity and fit are checked before any mutation.
A template whose slots cannot tile the tab above the minimum pane size refuses atomically, naming the overflowing slots, and the tab is left completely unchanged.

Past that, the engine diffs by binding identity, not by pane position:

- **A bound session's live pane** is reused in place, its PTY untouched, relocated to its new position in the shape. If it currently lives in another tab, it is detached from there (the PTY kept) and its emptied source tab is removed.
- **A shell or unbound slot** reuses one of the target tab's spare shells (drained in tree order, so a re-apply reassigns each shell to the same slot and the tree comes back byte-identical), or spawns one when none remain.
- **A dropped shell** the new spec no longer needs is closed.

The load-bearing invariant: a live pane bound to a live session is never killed by a re-apply, only relocated.
A per-slot receipt reports each outcome (`reused` / `shell` / `unbound` / `spawn-failed`); a per-slot spawn failure is a reported partial success that leaves the created panes standing, never a rollback that kills live panes.

Concurrency needs no lock: every apply helper is synchronous, so the mux's single core loop runs a whole apply as one atomic turn and a concurrent apply cannot interleave.

## Persistence and restore

The spec is persisted per named template tab (an additive, default-tolerant store field, so a store written before this feature loads unchanged).
On server restart, a template-managed tab is rebuilt by re-applying its stored spec, not by the one-tab-per-member fallback; a session that did not survive the restart restores its slot as a shell, never a duplicate.
An unnamed tab has no durable key and stays live-only.

Live re-binding at cold start is eventually consistent: the off-loop registry reader has not populated the in-memory agent catalog at restore time, so bindings that cannot resolve yet restore as shells and bind on a later re-apply once the sessions register.

## Local layout graft (`fno mux layout graft`)

`layout apply` owns and persists an entire tab.
`layout graft` owns only the subtree it substitutes at one anchor pane and is session-local: it is never written to the template store, and it leaves every pane outside that subtree exactly where it was.
The two verbs have deliberately different ownership scopes so a local automation cannot accidentally reconcile away an operator's tab.

```
fno mux layout graft --at current \
    --template col-thirds --slot anchor --slot fno:abcd1234 --slot fno:ef901234

# or a typed spec file (TOML or JSON), the same AnchoredLayoutSpec:
fno mux layout graft --at current --spec path/to/stack.toml [--focus] [--json]
```

`--at current` resolves the anchor from `FNO_PANE` (run the command inside a mux pane); a numeric `--at <pane>` pins a specific pane.
The graft replaces that anchor's leaf with the spec's subtree while preserving the enclosing tab.
A named template compiles into the same typed tree as an arbitrary spec (`template_to_tree`), so the macro form and the file form share one representation.

### The typed schema

The arbitrary topology is a recursive `LayoutTreeSpec`: a `Slot(name)` leaf, or a `Split { axis, children: [(weight, tree)] }`.
Slots bind to exactly one of `Anchor` (the calling pane, which survives inside the subtree), `Fno(id)` (a live session's pane, reused with its PTY untouched), or `Shell` (an empty pane the graft spawns).
Raw commands in leaves are out of scope; agent launch stays in the agents subsystem.

The spec carries a version (`version: 1`).
Weights must be finite and positive and are normalized before geometry validation.
The tree is bounded at 24 leaves and depth 16 so malformed or machine-generated input cannot create unbounded recursion or unusable geometry.
Exactly one slot must bind `Anchor`, and every slot name is referenced exactly once in the tree; `validate_anchored_spec` enforces all of this before any pane is touched.

```toml
version = 1
[tree]
axis = "vertical"
[[tree.children]]
weight = 0.5
[tree.children.tree]
slot = "anchor"
[[tree.children]]
weight = 0.5
[tree.children.tree]
axis = "horizontal"
[[tree.children.tree.children]]
weight = 0.5
[tree.children.tree.children.tree]
slot = "reviewer"
[[tree.children.tree.children]]
weight = 0.5
[tree.children.tree.children.tree]
slot = "tester"

[[slots]]
name = "anchor"
binding = "anchor"
[[slots]]
name = "reviewer"
binding = { fno = "abcd1234" }
[[slots]]
name = "tester"
binding = "shell"
```

### Atomicity, rollback, and the never-kill invariant

Graft validation and materialization are atomic from the operator's perspective.
The server validates the spec, resolves the bindings, and probes fit before it spawns anything; an unavailable `Fno` binding is refused rather than degraded to a shell, and two slots resolving to one pane are refused.
Shell slots spawn only after pure validation passes; if any shell spawn fails, every transaction-owned shell is reaped and the tree is left byte-unchanged (no partial subtree).
A reused live pane is detached from its old leaf and rehomed into the subtree with its PTY untouched; a live bound PTY is never killed.
The whole turn runs in the mux's single serialized core loop, so concurrent grafts at one anchor commit at most one valid tree and the second refuses.
Graft never moves the viewer's focus and never persists state, so the reusable artifact is the spec file rather than hidden ownership metadata.

