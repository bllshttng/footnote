# Detecting a stale installed `fno`

The `fno` on a developer's PATH is a snapshot, not a live repo view. `uv tool install` or `pip` installs the Python wheel. `cargo install --path crates/fno-agents` installs the Rust agent bins. Neither refreshes alone. `fno doctor update` reinstalls the Python wheel. Rust bins require a separate refresh. Verbs added after the last install remain invisible to the deployed CLI. Previously, nothing detected that skew.

The concrete failure involved a new capture group. The `deferrals_captured` gate depended on an event from that group. Older installed CLIs lacked the subcommand. The documented invocation then failed with `No such command`. No event landed, so the documented path was unable to satisfy the gate.

That group was reachable as both `fno backlog capture` and `fno backlog inbox` at the time.
The duplicate `inbox` spelling has since been removed, and the surviving spelling is `capture`.
The staleness failure is unchanged by that: it is about a deployed binary predating a verb, not about which name the verb has.

## `fno doctor` (detection)

`fno doctor` reports skew between the installed `fno` and a resolvable source
checkout. It is **network-free** and exits non-zero only when staleness is
*proven*. Two independent signals, each degrading to `unknown` rather than
crying wolf:

- **Revision compare (high-signal).** `fno doctor update` records source `git rev-parse HEAD` into `~/.fno/installed-rev` after a successful install. Source precedence is `--source`, `$FNO_SOURCE`, `~/.fno/source-path`, then well-known paths. `doctor` reads and compares source HEAD. A marker behind source means stale. An absent marker means "rev unknown" and falls back to the probe.
- **Capability probe (always available).** Runs `fno backlog capture --help`
  (the capture-tier verb, formerly named `backlog inbox`)
  against the installed CLI; a `No such command` failure proves the verb is
  missing regardless of any marker. The probe outcome is a three-valued
  `present | missing | unknown` so a "could not probe" result can never be
  conflated with "proven missing".

For the Rust side, `doctor` reports which `fno-agents` binary `auto` mode resolves (wheel-bundled or `~/.cargo/bin`). It proves Rust staleness through the `installed-rust-rev` marker (see below). `rust_stale` requires four known facts: a cargo binary, marker, crates-subtree revision, and mismatch. Anything less degrades to `unknown`, never false `fresh`. Proven Rust staleness sets the overall status to `stale` and exits 1. A binary-embedded commit cross-check remains planned for machines without a marker.

Plus an advisory **mux front-door** check: now that the Rust mux binary
(`crates/fno`) is meant to own `fno` on PATH, `doctor` reports whether it does -
`mux_front_door` is `active` (mux cargo-installed and `fno` on PATH resolves to
it), `shadowed` (installed but a Python `fno-py`, or nothing, wins PATH), or
`not-installed`. It never changes the status or exit code: a front-door setup
problem is distinct from source-vs-installed staleness.

Flags: `--json` emits one stdout object and sends human text to stderr. The object carries status, revision, binary, and mux-front-door fields. For Python staleness, `--fix` delegates to `fno doctor update`, whose Rust leg also refreshes the bins. For Rust-only staleness, it runs the cargo refresh helper without reinstalling Python. Under `--json`, `--fix` performs no repair and prints a skip message. `--source` overrides the source checkout.

```bash
fno doctor            # human verdict, exit non-zero iff proven stale
fno doctor --json     # machine verdict for an LLM caller
fno doctor --fix      # run `fno doctor update` if the Python install is stale
```

## The `installed-rev` marker

`fno doctor update` writes the marker only after a **successful** install. Failed or partial updates write no marker. A temporary file plus rename makes the write atomic, so concurrent `fno doctor` reads cannot see torn values. On Unix, `fno doctor update` uses `os.execvp` and never returns. Therefore, the shell chains `uv tool install --reinstall <src> && <atomic marker write>`. The `&&` requires a zero install exit. Windows keeps `subprocess.run` and writes after a zero return.

## Rust freshness: the binary self-reports

Both `fno doctor update`'s Rust-leg gate and `fno doctor`'s Rust verdict interrogate the installed binary itself, not a marker file. Every `fno-agents` build bakes in (via `crates/fno-agents/build.rs`, surfaced by `fno-agents version --json`):

- `crates_rev` - the last commit touching `crates/` at the built HEAD. This is the freshness signal, and it is true for ANY install path, including a bare `cargo install` outside `fno doctor update`.
- `git_rev` - the full HEAD the binary was built at (build provenance only).
- `dirty` - whether the working tree was dirty at build time.

If the installed `crates_rev` matches source and the build is clean, the **`fno doctor update` gate** skips `cargo install`. A missing binary, failed `version --json`, unknown revision, or dirty build triggers a rebuild. After rebuilding, update verifies the deployed binary. A remaining `crates_rev` mismatch halts the update. This catches stale artifacts and installs outside the runtime's resolved root. The old marker gate hid this lie: the marker was fresh while the binary was stale.

**Triad sync:** The three bins must share one build in every install location. They are `fno-agents`, `fno-agents-daemon`, and `fno-agents-worker`. The daemon resolves beside the client through `resolve_daemon_bin`. On rebuilt and fresh paths, update atomically copies the triad into every OTHER live location already hosting one member. It never seeds new locations and halts on unwritable ones. The fresh path lets an interrupted prior run converge later. A split pair surfaces as `DaemonBinMissing`, which names `fno doctor update` and `FNO_AGENTS_DAEMON_BIN` as fixes.

**`fno doctor` verdict:** `rust_source_rev` is the last commit touching `crates/`. The binary's `crates_rev` becomes `rust_installed_rev`. With a cargo binary and both revisions known, the verdict compares them. A mismatch sets `rust_stale: true`. `git_rev` is labeled build provenance and is never compared with source. Therefore, "rust bins fresh" cannot coexist with a mismatch. A Python-only commit after the last `crates/` change is not a mismatch.

**Legacy `~/.fno/installed-rust-rev` marker:** update still writes it as an
inert breadcrumb, but NO freshness verdict reads it anymore. Both consumers read
the binary's embedded `crates_rev` instead, which is correct for a bare
`cargo install` that the marker never tracked.

**`fno doctor update` Rust leg gating table:**

| Condition | Rust leg runs? |
|-----------|----------------|
| `--rust` flag present (force / first-install) | always (rebuild + post-deploy verify) |
| `--no-rust` flag present | never |
| auto: binary self-reports `crates_rev` == source AND not dirty | no (fresh; still syncs the triad to other live locations) |
| auto: binary stale / dirty / unparseable / absent | yes (rebuild + verify) |
| auto: rebuild needed but cargo not on PATH | warn and skip the Rust leg |

On cargo failure the Rust leg warns and continues to the Python reinstall
rather than aborting the entire update; a post-deploy verify mismatch or a
triad-sync failure, by contrast, HALTS update (a silently stale or split deploy
is the stale-deploy outage class this section exists to prevent).

`fno doctor update --rust / --no-rust` let you force or skip the Rust leg explicitly.

## Layer 2: the deferrals gate self-explains

When the `deferrals_captured` gate is unsatisfied in strict mode, the audit probes whether the installed `fno` exposes `backlog capture` (`capture_verb_available` in `scripts/lib/gates-reality.sh`). If the verb is missing, the message says the installed CLI predates it. It instructs the operator to run `fno doctor update` or `fno doctor --fix`, then retry. `translate_capture_unknown_command` fires only on a non-zero exit carrying Typer's unknown-command signature for `capture` or its legacy `inbox` spelling. A real `empty-pass` runtime error keeps its existing handling.

The gate path **instructs only**. It never invokes `fno doctor update`. Auto-fixing mid-gate can re-exec the CLI during a stop-hook check and risk a reinstall loop. Therefore, `--fix` is the only path that runs `fno doctor update`. It preserves update's refusal during an `IN_PROGRESS` target session.

## Surfacing staleness inside a running mux

`fno doctor` answers whether the installed CLI is stale. It never reaches an operator sitting inside a mux session rather than a terminal running `doctor` in another pane. `fno doctor update --check --json` is the **single resolver** for that surface. It answers a related but distinct question: not just whether an update is waiting, but what running it costs right now. The mux TUI (`crates/fno/src/client.rs`) renders that answer. The TUI computes nothing itself: no staleness logic, no wire comparison, no shell counting lives in Rust. It runs the CLI probe off its UI loop, an async subprocess mirroring the existing Connections-modal read. It parses the JSON and folds it into a sideline-menu row and an overlay.

The one input `doctor` lacks is whether an update breaks the live mux **wire protocol**. A server whose wire predates a new client's `PROTO_VERSION` rejects the handshake outright. That server is unreachable and must restart, ending every shell it holds. `update_readiness` (in `cli/src/fno/update.py`) computes this by comparing the source checkout's `crates/fno/src/proto.rs::PROTO_VERSION` against each **live** server's `wire_version`. That field comes from `fno mux ls --json` and matches `SessionRow::wire_stale` in `crates/fno/src/mux_cli.rs`, which already detects a stale-wire server. A wire bump names the shells the restart ends. It also names the workers `--revive` brings back, via the shared `is_revivable` predicate in `cli/src/fno/restart.py`, called from both `update_readiness` and `_revive_orphans`. No bump means the shells survive a plain `fno doctor update` and reattach.

Guidance names `fno restart --mux` for a routine wire-version bump, and `fno mux kill-server` for a wedged server a plain restart cannot reach. `kill-server` no longer depends only on the control channel it exists to recover from. It escalates through SIGTERM then SIGKILL, using a pid sidecar the server writes at bind. That makes it work in the wedged or wire-rejecting case an operator reaches for it. Every operator surface (`mux ls`, the attach error, `restart.py`) names this escalation, not a manual kill.

Every input degrades independently rather than failing the whole probe. A missing `fno mux ls`, an unreadable `proto.rs`, or a failing `fno agents list` each name themselves in a `degraded` field. An unknown wire status is always treated as a bump, so the guidance line never claims shells survive on evidence the resolver lacks. `fno doctor update --check` never installs anything and always exits 0. Readiness is data, not a verdict.

```bash
fno doctor update --check --json   # the one resolver, the mux TUI's only consumer
```

## Locked decisions

1. `fno doctor` is the primary mechanism, not reinstall-on-ship. Detection plus
   explicit repair beats implicit mutation that races a running pipeline.
2. Detection is network-free: local `git rev-parse` + local command trees, no
   PyPI / crates.io calls. No source checkout yields `unknown`, never a false
   `stale`.
3. The gate path instructs, it never executes the fix.
4. When all four facts are present, the `installed-rust-rev` marker proves Rust staleness. Anything less degrades to `unknown`. `doctor` still reports the resolved binary. A binary-embedded commit cross-check remains planned for environments without the marker.
5. The `installed-rev` marker is written only after a successful install. Absence means "rev unknown", never "fresh".
6. `fno doctor update --check --json` is the single resolver for mux update readiness. The TUI renders it and computes nothing. Guidance routes restart recovery through `fno restart --mux` only, never `fno mux kill-server` (that routing fix is tracked separately). A verb guaranteed to fail in the case it is offered for is worse than no guidance.

Implementation: `cli/src/fno/doctor.py`, `cli/src/fno/update.py`, `cli/src/fno/restart.py`, `crates/fno/src/client.rs`, `crates/fno/src/mux_cli.rs`, `scripts/lib/gates-reality.sh`, `scripts/lib/gate-audit.sh`.
