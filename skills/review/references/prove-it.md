<!-- style-exception: the report shape and the probe rules are taken from the /verify skill spec; rewriting verbatim-contract text to satisfy sentence-length rules would break the provenance that makes this the runtime-evidence contract. -->

# prove-it: runtime evidence

Code-reading review of a stable diff re-litigates indefinitely: its value decays and it never terminates. Runtime observation terminates - the route returns the header or it does not. `prove-it` is fno's verify: drive the changed code at its real surface, push on it with probes, capture what the artifact itself says.

**Verification is runtime observation.** You build the app, run it, drive it to where the changed code executes, and capture what you see. That capture is your evidence. Nothing else is.

**Do not run tests. Do not typecheck.** Running them here proves you can run CI, not that the change works. Not as a warm-up, not "just to be sure", not as a regression sweep after.

**Do not import-and-call.** `from x import foo` then calling `foo(x)` is a unit test you wrote; the app never ran. Whatever calls `foo` ends at a CLI, a socket, or a window. Go there.

## Find the change

In a git repo, establish the full range - a branch may be many commits, or the change may be uncommitted:

```bash
git log --oneline @{u}..              # count commits
git diff @{u}.. --stat                # full range, not HEAD~1
git diff HEAD --stat                  # uncommitted work
gh pr diff                            # in a PR context
```

State the commit count. **The diff is ground truth. Any description is a claim about it.** Read both; if they disagree, that is a finding.

## Find the surface

The surface is where a user - human or programmatic - meets the change: CLI terminal, server socket, GUI pixels, library package boundary, agent, CI workflow. **An internal function is not a surface.** Something calls it and that caller ends at one; follow it there. No runtime surface at all (docs-only, types with no emit) reports **SKIP - no runtime surface**, one line why. Do not run tests to fill the space.

## Get a handle

Check `.claude/skills/` for a `verifier-*` skill first - it is the repo's evidence-capture protocol. A `run-*` skill without a verifier gives build/launch primitives. Neither: cold start from the README, timebox about fifteen minutes, and BLOCKED with exactly where it stopped if you do not get through.

## Drive it

Smallest path that makes the changed code execute: changed a flag, run with it; changed a handler, hit that route; changed error handling, trigger the error. **Read your plan back before running - if every step is build/typecheck/run-test, you have planned a CI rerun, not a verification.**

## Push on it (the load-bearing part)

The claim checking out is step one. Probe around it, at the same surface: empty values, passed twice, combined with a conflicting flag, wrong method, malformed body, stale state underneath, do it twice, resize the pane mid-op. Pick the ones the change points at; stop when the obvious adjacents are covered or something is worth a warning.

A probe that finds nothing is still a step and still gets its line - it tells the author what WAS covered, which a bare PASS cannot. **A Steps list that is all pass and no probe is a happy-path replay: fno's prove-it REFUSES to report PASS over it** (`scripts/validate-prove-it.sh` enforces this; /verify would still say PASS with a note - this is the deliberate hardening).

## Capture

Stdout, response bodies, screenshots, pane dumps. Captured output is evidence; memory is not. Something unexpected: capture it, note it, decide if it is the change or the environment. Unrelated breakage is a finding, not noise. Isolate shared process state - `tmux -L`, bound ports, `mktemp -d`.

## Report

Inline, final message:

```
## Verification: <one-line what changed>
**Verdict:** PASS | FAIL | BLOCKED | SKIP
**Claim:** <what it is supposed to do - your read of the diff; note any mismatch>
**Method:** <how you got a handle; what you launched>
### Steps
1. ✅/❌/⚠️/🔍 <what you did to the running app> -> <what you observed>
   <evidence: the app's own output, captured>
2. 🔍 <probe> -> <result>
### Findings
- each probe gets a line here even when it held
fno-prove-it: {"verdict":"<PASS|FAIL|BLOCKED|SKIP>","claim":"<one line>"}
```

Build/install/checkout are setup, not steps. The terminal `fno-prove-it:` JSON line is the machine record: `validate-prove-it.sh` reads it, refuses a PASS with no 🔍 marker in Steps, and passes FAIL/BLOCKED/SKIP through untouched.

Verdicts, and what each is worth:

- **PASS** - you ran the artifact, the change did what it should at its surface, and at least one marked probe pushed off the happy path.
- **FAIL** - you ran it and it does not work, or it breaks something else, or claim and diff disagree materially.
- **BLOCKED** - you could not reach a state where the change is observable (build broke, env missing a dep). Not a verdict on the change.
- **SKIP** - no runtime surface exists. Nothing went wrong; there is nothing to run.

No partial pass: "3 of 4 passed" is FAIL until the fourth passes or is explained away. When in doubt, FAIL - a false PASS ships broken code; a false FAIL costs one more look.

## Gate integration

PASS maps to a satisfied `done_probe`; FAIL blocks; BLOCKED and SKIP carry NO verdict and read as UNANSWERED - they hold the gate without naming a failure. Probe declarations may carry their claim as a trailing ` # <claim>` comment; the row records it. A probe run through `fno-agents probe-run` must emit output to count: exit 0 with nothing captured reads SKIP, never pass.
