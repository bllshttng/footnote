---
name: think
description: "Reason about a design before building. Routes to design+BDD exploration (think, default), scenario/failure-mode stress testing (what-if), or a multi-persona expert debate (panel). Use when: 'think through this', 'brainstorm', 'what could go wrong', 'stress test this idea', 'convene a panel', 'should we build X'."
argument-hint: "[what-if|panel]  (bare or prose = think default; what-if: [domain] [depth] failure-modes \"scope\"; panel: [auto] [deep] \"decision\")"
requires:
  binaries:
    - "fno >= 0.1"
    - "git >= 2.0"
---

# Think

**One verb on a design.** `/think` routes to the right reasoning flow for the idea in front of you.

| Mode | What runs | Use when |
|------|-----------|----------|
| `think` (default) | design exploration + multi-perspective challenge + BDD acceptance criteria | you are shaping a feature and want a reviewable design doc |
| `what-if` | scenario / failure-mode stress test across 12 dimensions | you want to break the idea before committing to it |
| `panel` | a panel of opinionated expert personas debates the decision | a strategic call (build / pivot / prioritize) needs more than one lens |

This is a **router**, not a monolith. It parses the first argument as a mode, announces the resolved mode, then loads that mode's body and follows it in this same context. It never calls another skill at runtime (it dispatches subagents via the Task/Agent tool and loads modes via Read).

## Step 1: Resolve the mode (ALWAYS announce it)

The default mode `think` takes a **free-text design seed** (or no argument, for interactive design). The other two modes are selected by an **exact** leading keyword. Parse the first whitespace-delimited token:

- **no argument** -> mode is `think`. Print exactly: `running think (default)` and go to Step 2 in interactive mode (no seed).
- **`think`** -> mode is `think` (explicit). Print `running think (default)`. Consume the token; the remaining text is the design seed. Go to Step 2.
- **`what-if`** -> mode is `what-if`. Print `running what-if (scenario stress-test)`. The remaining tokens are what-if's own arguments (`[domain] [depth] [from-scenarios] [failure-modes] "scope" [Iterations: N]`). Go to Step 3.
- **`panel`** (or the hidden alias **`tank`**) -> mode is `panel`. Print `running panel (multi-persona debate)`. The remaining tokens are panel's own arguments (`[auto] [continue {slug}|list] [deep|shallow] [startup|adversarial] <decision>`). Go to Step 4.
- **a single bare word that is none of the above** -> this is an unknown mode (almost always a typo'd mode keyword, not a one-word design seed). Do NOT default, do NOT guess. Print:

  ```
  unknown think mode: '<token>'
  valid modes: think (default), what-if, panel
  ```

  and stop with a non-zero result (start no design, dispatch no agents). This is the locked router contract: an unknown non-empty mode never silently falls through. To seed the design with a single word, prefix the default mode explicitly (`/think think dark-mode`) or quote it (`/think "dark-mode"`).
- **any multi-token argument whose first token is none of the above** -> mode is `think` (default). Print `running think (default)` and **echo the resolved seed** (`seed: <full argument>`) so a mistyped mode keyword is visible immediately rather than silently swallowed. The entire argument is the design seed. Go to Step 2.

> Note: a quoted prose seed is the common `think` invocation (`/think "add an AI chat feature"`). The mode keywords (`what-if`, `panel`) only ever bind when they are the bare first token; a quoted phrase is never parsed for embedded mode keywords.

## Step 2: think mode (design + BDD, default)

Load [think.md](think.md) and execute it in full, in this context. That body is the canonical design-thinking flow: understand context, explore approaches, multi-perspective challenge, UI state-machine audit, the mandatory `## Failure Modes` section, and BDD acceptance criteria, then save and review the design doc.

**Self-reference (no recursion).** `think` is the running skill during its own default mode. The router loads the think flow **inline via Read** and follows it here. It must never re-invoke `/think` to reach its own default - that body is `think.md`, loaded directly.

When the think flow reaches its failure-mode step and the feature is large or risky enough to exceed the inline budget, it hands off with a copy-pasteable `/think what-if ...` line (the same router, `what-if` mode).

## Step 3: what-if mode (scenario stress-test)

Load [what-if.md](what-if.md) and execute it in full, in this context. That body is the canonical scenario-exploration loop: seed -> decompose into 12 dimensions -> iterate one situation per pass -> classify -> write the two output files. When invoked with the `failure-modes` positional modifier (the form the think flow emits when it hands off), it appends a top-level `## Failure Modes` section using the Boundaries / Errors / Invariants / Concurrency vocabulary the think flow consumes.

**Thin input is fine.** A one-line scope still runs: what-if seeds from whatever it is given and explores the highest-priority dimensions. It never crashes on sparse input - if intent is too vague it gathers context in one batched question first.

## Step 4: panel mode (multi-persona debate)

Load [panel.md](panel.md) and execute it in full, in this context. That body is the canonical think-tank flow: assemble a panel of opinionated personas, run independent analysis, debate rounds with a mandatory Devil's Advocate, reach consensus, and write a ranked-recommendation report. All personas run on the main thread (no agents spawned); the user holds a seat unless `--no-user-seat` is passed.

**Handoff, not runtime invocation (router contract override).** panel.md's report step offers downstream verbs - both the default handoff options and the optional `--chain think|plan|megawalk`. In router mode those are **presented as copy-paste handoff lines** (`/think ...`, `/blueprint ...`, `/megawalk`) for the user to run next; the panel mode never invokes another skill at runtime. Routers load modes inline and dispatch only via Task/Agent, and a self-contained install may ship the `think` folder without the chained skill present, so `/think panel --chain plan ...` resolves the recommendation and then emits the `/blueprint ...` line rather than calling it.

**Thin input is fine.** `/think panel "should we add dark mode"` (a one-line decision) still convenes the full panel - the flow gathers project context itself and never crashes on a sparse prompt.

## Multi-CLI

Claude-Code primary. All three modes need `fno` and `git`. If a dependency is missing, the mode fails loud and reports it - it never fakes a design, a stress-test, or a panel. On a CLI without `AskUserQuestion`, the interactive setup steps degrade to a single prose prompt; the rest of each flow is markdown the runtime follows directly.
