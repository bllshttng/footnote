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
| **Place** a teammate near the king | `fno agents spawn --name <n> "<payload>" --substrate pane --squad <s> --split <dir>` | Splits the target squad's active tab; min-size refusal falls back to a same-squad tab. Read the receipt for which one landed. |
| **Inject** the next phase into a live session | `fno mail send <handle> "<ruling + /fno:verb>" --from-self` | A direct send to a live pane injects as a notification it acts on this turn. Receipt-gated - see delivery truth below. Auto-wrapped in the `<fno_mail>` envelope; a raw pane-layer prompt is not - see the envelope rule below. |
| **Wait** on lifecycle | `fno agents top` + `fno agents peek <handle>` per heartbeat | Push-first (the teammate's report mail); this is the backstop sweep. `top` = who is alive; `peek` = is a quiet pane done/blocked/dead. `fno-agents needs --json` is a separate loop-wedge signal, not pane completion. |
| **Read / triage** | `fno agents peek <handle>` | Read-only. Full-screen agents render in the alternate screen, so scrolled-off rows are unrecoverable - reads are triage, results live in artifacts and the graph. |

## The `<fno_mail>` envelope, on every lane

Every agent-to-agent payload carries the `<fno_mail>` envelope - king to teammate, teammate to teammate, on every lane. The reason is a transcript-safety one: an injected message lands in the recipient's transcript as *user-role* text, indistinguishable from the human at the keyboard, and the envelope is the only marker that says "an agent said this." An unwrapped ruling impersonates the maintainer.

- **`fno mail send` wraps automatically.** Nothing to do; the ruling is already marked.
- **A pane-layer prompt verb does not.** If the crowning brief routes you through the pane layer's own prompt/send verb instead of `fno mail`, include the wrapper in the text yourself:

  ```
  <fno_mail from="<your-handle>" to="<teammate-handle>">Ruling: approved. Next: /fno:blueprint <node>.</fno_mail>
  ```

## Control surfaces

| Job | Verb |
|---|---|
| Spawn a teammate pane | `fno agents spawn --name <n> "<payload>" --substrate pane --squad <s> --split <dir> --effort <e>` |
| Anoint a sub-king at spawn | `fno agents spawn --name <n> "<payload>" --substrate pane --crown level=<N>,scope=<scope>` |
| Coronate a running session in place | `fno agents crown <handle> --scope <scope> [--level N]` (scope = epic/project/node id; level 0..2) |
| Read your own crown | `fno whoami` (prints a `crown:` line when your row holds one) |
| Message a live teammate | `fno mail send <handle> "<msg>" --from-self` |
| Resolve a handle you lost | `fno agents discovered-json` · `fno agents top` |
| Is it alive? | `fno agents peek <handle>` |
| Who is actually running | `fno agents top` |
| The loop-wedge fold | `fno-agents needs --json` (review_wedged / budget_stop; NOT pane completion) |
| Wake an idle teammate | `fno agents resume <handle>` (then re-send) |
| Close a teammate pane | `fno mux pane kill` (a mux row's short_id is empty, so `fno agents stop` refuses it) |
| End a bg/daemon worker | `fno agents stop <name>` |
| Encode a ruling | `fno backlog update <id> --dispatch-verb /fno:... --dispatch-brief "..." --add-blocker <up>` |
| Land a green child | `fno pr merge <n>` (only when config permits) |

## Lifecycle state semantics

The runtime serializes three teammate states. Read them precisely - the failure that built a duplicate PR was reading *invisibility* as *death*.

- **working** - the pane has an active session doing its unit of work. No action.
- **blocked** - the teammate hit something it cannot resolve from its own scope (an open dependency, a question). Surfaces as a `BlockedAnswerable` badge in the mux sideline; confirm with `fno agents peek <handle>`. Your job: read the block, rule, mail the answer back into the same session.
- **done** - finished and you have not looked at it yet, a `DoneUnseen` badge in the mux sideline (confirm with `peek`; `fno-agents needs` does NOT report it). Your job: reconcile (read the artifact, rule, route, encode).

Two states that are **not** death and must never be treated as such:

- **unknown / unregistered but alive.** A teammate can finish work, ship a PR, and never register a row or send a report. Silence proves nothing. Before declaring death, `peek` the pane, check the node claim (`fno claim`), and check open PRs (`gh pr list --head <branch>`). Only a confirmed-dead pane with no claim and no PR is a corpse.
- **queued (durable), not confirmed delivered.** A mail receipt that is not `delivered (hosted)` is sitting in a queue the recipient may never drain - treat it as undelivered. But `peek` the handle first: a `queued (durable)` can be a timed-out live inject that already landed. Re-resolve and re-send only if the peek shows it absent.

## Recipes

**Spawn a teammate for a node (with the minion clause):**

Assemble the payload with a quoted heredoc so the clause's single and double quotes pass through literally (the clause carries single quotes in its `RESULT:` line and a double quote in `<help reason="...">`; a plain double-quoted payload terminates at that inner double quote and splits the argument list during spawn):

```bash
read -r -d '' payload <<'CLAUSE' || true   # read -d '' exits 1 at EOF; absorb it so set -e does not abort
Take node x-b3a8 through /fno:think.
<minion clause - paste verbatim from references/minion-clause.md>
CLAUSE
fno agents spawn --name node-x-b3a8 "$payload" --substrate pane --squad epic-squad --split right --effort high
```

The `<minion clause>` is the canonical block in [minion-clause.md](minion-clause.md), not something you compose here - that is the whole point of the template. Capture the teammate's mail handle from the spawn receipt's `short_id` (a claude pane now carries its 8-hex jobId there).

**Route the next phase into the live session (reuse):**

```bash
fno mail send <teammate-handle> \
  "Ruling: approved, the design covers the three failure modes. \
Cross-squad: node <sibling> owns <shared-file> - do not touch it. \
Next: /fno:blueprint <node>." --from-self
```

**Hand off on context pressure (report said `context: 62% used`, trigger is 50):**

```bash
# spawn the successor FIRST, carrying the phase artifact - same quoted-heredoc
# assembly as the primary spawn (the clause's single and double quotes need it here too)
read -r -d '' payload <<'CLAUSE' || true
Continue node x-b3a8 at /fno:blueprint. Prior /think artifact: <path>.
<minion clause - paste verbatim from references/minion-clause.md>
CLAUSE
fno agents spawn --name node-x-b3a8-g2 "$payload" \
  --substrate pane --squad epic-squad --split down --effort high
# ...only after the successor's session header prints, close the predecessor
# PANE (a mux row -> fno mux pane kill, not fno agents stop). Its <session>:<pane_id>
# ref is in the mux field of `fno agents list --json`:
ref=$(fno agents list --json | jq -r '.agents[] | select(.name=="node-x-b3a8") | "\(.mux.session):\(.mux.pane_id)"')
fno mux pane kill "$ref"
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

Every teammate spawned into a court owes its king four behaviors - report, ask, message-peers, escalate - stated in the spawn payload. The canonical, pasteable form (report line, delivery doctrine, `context: NN% used` field) is [minion-clause.md](minion-clause.md); it is the single source, and both this reference and the SKILL body point to it rather than restating it, so it cannot fork.
