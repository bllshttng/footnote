---
name: think
description: "Reason about a design before building. Routes to design+BDD exploration (think, default), scenario/failure-mode stress testing (what-if), or a multi-persona expert debate (panel). Use when: 'think through this', 'brainstorm', 'what could go wrong', 'stress test this idea', 'convene a panel', 'should we build X'."
argument-hint: "[what-if|panel|dispatch]  (bare or prose = think default; what-if: [domain] [depth] failure-modes \"scope\"; panel: [auto] [deep] \"decision\"; dispatch: <node-id>)"
requires:
  binaries:
    - "fno >= 0.1"
    - "git >= 2.0"
---

# Think

**One verb on a design.** `/think` routes to the right reasoning flow for the idea in front of you.

When `$CODEX_THREAD_ID` is nonblank, before any routing or work, Print exactly once:
`codex posture: think uses this Codex conversation as the source; dispatch defaults to Claude bg; explicit non-Claude providers are refused.`

| Mode | What runs | Use when |
|------|-----------|----------|
| `think` (default) | design exploration + multi-perspective challenge + BDD acceptance criteria | you are shaping a feature and want a reviewable design doc |
| `what-if` | scenario / failure-mode stress test across 12 dimensions | you want to break the idea before committing to it |
| `panel` | a panel of opinionated expert personas debates the decision | a strategic call (build / pivot / prioritize) needs more than one lens |
| `dispatch` | hand a named node to a bg `/think` carrying THIS conversation's context | you are mid-discussion about an fno node and want a deep think to pick it up off the main thread |

This is a **router**, not a monolith. It parses the first argument as a mode, announces the resolved mode, then loads that mode's body and follows it in this same context. It never calls another skill at runtime (it dispatches subagents via the Task/Agent tool and loads modes via Read).

## Step 1: Resolve the mode (ALWAYS announce it)

The default mode `think` takes a **free-text design seed** (or no argument, for interactive design). The other two modes are selected by an **exact** leading keyword. Parse the first whitespace-delimited token:

- **no argument** -> mode is `think`. Print exactly: `running think (default)` and go to Step 2 in interactive mode (no seed).
- **`think`** -> mode is `think` (explicit). Print `running think (default)`. Consume the token; the remaining text is the design seed. Go to Step 2.
- **`what-if`** -> mode is `what-if`. Print `running what-if (scenario stress-test)`. The remaining tokens are what-if's own arguments (`[domain] [depth] [from-scenarios] [failure-modes] "scope" [Iterations: N]`). Go to Step 3.
- **`panel`** (or the hidden alias **`tank`**) -> mode is `panel`. Print `running panel (multi-persona debate)`. The remaining tokens are panel's own arguments (`[auto] [continue {slug}|list] [deep|shallow] [startup|adversarial] <decision>`). Go to Step 4.
- **`dispatch`** -> mode is `dispatch`. Print `running dispatch (conversational /think handoff)`. The remaining token is the target node id/slug. Go to Step 5.
- **a single bare token that is none of the above AND strict-resolves in the graph** -> mode is `think`, seeded by that node. This is the common node-seed invocation (`/think x-4af4`, `/think dashless-spawn`, `/think next`) that no longer needs the redundant `think` keyword. A bare hex like `/think 4af4` also resolves, but ONLY for the built-in `ab-<8 lowercase hex>` id form (it re-prefixes `ab-`); a repo on a different configured id prefix/width (e.g. `x-4af4`) resolves via the full id or the slug, not the bare hex. Resolve it with STRICT resolution (exact id, bare hex, exact slug, or the literal `next` only - never fuzzy):
  - token is the literal `next` -> `fno backlog next` (already strict by construction).
  - else -> `fno backlog get --strict "<token>"`.

  If resolution succeeds (exit 0), print `seed: <token> (graph-resolved: "<title>")`, take the node as the design seed, and go to Step 2. If it exits non-zero (no exact match, OR the resolver errored - graph missing/lock contention), surface the resolver's stderr and fall through to the unknown-mode stop below - never guess a seed. Strictness lives in the resolver, NOT in a shape regex here: a slug-shaped typo (`panle` near a `panel-mode` node) is indistinguishable from a real slug by shape, so `--strict` (exact-only, no describe-it) is what separates them - `panle` misses and correctly hits the unknown-mode stop.
- **a single bare word that is none of the above and does NOT strict-resolve** -> this is an unknown mode (almost always a typo'd mode keyword, not a one-word design seed). Do NOT default, do NOT guess. Print:

  ```
  unknown think mode: '<token>'
  valid modes: think (default), what-if, panel, dispatch
  ```

  and stop with a non-zero result (start no design, dispatch no agents). This is the locked router contract: an unknown non-empty mode never silently falls through. To seed the design with a single word, prefix the default mode explicitly (`/think think dark-mode`) or quote it (`/think "dark-mode"`).
- **any multi-token argument whose first token is none of the above** -> mode is `think` (default). Print `running think (default)` and **echo the resolved seed** (`seed: <full argument>`) so a mistyped mode keyword is visible immediately rather than silently swallowed. The entire argument is the design seed. Go to Step 2.

> Note: a quoted prose seed is the common `think` invocation (`/think "add an AI chat feature"`). The mode keywords (`what-if`, `panel`) only ever bind when they are the bare first token; a quoted phrase is never parsed for embedded mode keywords.

## Step 2: think mode (design + BDD, default)

Load [think.md](references/think.md) and execute it in full, in this context. That body is the canonical design-thinking flow: understand context, explore approaches, multi-perspective challenge, UI state-machine audit, the mandatory `## Failure Modes` section, and BDD acceptance criteria, then save and review the design doc.

**`$TARGET_BRIEF` (US3):** when a dispatcher launched this `/think` on a node via `dispatch_verb: /think`, it may set `$TARGET_BRIEF` in the environment - a plain-text brief (capped at 8 KB, carried via env not the command line) with the scope/"why" the dispatcher wanted explored. If it is set, fold it in as design context; treat it as guidance, not a command to run.

**Self-reference (no recursion).** `think` is the running skill during its own default mode. The router loads the think flow **inline via Read** and follows it here. It must never re-invoke `/think` to reach its own default - that body is `references/think.md`, loaded directly.

When the think flow reaches its failure-mode step and the feature is large or risky enough to exceed the inline budget, it hands off with a copy-pasteable `/think what-if ...` line (the same router, `what-if` mode).

## Step 3: what-if mode (scenario stress-test)

Load [what-if.md](references/what-if.md) and execute it in full, in this context. That body is the canonical scenario-exploration loop: seed -> decompose into 12 dimensions -> iterate one situation per pass -> classify -> write the two output files. When invoked with the `failure-modes` positional modifier (the form the think flow emits when it hands off), it appends a top-level `## Failure Modes` section using the Boundaries / Errors / Invariants / Concurrency vocabulary the think flow consumes.

**Thin input is fine.** A one-line scope still runs: what-if seeds from whatever it is given and explores the highest-priority dimensions. It never crashes on sparse input - if intent is too vague it gathers context in one batched question first.

## Step 4: panel mode (multi-persona debate)

Load [panel.md](references/panel.md) and execute it in full, in this context. That body is the canonical think-tank flow: assemble a panel of opinionated personas, run independent analysis, debate rounds with a mandatory Devil's Advocate, reach consensus, and write a ranked-recommendation report. All personas run on the main thread (no agents spawned); the user holds a seat unless `--no-user-seat` is passed.

**Handoff, not runtime invocation (router contract override).** panel.md's report step offers downstream verbs - both the default handoff options and the optional `--chain think|plan|megawalk`. In router mode those are **presented as copy-paste handoff lines** (`/think ...`, `/blueprint ...`, `/megawalk`) for the user to run next; the panel mode never invokes another skill at runtime. Routers load modes inline and dispatch only via Task/Agent, and a self-contained install may ship the `think` folder without the chained skill present, so `/think panel --chain plan ...` resolves the recommendation and then emits the `/blueprint ...` line rather than calling it.

**Thin input is fine.** `/think panel "should we add dark mode"` (a one-line decision) still convenes the full panel - the flow gathers project context itself and never crashes on a sparse prompt.

## Step 5: dispatch mode (conversational /think handoff)

You are mid-conversation about an fno-touched node and want a deep `/think` to pick it up off the main thread, carrying THIS conversation as context. One verb hands the node to a fully interactive `claude --bg` `/think` worker that runs beside this thread in its own agents-view row. Codex and Gemini may supply the source conversation, but this verb does not degrade to a one-shot non-Claude headless draft because that substrate has no live think-session receipt.

This mode is **explicit-only**: it dispatches ONLY when you invoke it. There is no auto-grep detector that volunteers a dispatch when you merely mention a node (the false-positive class that bit prior detectors - the offer-on-detection heuristic is a deliberate later layer).

The whole mechanism is the `fno think dispatch` verb - do NOT hand-assemble a spawn. Resolve the node token, then shell:

```bash
fno think dispatch <node-id>
```

The verb reads the live transcript pointer from the first non-empty marker in this shared precedence: `$CODEX_THREAD_ID` > `$CLAUDE_CODE_SESSION_ID` > `$CODEX_SESSION_ID` > `$GEMINI_SESSION_ID`, plus the current cwd. It resolves the node in the graph and routes through the shared dispatch core (reason-scoped dedup token, per-day firehose ceiling, forward stamp, single decision event, strict non-fatality). The source pointer may therefore identify a Codex conversation, but the `/think` worker still uses the Claude `bg` fallback; an explicit non-Claude provider is refused before spawn. Relay the verb's output verbatim - it prints the spawned worker's id and an `fno agents watch` hint, or a one-line skip reason (e.g. dedup / daily-cap). If no node token was given, print `dispatch needs a node id: /think dispatch <node-id>` and stop. If the verb exits non-zero, surface its stderr; never fabricate a launch.

## Multi-CLI

Claude-Code primary. All three modes need `fno` and `git`. If a dependency is missing, the mode fails loud and reports it - it never fakes a design, a stress-test, or a panel. On a CLI without `AskUserQuestion`, the interactive setup steps degrade to a single prose prompt; the rest of each flow is markdown the runtime follows directly.
