# Detecting a stale installed `fno`

The `fno` on a developer's PATH is a snapshot, not a live view of the repo.
It is installed by `uv tool install` / `pip` (the Python wheel) and, for the
Rust agent bins, by `cargo install --path crates/fno-agents`. Neither refreshes
on its own: `fno update` reinstalls the Python wheel, and nothing auto-refreshes
the Rust bins. So any verb added to the repo after the last install is invisible
to the deployed CLI, and there was previously no way to detect the skew.

The concrete failure this closes: a change added the `fno backlog inbox` command
group and made the `deferrals_captured` gate depend on an `inbox_add` /
`inbox_empty_pass` event. An `fno` installed before that change has no `inbox`
subcommand, so `fno backlog inbox empty-pass` fails with `No such command 'inbox'`,
the event never lands, and the gate is unsatisfiable through the documented path.

## `fno doctor` (detection)

`fno doctor` reports skew between the installed `fno` and a resolvable source
checkout. It is **network-free** and exits non-zero only when staleness is
*proven*. Two independent signals, each degrading to `unknown` rather than
crying wolf:

- **Revision compare (high-signal).** `fno update` records the source's
  `git rev-parse HEAD` into `~/.fno/installed-rev` on a successful install.
  `doctor` resolves the source via the existing precedence (`--source` >
  `$FNO_SOURCE` > `~/.fno/source-path` cache > well-known paths),
  reads the source's HEAD, and compares. Marker behind source ⇒ stale; marker
  absent ⇒ "rev unknown" (fall back to the probe, never a false `fresh`).
- **Capability probe (always available).** Runs `fno backlog capture --help`
  (the capture-tier verb, formerly named `backlog inbox`)
  against the installed CLI; a `No such command` failure proves the verb is
  missing regardless of any marker. The probe outcome is a three-valued
  `present | missing | unknown` so a "could not probe" result can never be
  conflated with "proven missing".

For the Rust side, `doctor` reports *which* `fno-agents` binary `auto` mode
resolves (wheel-bundled vs `~/.cargo/bin`), and now proves Rust staleness via
the `installed-rust-rev` marker (see below). `rust_stale` is true only when all
four facts are simultaneously known: a cargo-installed binary exists, the marker
exists, the crates-subtree rev is computable, and the marker differs from the
subtree rev. Anything less degrades to `unknown`, never a false `fresh`. Proven
Rust staleness folds into the overall status (`stale`) and the exit code (1).
A binary-embedded commit cross-check remains separately planned for
machines where the marker is absent.

Plus an advisory **mux front-door** check (x-c267): now that the Rust mux binary
(`crates/fno`) is meant to own `fno` on PATH, `doctor` reports whether it does -
`mux_front_door` is `active` (mux cargo-installed and `fno` on PATH resolves to
it), `shadowed` (installed but a Python `fno-py`, or nothing, wins PATH), or
`not-installed`. It never changes the status or exit code: a front-door setup
problem is distinct from source-vs-installed staleness.

Flags: `--json` (single stdout object `{status, python_stale, rust_stale,
missing_verbs, source_rev, installed_rev, rust_binary, rust_installed_rev,
rust_source_rev, mux_binary, path_fno, mux_front_door}`, human text to stderr),
`--fix` (python-stale: delegates to `fno update`, whose Rust leg also refreshes
the bins; rust-only-stale: runs the cargo refresh helper directly without a
Python reinstall; under `--json`, `--fix` is repair-free and prints a skip
message for both legs), `--source` (override the source checkout).

```bash
fno doctor            # human verdict, exit non-zero iff proven stale
fno doctor --json     # machine verdict for an LLM caller
fno doctor --fix      # run `fno update` if the Python install is stale
```

## The `installed-rev` marker

Written by `fno update` only on a **successful** install (no marker on a
failed or partial update) and **atomically** (temp file + rename), so a
concurrent `fno doctor` read never sees a torn value. Because `fno update`
uses `os.execvp` on Unix (the process is replaced and never returns), the
marker write is chained onto the installer through the shell:
`uv tool install --reinstall <src> && <atomic marker write>`. The `&&` gates
the write on a zero install exit. Windows keeps the `subprocess.run` path and
writes the marker after a zero return.

## The `installed-rust-rev` marker

A parallel marker at `~/.fno/installed-rust-rev`, written by `fno update`
(or the standalone cargo refresh helper) **only on a successful `cargo install`
exit** and atomically via the same temp-file-rename pattern. It stores the
output of `git log -1 --format=%H -- crates/` - the hash of the last commit
that touched the `crates/` subtree - rather than `HEAD`. This means Python-only
commits never advance the marker and never trigger a redundant Rust rebuild in
auto mode.

`fno doctor` computes `rust_source_rev` the same way (last commit touching
`crates/`), reads `installed-rust-rev` as `rust_installed_rev`, and sets
`rust_stale: true` only when all four facts are known and the values differ.
If the marker is absent, binary is absent, or the subtree rev is
uncomputable (e.g. no git checkout), the field degrades to `unknown`.

**`fno update` Rust leg gating table:**

| Condition | Rust leg runs? |
|-----------|----------------|
| `--rust` flag present (force / first-install) | always |
| `--no-rust` flag present | never |
| auto (neither flag): cargo binary exists AND crates subtree moved past marker | yes |
| auto: no cargo binary on this machine | no (never installs on a first-time machine without `--rust`) |
| auto: marker absent (binary present, marker missing) | no (treats as unknown, not stale) |

On cargo failure the Rust leg warns and continues to the Python reinstall
rather than aborting the entire update.

**Manual `cargo install` caveat:** the `installed-rust-rev` marker tracks only
what `fno update` (or the cargo refresh helper) installed. A manual
`cargo install --path crates/fno-agents --bins` run outside of those paths
updates the binary but does not write the marker, so `fno doctor` may report
`rust_stale: true` until the next `fno update` run re-syncs the marker.

`fno update --rust / --no-rust` let you force or skip the Rust leg explicitly.

## Layer 2: the deferrals gate self-explains

When the `deferrals_captured` gate is unsatisfied in strict mode, the audit
message probes whether the installed `fno` exposes `backlog capture`
(`capture_verb_available` in `scripts/lib/gates-reality.sh`). If the verb is
missing, the message becomes "installed fno predates the 'backlog capture'
verb; run `fno update` (or `fno doctor --fix`) then retry" instead of the
opaque Typer error. `translate_capture_unknown_command` is the pure
translator: it fires only
on a non-zero exit carrying Typer's unknown-command signature for `capture`
(or the legacy `inbox` spelling), so a
real `empty-pass` runtime error falls through to existing handling unchanged.

The gate path **instructs only; it never invokes `fno update`**. Auto-fixing
mid-gate would re-exec the CLI during a stop-hook check and risk a
reinstall/recheck loop, so `--fix` is the only path that runs `fno update`,
and it keeps `update`'s existing refusal to reinstall during an `IN_PROGRESS`
target session.

## Locked decisions

1. `fno doctor` is the primary mechanism, not reinstall-on-ship. Detection plus
   explicit repair beats implicit mutation that races a running pipeline.
2. Detection is network-free: local `git rev-parse` + local command trees, no
   PyPI / crates.io calls. No source checkout yields `unknown`, never a false
   `stale`.
3. The gate path instructs, it never executes the fix.
4. Rust staleness is now provable via the `installed-rust-rev` marker when all
   four facts are present; anything less degrades to `unknown`. `doctor` still
   reports which binary is resolved. A binary-embedded commit cross-check is
   separately planned for environments where the marker is absent.
5. The `installed-rev` marker is written only on a successful install; absence
   means "rev unknown", never "fresh".

Implementation: `cli/src/fno/doctor.py`, `cli/src/fno/update.py`,
`scripts/lib/gates-reality.sh`, `scripts/lib/gate-audit.sh`.
