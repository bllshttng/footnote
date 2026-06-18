# fno CLI short-flag convention

Typing long flags into `fno` from a phone (the Happy app and other runner-less surfaces) is painful, and one case is acute: `fno agents ask --provider claude` requires `--provider`, and on iOS the double-hyphen autocorrects to an em-dash. This document defines the short-flag scheme that fixes that, and the test that enforces it.

Phase 1 establishes the convention and the enforcement; the rollout across the rest of the command surface is phased (see [Phasing](#phasing)).

## The convention

Two rules, chosen for phone ergonomics:

- **UPPERCASE letters are a small fixed GLOBAL register.** Each means the same thing on every command and is never reused for a per-command meaning. The cross-cutting booleans get one stable letter everywhere.
- **lowercase letters are per-command value flags.** They may differ per command (no shift key needed), so `-p` legally means provider on `agents`, priority on `backlog` mutate, project on `backlog` query. These never co-occur on one command, so there is no collision.

Entrenched unix lowercase is kept where it already exists (`-h` help, `-q` quiet, `-f`/`-n` already on `logs`); the uppercase register is reserved for footnote-specific cross-cutting flags. Click is case-sensitive, so `-n` (`--tail`) and `-N` (`--dry-run`), or `-f` (`--follow`) and `-F` (`--force`), coexist on the same command without conflict.

## The global UPPERCASE register

Six letters, reserved across the entire CLI:

| Short | Long | Type | On (examples) |
|-------|------|------|---------------|
| `-J` | `--json` | bool | the runaway most-common flag (~40 commands) |
| `-A` | `--all` | bool | backlog next/ready/status/pick/queued, triage, megawalk |
| `-F` | `--force` | bool | backlog decompose/remove, providers add/remove, agents rm, update |
| `-N` | `--dry-run` | bool | backlog intake/reconcile, runtime reap, worktree cleanup, update |
| `-R` | `--reason` | value | backlog defer/queue/supersede, claim acquire/force-release, inbox passes |
| `-Y` | `--yolo` | bool | danger-mode bypass (agents ask `--yolo`) |

Phase 1 applies these six to every Typer command that already declares the matching long flag. The addition is purely a second string in the existing declaration (`typer.Option(False, "--json", "-J", ...)`); long-flag behavior is byte-identical.

## The phone-critical command: `agents ask`

`fno agents ask` gets three lowercase shorts in Phase 1, because it is the command the convention exists for:

| Short | Long | Meaning |
|-------|------|---------|
| `-p` | `--provider` | claude / codex / gemini |
| `-c` | `--cwd` | working directory for the agent subprocess |
| `-t` | `--timeout` | per-ask timeout in seconds |

So `fno agents ask a "hi" -p claude -c /repo -t 30` works end to end.

## The per-command lowercase map (Phase 2)

Phase 2 rolls the lowercase map across the everyday commands. Purely additive, same mechanics as Phase 1: a short string slots into the existing `typer.Option`; formerly-bare declarations gain an explicit long spelling identical to the Typer-derived name.

| Command | Lowercase shorts | Plus global |
|---------|------------------|-------------|
| `backlog add` | `-p` priority, `-c` cwd, `-d` details, `-t` type | - |
| `backlog idea` | `-d` description, `-p` priority, `-c` cwd | - |
| `backlog intake` | `-t` title, `-p` priority | `-N` |
| `backlog update` | `-p` priority, `-c` cwd, `-t` title | - |
| `backlog next` / `ready` | `-p` project | `-A`, `-I` |
| `backlog find` | `-p` project, `-s` status, `-d` domain | `-J` |
| `backlog capture add` | `-s` source, `-w` where, `-p` priority | - |
| `agents send` (was `inbox send`) | `-k` kind, `-b` body (`--to-project` is long-only) | `-J` |
| `providers add` | `-c` cli, `-a` auth, `-s` scope, `-p` priority | `-F` |
| `gate verify` | `-p` phase, `-s` state, `-x` strict | - |
| `gate check` | `-s` state | - |
| `event emit` | `-t` type, `-d` data, `-s` source | - |
| `done` | `-p` pr-number, `-l` link, `-m` note (pre-existing) | - |
| `carveout add` | `-k` kind, `-p` priority | - |

Path/default-resolved flags (`--state` on `event emit`, `--body-file`, `--credentials-source`, `--roadmap-id`, ...) intentionally stay long. `backlog pick`/`queued`/`status` are outside the Phase 2 group and gained nothing. The per-command rule does real work here: `-b` means `--body` on `agents send` (the verb the Phase 2 `-k`/`-b` shorts moved onto when `inbox send` was removed; `-t --to` became `--to-project`, which is long-only) while `backlog pick` keeps `-b --blocked`, and `-p` carries six different per-command meanings - legal because those commands never co-occur.

`agents list`/`resume` lowercase shorts on the Python path remain unassigned to any phase; on the Rust runtime the verb-agnostic parser already accepts them (see below).

### Cross-language parity (the load-bearing detail)

`agents ask`/`resume`/`list` auto-route to the compiled Rust client (`crates/fno-agents/src/bin/client.rs`) when an installed `fno-agents` binary is present; the Python `typer.Option` path is the fallback under `FNO_AGENTS_RUNTIME=python` or when no binary is installed. A short flag added only to the Python declaration would silently no-op on the exact command (phone `agents ask`) that motivated the feature.

So the same aliases live in both parsers. The Rust `build_request` hand-parser gains `-p`/`-c`/`-t` plus the globals it already recognizes (`-J`/`-A`/`-F`/`-Y`). Short value flags take a space-separated value (`-p claude`), matching Click's short-option convention; the `-p=value` form is intentionally not normalized on the Rust path.

Because the Rust parser is verb-agnostic, its aliases apply to every verb that uses those long flags, so `agents list -c /repo` works on the Rust runtime even though the Python `agents list` has no `-c`. This is additive and benign. Phase 2 deliberately did not touch the `agents` family (its delivery group covers the Python-only command surface, where no Rust parity question exists); reconciling Python `agents list`/`resume` is tracked as a separate carveout.

## Two-spelling canonicalization (Phase 3)

The flag enumeration found spelling drift that blocked unambiguous short assignment: `--session` vs `--session-id` and `--pr` vs `--pr-number`, each pair meaning the same thing on different commands. Phase 3 makes `--session-id` and `--pr-number` canonical everywhere and demotes the old spellings to **hidden deprecated aliases**.

The mechanics live in `cli/src/fno/_flag_aliases.py`. Typer/Click cannot hide one name of a multi-name option, so each alias is a SEPARATE `typer.Option(..., hidden=True)` parameter folded into the canonical value at the top of the command body by `merge_deprecated_alias`: legacy-only use works but warns on stderr (never stdout, so JSON consumers are safe); passing both spellings is refused as a usage error (exit 2) even when the values agree. Two formerly-required options (`backlog cost --session-id`, `reality-check gh --pr-number`) became `Optional` + an explicit missing-option check, because the hidden alias forces a `None` default.

Touched sites: `loop`, `review`, `gate check`, `backlog cost`, `retro run` (both concepts; its `--session-id` is repeatable), `worker review`, `worker external`, `reality-check gh`. `done` already accepted both spellings on one option and only needed a canonical-first reorder (`--pr-number, --pr, -p`). Internal callers were migrated too - notably the exit-42 dispatch payload's machine-generated `resume_command` (`handoff/dispatch.py`), which would otherwise have made the loop trigger its own deprecation warning on every reasoning-phase continuation.

Two subtleties worth knowing:

- **`megatron reconcile --pr` is exempt.** Its `--pr` is a 1-indexed *candidate-position* selector for ambiguous backfills, not a PR number; renaming it `--pr-number` would be actively misleading. The exemption is encoded in `TWO_SPELLING_EXEMPTIONS` in the convention test.
- **Direct (non-Click) calls.** Tests and internal callers sometimes invoke a Typer command function directly, leaving unfilled params holding their `OptionInfo` declaration defaults instead of `None`. `merge_deprecated_alias` coerces `OptionInfo` to "not passed" so direct calls keep pre-alias semantics.

### Discoverability: `fno help shorthands`

`-p` is deliberately the most overloaded letter, which makes a legend mandatory. `fno help shorthands` is a help topic (special-cased in `help_command` before the subprocess forward) printing the global register, every per-command `-p` meaning, the unix-entrenched exceptions, and the canonical spellings. Bare `fno help` appends a one-line pointer at it.

## Enforcement: the convention test is the source of truth

`cli/tests/test_short_flag_convention.py` is a static AST scan over every `typer.Option` in `footnote` source. It needs no runtime import of the lazily loaded command tree and cannot be fooled by deferred sub-apps. It enforces:

- **Global register, positive:** every command declaring a global long carries its reserved short.
- **Global register, negative:** the six letters never map to a non-global long.
- **No collisions:** no command declares the same short for two different options.
- **Pins (per-command semantics since Phase 2):** each of the 7 pre-convention (short, long) pairs (`-A -I -b -n -f -m -o`) still exists at its home declaration site. Lowercase letters are per-command, so a pre-existing letter may legally carry a different meaning on a new command (`-b --body` on `agents send` vs `-b --blocked` on `backlog pick`); the legacy uppercase `-I` stays codebase-exclusive like the global register.
- **Phone shorts:** `agents ask` carries `-p`/`-c`/`-t` on the Python path.
- **Phase 2 map:** every command in the table above declares its design-table shorts (`PHASE2_LOWERCASE_MAP`).
- **Phase 3 spellings:** every drift site declares its canonical long visibly and its legacy alias `hidden=True` (`TWO_SPELLING_SITES`); `--session`/`--pr` never appear as a VISIBLE primary long anywhere outside `TWO_SPELLING_EXEMPTIONS`; `done` lists `--pr-number` before `--pr`.

The AST scan proves declarations but structurally cannot catch a Click registration failure: every touched sub-app is lazily loaded, so a malformed declaration only raises at first dispatch. `cli/tests/test_short_flag_dispatch.py` closes that hole at runtime - a `--help` registration smoke per Phase 2 surface through the real root app, plus short-vs-long parity proofs for `backlog find` (read-only graph) and `providers add` (previously had no CLI test at all). Phase 3 adds `cli/tests/test_two_spelling_dispatch.py` (alias-vs-canonical equivalence on `reality-check gh`, stderr-warning and both-passed-refusal proofs, help-hiding probes) and `cli/tests/test_help_shorthands.py` (legend content + bare-help pointer).

The Rust path is pinned separately: `ask_accepts_phone_short_flags` (in `client.rs`) asserts the short form builds the byte-identical request the long form does, and `test_codex_ask_short_flags_match_long_through_rust_binary` (in `cli/tests/agents/test_ask_e2e_dispatch.py`) drives the compiled binary with `-p`/`-c`/`-t` and asserts identical stdout and exit to the long form.

Any future `typer.Option` that declares a global long without its short, or reuses a reserved letter, reddens the convention test. The convention self-enforces.

## Phasing

| Phase | Status | Scope |
|-------|--------|-------|
| 1 | shipped | The global uppercase register + the collision/invariant test + `-p`/`-c`/`-t` on `agents ask` (Python and Rust). |
| 2 | shipped | The lowercase per-command map across the backlog family, `agents send` (the `-k`/`-b` shorts originally landed on `inbox send`, which has since been removed), `providers add`, `gate verify`/`check`, `event emit`, `done`, `carveout add` (table above), + runtime dispatch tests. |
| 3 | shipped | Canonicalize `--session-id`/`--pr-number` (old spellings become hidden aliases via `_flag_aliases.merge_deprecated_alias`) and ship the `fno help shorthands` legend documenting the global register and each command's `-p` meaning. |
