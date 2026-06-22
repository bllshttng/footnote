# footnote - f[no]

**Set a target and walk away. Say f[no] to mostly done.**

I got tired of watching coding agents stop at "looks done." Claude, Codex, Gemini, all of them[^1]: they write the code, the diff looks right, they hand it back, and then it's on me to find out at 11pm that the tests were never green. So I said f[no].

I built the loop that doesn't stop there. It keeps going until the PR is merged and CI is actually green, and when it isn't sure what to build next it walks the backlog and works it out.

Claude Code is my hammer, and I see a shit ton of nails. So I kept swinging: I got it running across providers, then got a Claude agent and a Codex agent mailing each other across terminal panels while they worked.

Really, I built this to ship my own projects faster. One of them became a business. footnote is the part underneath all of it. The rest of this is what it does.

## Install

**Agent integration** - the `/fno:*` commands and the walk-away workflow. Each AI CLI installs its own integration; the Claude plugin also bundles the `fno` CLI (its postinstall puts it on your PATH in a new session), so you don't need a separate CLI install:

```
Claude Code:   /plugin marketplace add bllshttng/footnote
               /plugin install fno@footnote
Gemini CLI:    gemini extensions install https://github.com/bllshttng/footnote
Codex CLI:     codex plugin marketplace add bllshttng/footnote   # then enable
```

Then configure with `/fno:setup` and point it at a feature:

```
/fno:target "add OAuth login"
```

**CLI only** - just the `fno` binary, for scripting, CI, or driving footnote yourself. This installs `fno` but **not** the `/fno:*` slash commands (those need the agent integration above):

```
curl -fsSL fno.sh | sh          # one-liner
uv tool install fno             # uv  (or: pip install fno)
brew install bllshttng/fno/fno  # homebrew
cargo install fno               # cargo
```

Local-clone install and path configuration: [docs/getting-started.md](docs/getting-started.md).

## Set it up

Configure a project from a Claude Code session with `/fno:setup`, or from the terminal with no agent:

```
fno setup wizard              # asks the few real per-project decisions, writes them validated
```

Read or edit any setting directly: `fno config get|set|unset <key>` (atomic and schema-checked). Defaults are sensible; you can skip straight to running.

## Run it

Two ways to ship a feature; both end in a merged PR. Then scale up.

**Autopilot.** Point the `target` skill at a feature description or a backlog node, then walk away:

```
/target "add OAuth login"     # think -> plan -> code -> review -> ship
/target fno-a3f9              # by backlog node id
```

It runs the whole loop with or without you watching, and prints the PR URL when it ships. You don't have to pass a size; it runs a sensible default.

**Hands-on.** Drive the design yourself, then hand off the build:

```
/think "OAuth login"          # explore the design space
/blueprint "OAuth login"      # plan it, then hands off to a fresh /target thread
```

`/blueprint` spawns the build in its own Claude Code thread so your planning context stays clean.

**Keep going.** `/megawalk` chews through the whole backlog until it's done or out of budget, walking the dependency graph to pick what ships next.

**Loop harnesses together.** Spawn an agent on another provider and work alongside it:

```
fno agents spawn reviewer "review the diff" -p codex   # a Codex agent on this repo
fno agents ask reviewer "what did you find?"            # message it; it works on its own
```

Each agent runs its own loop and they coordinate over a message bus. Claude, Codex, and Gemini, one project.

**Also in the box:**

- A six-agent review panel reads the diff before it ships, analyzing integration and UX-flow, not just unit tests, via `/review`.
- Provider rotation with failover and per-model lockout, so a flaky or rate-limited model doesn't stall the loop.
- `/megatron` runs the same loop across a fleet of repos for cross-project missions.

## What it is

An orchestration loop for shipping software, packaged as a plugin. Most loops re-run a prompt or fire on a timer; this one won't let a session stop until external truth says so: the PR exists, CI is actually green, and every required reviewer has signed off with nothing blocking. It reasons over a dependency graph to decide what to ship next, and survives session compactions, provider hiccups, and your skepticism. No vibes-based "done."

## What it isn't

Not a sandbox. Not a babysitter. Not a hero-video launch. It runs your plans with your credentials on your machine and assumes you meant what you asked for. [docs/security-posture.md](docs/security-posture.md) draws the trust boundary. Read it before you point this at anything you'd hate to hand a robot.

## Docs

- [Getting started](docs/getting-started.md): install, setup, and the commands you'll actually run
- [Target pipeline](docs/guides/target.md): the loop: flags, gates, cross-project, resume
- [Think and plan](docs/guides/think-and-plan.md): design exploration and planning
- [PR lifecycle](docs/guides/pr-lifecycle.md): review, create, check, merged
- [Agents quickstart](docs/guides/agents-quickstart.md): spawn and message peer agents
- [Best practices](docs/best-practices.md): reliable, cost-bounded runs
- [Troubleshooting](docs/troubleshooting.md): when it breaks
- [Security posture](docs/security-posture.md): what it will and won't do
- [Architecture](docs/architecture/control-plane-loop.md): how completion is decided
- `AGENTS.md`: multi-CLI setup notes (Claude Code, Codex, Gemini)

Skills are portable markdown. Grab pieces without the full plugin: `npx skills add bllshttng/footnote/<skill>`. Full list under [skills/](skills/).

## Requirements

macOS (Apple Silicon or Intel), Linux (x86_64 / arm64), or Windows via [WSL2](docs/getting-started.md#windows-wsl2). Python 3.11+, `jq`, and `gh` (authenticated). Optional: Playwright for browser testing.

## Companions

[RTK](https://github.com/rtk-ai/rtk) compresses shell output 60-80% so long loops don't drown in their own context. `/fno:setup` detects and wires it.

## Status

Pre-launch (open-source readiness). Screencast coming. Built in the open and dogfooded daily: footnote ships footnote.

Inspired by Geoffrey Huntley's [Ralph Wiggum pattern](https://ghuntley.com/ralph/).

## License

Apache-2.0, [Jason Noah Choi](https://github.com/bllshttng)

[^1]: The autonomous loop is most battle-tested on Claude Code. Codex and Gemini run it too; Hermes and Openclaw can as well, with the least mileage there.
