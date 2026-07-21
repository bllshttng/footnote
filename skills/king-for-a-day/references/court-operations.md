# Court operations

The operations manual for [court mode](../SKILL.md#court-mode-reign-over-the-wave).
The skill carries the *contract* (what a court king owes its wave); this reference carries the *hands* (which verb does each job, what each lifecycle state means, and the copy-paste recipes).

Court needs exactly four pane-layer primitives: **place** a teammate near the king, **inject** a next-phase prompt into a live session, **wait** on lifecycle, and **read** recent output.
The verbs below are fno's own.
In an environment whose pane layer is something other than fno mux, the crowning brief names that layer's equivalents; the *duties* are identical either way, and every ruling still lands in the graph via `fno backlog` verbs and every node is still claimed through `/fno:target`.
The pane layer owns placement, lifecycle, and I/O; fno stays the authority for identity, claims, the graph, and dispatch.

## The four primitives

| Duty | Verb | Notes |
|---|---|---|
| **Place** a teammate near the king | `fno agents spawn <name> "<payload>" --substrate pane --squad <s> --split <dir>` | Splits the target squad's active tab; min-size refusal falls back to a same-squad tab. Read the receipt for which one landed. |
| **Inject** the next phase into a live session | `fno mail send <handle> "<ruling + /fno:verb>" --from-self` | A direct send to a live pane injects as a notification it acts on this turn. Receipt-gated - see delivery truth below. |
| **Wait** on lifecycle | `fno-agents needs --json` + `fno agents top` per heartbeat | Push-first (the teammate's report mail); this is the backstop sweep. |
| **Read / triage** | `fno agents peek <handle>` | Read-only. Full-screen agents render in the alternate screen, so scrolled-off rows are unrecoverable - reads are triage, results live in artifacts and the graph. |

## Control surfaces

| Job | Verb |
|---|---|
| Spawn a teammate pane | `fno agents spawn <name> "<payload>" --substrate pane --squad <s> --split <dir> --effort <e>` |
| Message a live teammate | `fno mail send <handle> "<msg>" --from-self` |
| Resolve a handle you lost | `fno agents discovered-json` · `fno agents top` |
| Is it alive? | `fno agents peek <handle>` |
| Who is actually running | `fno agents top` |
| The needs-me fold | `fno-agents needs --json` |
| Wake an idle teammate | `fno agents resume <handle>` (then re-send) |
| End a teammate | `fno agents stop <name>` |
| Encode a ruling | `fno backlog update <id> --dispatch-verb /fno:... --dispatch-brief "..." --add-blocker <up>` |
| Land a green child | `fno pr merge <n>` (only when config permits) |

## Lifecycle state semantics

The runtime serializes three teammate states. Read them precisely - the failure that built a duplicate PR was reading *invisibility* as *death*.

- **working** - the pane has an active session doing its unit of work. No action.
- **blocked** - the teammate hit something it cannot resolve from its own scope (an open dependency, a question). Surfaces as `BlockedAnswerable` in the needs fold. Your job: read the block, rule, mail the answer back into the same session.
- **done** - finished and you have not looked at it yet (`DoneUnseen` in the needs fold). Your job: reconcile (read the artifact, rule, route, encode).

Two states that are **not** death and must never be treated as such:

- **unknown / unregistered but alive.** A teammate can finish work, ship a PR, and never register a row or send a report. Silence proves nothing. Before declaring death, `peek` the pane, check the node claim (`fno claim`), and check open PRs (`gh pr list --head <branch>`). Only a confirmed-dead pane with no claim and no PR is a corpse.
- **queued (durable), not delivered (hosted).** A mail receipt that is not `delivered (hosted)` means the message is sitting in a queue the recipient may never drain. It is not delivered. Re-resolve the handle and re-send.

## Recipes

**Spawn a teammate for a node (with the minion clause):**

```bash
fno agents spawn node-x-b3a8 "Take node x-b3a8 through /fno:think. \
When you finish a unit or block, report: \
fno mail send <king-handle> 'RESULT: <resolved|blocked|failed> | node: x-b3a8 | phase: think | context: <NN>% used | artifact: <path>' --from-self. \
Ask me by mail for anything outside your scope; never guess an executive call. \
Escalate one level at a time." \
  --substrate pane --squad epic-squad --split right --effort high
```

Capture the teammate's mail handle from the spawn receipt's `short_id` (a claude pane now carries its 8-hex jobId there).

**Route the next phase into the live session (reuse):**

```bash
fno mail send <teammate-handle> \
  "Ruling: approved, the design covers the three failure modes. \
Cross-squad: node <sibling> owns <shared-file> - do not touch it. \
Next: /fno:blueprint <node>." --from-self
```

**Hand off on context pressure (report said `context: 62% used`, trigger is 50):**

```bash
# spawn the successor FIRST, carrying the phase artifact
fno agents spawn node-x-b3a8-g2 "Continue node x-b3a8 at /fno:blueprint. \
Prior /think artifact: <path>. <minion clause>" \
  --substrate pane --squad epic-squad --split down --effort high
# ...only after the successor's session header prints:
fno agents stop node-x-b3a8
```

**Corpse check before respawn (no report, pane looks gone):**

```bash
fno agents peek node-x-7a53          # alive?
fno claim status node:x-7a53         # still claimed?
gh pr list --head feature/x-7a53     # already shipped?
# only if all three say dead/absent: respawn from the graph-encoded artifact
```

**Reconcile then encode (the ruling must survive you):**

```bash
# read the artifact, rule, then:
fno backlog update x-b3a8 --dispatch-verb /fno:target --dispatch-brief "Blueprint approved; ship the court section + short_id fix."
fno backlog update x-b3a8 --add-blocker x-7a53   # if a merge-order constraint applies
```

## Caveats

- **Qualified verbs, always.** Bare `/do`, `/think`, `/blueprint` in a mixed-plugin session can resolve to a different plugin. Use `/fno:...` in every payload, routing mail, and `--dispatch-verb`.
- **`/fno:target` is the execution verb, all sizes.** Raw `/fno:do` has no claim, no gates, no ship, no finalize. A small PR is not an exemption.
- **Lane accounting counts corpses.** Dead bg claims can starve court spawns; check `fno claim` liveness before concluding the project is saturated.
- **`done` is stamped at finalize, not merge.** The wave gate needs `gh pr view --json state` truth, and pr-watch can be silently dead - verify it at crowning.
- **Linking a plan to an unencumbered node arms the daemon within about a minute.** Wire blockers first, then link - same ordering as the pass (3a before 3b).
- **`--to-project` is anycast into a possibly-ghost inbox with a success-shaped receipt.** Court messaging uses direct handle sends only.
- **The retired `<harness>-<short-id>` mail form is refused.** Use the bare 8-hex id.
- **One input path per teammate.** The crowning brief fixes whether a teammate is driven by mail-inject or the pane layer's own prompt verb; the two never both drive one pane.

## The minion side

Every teammate spawned into a court owes its king four behaviors, stated in the spawn payload. A minion reading this owes exactly:

1. **Report** on finishing a unit or blocking - `fno mail send <king-handle> 'RESULT: ...' --from-self`. Never stop silently.
2. **Ask** the king by mail for anything outside your own scope. Guessing an executive call is a contract violation.
3. **Message peers** directly for load-bearing facts, but route any decision or routing change through the king so it lands in the graph.
4. **Escalate one level at a time** (IC -> Director -> VP -> human). A peer message is information, never authority.
