# Test hermeticity: neutralising ambient state as a class

CI runs with a clean `HOME`.
That single fact makes the suite structurally blind in one direction and mute in the other.

A test that **reads** the developer's ambient state passes in CI forever, because the value it reads is always absent there.
A test that **depends** on ambient state fails in CI and passes locally, so the developer sees a red they cannot reproduce.
Every specimen of either kind has been found by a human noticing a red suite, never by the thing we trust to catch things.

## The measurement that rules out a per-test pin

Three specimens landed on 2026-08-11, from three workers not looking for the same thing, and each leaked through a different channel:

| Channel | What leaked |
|---------|-------------|
| An env var the command itself resolves | a hook suite sandboxed `HOME`, the state dir and the config, but not `CLAUDE_CODE_SESSION_ID`, which its own `--to-self` resolves |
| The config chain | a dispatch test resolved a substrate through the ambient config chain, so any developer with a routed codex got a deterministic red |
| The real state ledger | a shell test read the developer's actual deferred carve-outs and refused a node it had just minted |

Each got a per-test pin. That is not the fix, and the numbers say why:

- `fno` source reads **124 distinct `FNO_*` environment variables**.
- It reads **49 distinct non-`FNO_*` ones**.
- The pytest conftest pinned **five** of them by name.

The author of that conftest sandboxed the four channels they could think of and missed the fifth.
A sixth entry on an allowlist facing a surface that size is the same defect with more ceremony.

## Move 1: deny by default

`cli/src/fno/hermetic.py` owns the surface and two operations over it.

`FNO_*` and `TARGET_*` are swept as **classes**. A 125th `FNO_*` var added to source neutralises without editing the module.
`TARGET_*` earns the same treatment because a live `/target` session exports `TARGET_INPUT`, `TARGET_PLAN_PATH`, `TARGET_SIZE` and more, so a suite run inside one inherits that session's parameters.

Session identity is **imported** from `harness_identity.AMBIENT_IDENTITY_ENV`, never retyped.
That tuple is already derived from `HARNESS_SESSION_MARKERS` because a hand-maintained copy had previously lost `CLAUDE_SESSION_ID`.

Everything else is a short, justified list, and `cli/tests/unit/test_ambient_surface.py` is what keeps it from rotting: it walks every env read in `cli/src` and `crates`, and fails on a name classified neither `ambient` nor `environment`, naming the file that reads it.
Writing the decision down is the whole job; both answers are fine.
On its first run it caught `$CLI` in `loop_dispatch.rs`, which nothing had scrubbed.

### What is deliberately NOT pinned

Isolation must not quietly change which code path runs.
Three pins were tried and removed because they did:

- **`FNO_CONFIG`** - pinning it to one path overrides project-local discovery for the whole suite. The sandboxed `HOME` already relocates `~/.fno/config.toml`, and `FNO_CONFIG_SEARCH_ROOT` bounds the rest of the chain.
- **`FNO_GLOBAL_SETTINGS_PATH`** - the global candidate is `Path.home()/.fno/settings.yaml`, so the sandboxed `HOME` covers it. Re-pinning additionally overrode the candidate for tests that monkeypatch `HOME` precisely to exercise the global-fallback path.
- **`FNO_REPO_ROOT`** - pinning it points repo-root resolution at an empty sandbox, and a large part of the suite legitimately resolves the real checkout to find a lint script or the installed package. Unset is also exactly what CI has.

The toolchain caches (`CARGO_HOME`, `RUSTUP_HOME`, `UV_CACHE_DIR`, `XDG_CACHE_HOME`) are resolved from the **passwd entry** rather than `$HOME` and re-exported.
Reading them from `$HOME` would make them differ between the two lanes below, and the dirty lane would report that skew as a leak forever.

### The channel an env fixture cannot close

`_carveout_ledger_root` resolves from the caller's **CWD** via `git worktree list`.
No environment variable bounds that, so the carve-out channel is closed at the reader (`_hermetic_promise_carveout_gate` in `cli/tests/conftest.py`) for the pytest tree, and by running from a non-worktree directory for a shell test.
This is a real limit of the env-level cure, stated here rather than papered over.

## Move 2: one boundary, four trees

`cli/src/fno/test_cmd.py::_child_env` builds the environment for all four test trees:

- pytest over `cli/tests/**`
- pytest over `cli/src/fno/**/test_*.py` (a separate pytest root with its own conftest)
- every `bash tests/*.sh` step in the smoke registry, via `_smoke_env`
- `cargo nextest` / `cargo test`, per crate

One `neutralise` call there covers all four.
No shell mirror and no Rust mirror exist, so none can drift: a guard placed on one of N reachable paths is decorative.

Both conftests additionally call `neutralise(os.environ)` **at module load**, which covers a developer running a bare `pytest cli/tests/...`.
Module load rather than a fixture because `fno.graph` freezes its path constants at import time, so a fixture is already too late.

## Move 3: the dirty lane

Neutralising alone cannot detect a test that reads ambient state, because afterwards there is none to read.

`fno test smoke --ambient clean|dirty|both` (default `clean`).

`dirty` poisons the **runner's** environment, and `_child_env` then neutralises that poisoned parent to build each child env.

- If the surface is complete, `neutralise(poison(E)) == neutralise(E)`, the child is identical to the clean lane, and the dirty lane is green.
- If the surface missed a name, that name survives the copy into the child, a test that reads it sees an `fno-poison-*` sentinel, and it fails **naming itself**.

A green clean lane beside a red dirty lane on the same commit is the divergence; no diffing machinery is needed, and the failing test id is the attribution.
The invariant this rests on is asserted directly in `cli/tests/unit/test_hermetic.py`, not inferred from a green suite.

The poison profile (`cli/tests/fixtures/ambient-poison/`) is derived from the three measured specimens.
Values are sentinels rather than plausible data: a leak has to fail loudly, and a realistic value might quietly pass and teach nobody anything.

### The positive control is mandatory

A dirty lane that is green because it never poisons anything is an absence-only success condition: it cannot distinguish "no leaks" from "the instrument never ran".

`AMBIENT_LEAK_CANARY` carries no `FNO_` prefix and is not a session marker, so nothing in `hermetic.py` scrubs it.
That is the point: it simulates the channel the inventory has not thought of yet.

`cli/tests/unit/test_ambient_canary.py` reads it, and `tests/ci/test_hermetic_lanes.sh` asserts **both** halves:

```
clean lane -> canary unset -> the canary test PASSES
dirty lane -> canary leaks -> the canary test FAILS
```

If someone widens a scrub rule until it swallows the canary, the second half stops holding and the self-test goes red, so the silent disarming is caught.
A dirty-lane failure of the canary is never something to fix by pinning it.

## CI

`smoke` is the merge gate. A separate `smoke-dirty` CI job used to rerun the full suite under synthetic ambient state beside it, advisory. Its full history (122 runs, 2026-08-11 to 2026-08-19) caught zero real ambient leaks. It was deleted (x-b130) rather than kept as a second 1500s+ full-suite run for no observed signal. The hermeticity fixture and `cli/tests/unit/test_ambient_canary.py` keep running inside `smoke` itself. `fno test smoke --ambient dirty` (see above) still exists for local verification and `tests/ci/test_hermetic_lanes.sh`. If a future ambient leak surfaces some other way, narrow a lane to the tests that can observe it rather than reviving the full-suite rerun.

## The uncovered path

Hermeticity is applied by the **runner**.
`bash tests/whatever.sh` skips it, and that run reads your real `HOME`, config chain and carve-out ledger: a pass proves nothing about hermeticity, and a failure may be your machine.
`tests/README.md` and `fno test smoke --help` both say so, at the two places someone stands when they do it.
Closing it properly would mean a per-file header enforced by a lint across 131 shell harnesses; that has not been done.
