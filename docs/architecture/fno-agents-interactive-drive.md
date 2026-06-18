# fno agents interactive-drive agent type (host_mode)

`fno agents` could previously host only **one-shot exec** runs of codex and gemini in a worker PTY (`codex exec --json ...` / `gemini -p ... --output-format json`): both run the message and exit, so the grid (`fno agents grid`) and `fno agents drive` could *watch* them mid-exec but never *drive* them, because there was no live interactive prompt to send keystrokes to. `claude` was the only drivable agent.

This feature adds the ability to host the **interactive** TUI of codex and gemini inside an fno-managed worker PTY, so the existing drive/grid surface can drive them with real keystrokes. The headline flow: an earlier `ask`/`spawn` produces a session UUID; later you *promote* that session into a live interactive worker and drive it. A fresh interactive spawn (no prior session) is the same substrate with different argv.

The key realization (verified against the live CLIs): this is **not** a grid change and **not** a new provider. It is a hosting-mode change. The provider stays `codex`/`gemini`; a new persisted `host_mode: exec | interactive` field tells the spawn path which argv to build and tells reconcile that this worker is a long-lived drivable session rather than a one-shot expected to exit.

## `host_mode` registry field

`host_mode` is added to the registry row in both languages, following the cross-language coupling discipline (see [cross-language-schema-parity.md](cross-language-schema-parity.md)):

- **Rust** (`RegistryEntry`, `crates/fno-agents/src/state.rs`): `host_mode: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. `None` == `exec`. Consumers read it via `host_mode_or_default()` / `is_interactive()`, never the raw `Option`, so the absent==exec rule lives in one place. Constants `HOST_MODE_EXEC` / `HOST_MODE_INTERACTIVE`.
- **Python** (`AgentEntry`, `cli/src/fno/agents/registry.py`): `host_mode: Optional[str]`. `load_registry` coerces a missing key OR an explicit null to `"exec"` so every consumer sees a concrete mode, and rejects an alien non-null value (`KNOWN_HOST_MODES`) like an alien status.

**Round-trip:** a Rust exec row omits the key (skip-when-None) and Python's coercion maps the absence back to `exec`; Python always emits the key via `asdict` (as `exec`/`interactive`) and Rust reads the concrete value. Both directions agree. No `SCHEMA_VERSION` bump was needed: the field is additive-optional and absent-key handling is version-independent on both sides (a bump would also break the `schema_version: 4`-is-unknown contract test). Note the residual mixed-registry gap from [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md) still applies: a real interactive PTY row carries a non-empty `short_id` Python rejects, so the Python CLI reaches interactive rows through the auto-routed Rust daemon, not `load_registry`.

## Verbs

Both are thin front-doors that call the spawn IPC (`agent.spawn`) with `host_mode=interactive`. Added to the hand-rolled dispatch in `crates/fno-agents/src/bin/client.rs` (`build_request`) and to `AUTO_ROUTE_VERBS` in `cli/src/fno/agents/rust_runtime.py`. The custom agents group intercepts before Typer, so these daemon-native verbs route to the Rust binary without a Python Typer command (like `spawn`/`drive`).

| Verb | Form | Meaning |
|---|---|---|
| `host` | `fno agents host <name> --provider codex\|gemini ["<task>"]` | Fresh interactive host. Empty task = bare interactive session. |
| `promote` | `fno agents promote <name> --from <uuid>` | Resume an existing session into a drivable TUI. The daemon **infers the provider from the source row**, so `--provider` is not required. `--from` accepts the `=`equals form. |

### Data flow (promote, the headline path)

```
fno agents ask|spawn bot --provider codex "task"   # exec; codex_session_id captured -> registry
   ... runs codex exec, exits, codex_session_id=UUID persisted ...
fno agents promote bot2 --from <UUID>
   -> daemon handle_spawn(host_mode=interactive, resume_id=UUID)
   -> admit_promote: infer provider=codex, enforce invariants
   -> resume_interactive_argv -> codex resume <UUID> --include-non-interactive
   -> worker forks PTY, execs interactive codex, owns PTY master (Outcome B)
   -> registry row: provider=codex, host_mode=interactive, codex_session_id=UUID, status=live
fno agents grid bot2     (or  fno agents drive bot2 --mode interactive)
   -> drive admits (PTY-managed, drive-eligible), raw PTY <-> alacritty + keystrokes
```

The drive/grid layer needs **zero changes** for MVP: it already renders raw PTY and forwards keystrokes in `mode=interactive`. The only missing piece was a worker hosting an interactive process. (Grid keying off `host_mode` for Enter-to-promote UX is a deferred follow-up.)

## Interactive argv (empirically verified)

Selected by `host_mode` via the `Provider` trait methods `create_interactive_argv` / `resume_interactive_argv`, which default to `None` (claude has no fno-hosted interactive form, so `host`/`promote` reject it in the type system). Verified against codex-cli 0.133.0 and gemini 0.42.0 (AC3-FR):

| Provider | Fresh (`host`) | Promote (`--from <uuid>`) |
|---|---|---|
| codex | `codex -C <cwd> {--sandbox workspace-write \| --dangerously-bypass-approvals-and-sandbox} ["<task>"]` | `codex resume <uuid> --include-non-interactive [bypass] ["<task>"]` |
| gemini | `gemini --skip-trust [-i "<task>"] {--approval-mode default \| --yolo}` | `gemini --skip-trust -r <uuid> [-i "<task>"] {approval}` |

- Interactive uses **no `--json`** (that is the exec/parse path); driving renders raw PTY. `codex` interactive is the bare subcommand-less form, **not** `codex exec`, and needs no `--skip-git-repo-check` (exec-only).
- **`codex resume <uuid>` resumes an exec-born session with full history** (verified: the model recalled the prior turn). `--include-non-interactive` only governs the resume *picker* / `--last`; an explicit positional UUID bypasses the picker, so the flag is redundant-but-harmless and is included per the AC3-FR fallback.
- **`gemini -r <full-uuid>` resumes a `-p`-created session** (verified: same `session_id` returned). The `-r` help documents only `latest`/index, but a full UUID resolves.
- yolo maps to codex `--dangerously-bypass-approvals-and-sandbox` / gemini `--yolo`; the non-yolo default leaves the provider's own approval UI active (a human drives).

## Interactive readiness gate

Exec mode confirms readiness later from the `--json` stream; an interactive TUI has none, so the daemon's spawn path settles on **"the PTY painted its first frame AND the child is still alive after a minimum dwell"** (`await_interactive_readiness` / pure `interactive_readiness_step` in `daemon.rs`):

- child died within the settle window -> **spawn-failed**, with the worker's last painted bytes for diagnosis (a `codex resume`/`gemini -r` that fails at launch — auth expired, unknown session — is the AC1-FR case).
- painted AND alive AND survived the dwell (1s) -> **ready/live**. Survival, not first paint, is the discriminator, so a resume that prints an error *then* exits cannot pass on its dying first paint.
- alive but unpainted at the 4s deadline -> **live** (a slow first paint / warming model; never reap a live process).

The verb therefore reports either a `live` summary (name, short_id, provider, drive hint, exit 0) or a `spawn-failed` (nonzero exit) — never exit 0 on a worker that died (AC1-UI).

## Reconcile / liveness branch

`plan_reconcile` branches on `host_mode`. An interactive host's liveness is its **PTY process**, not exec-session-store membership (a live `codex resume` TUI may not appear in the exec session index), so a store-miss must **not** orphan a healthy interactive worker (US4). A genuinely dead interactive worker is reaped to `Exited` by the pid-liveness sweep in `recover()` — "an unexpected exit is exited/failed, not orphaned" — and `host_mode` round-trips through daemon recovery (AC2-FR).

## Concurrency invariant: one host per session

Promoting the same session UUID into two interactive workers would let two `codex resume <uuid>` processes write the same session (corruption). `admit_promote` enforces, against a registry snapshot:

- empty/blank `--from` -> reject.
- a non-terminal interactive host already on this session -> reject (one interactive host per session; covers a re-promote and a still-live interactive source).
- no recorded agent holds the session id -> reject (unknown session).
- a source in a mid-flight transient state (`Spawning`/`Restarting`/`Busy`) -> reject (still running).
- a source row with no provider recorded -> reject (cannot infer host).

The one-host check is **re-run atomically under the registry update lock** alongside the name reservation, so two concurrent `promote --from <same-uuid>` calls produce exactly one host plus one rejection.

> **Deviation (deliberate):** the plan's literal rule "only an *exited* source is promotable" cannot be enforced as `status == exited`, because codex/gemini `ask` leaves the source row `Live` after the one-shot exits (`codex_ask.rs` stamps `status=live` on success). Enforcing the literal rule would make every exec-born session un-promotable, contradicting AC1-HP. `admit_promote` therefore rejects only genuinely-in-flight states and treats a settled `Live` exec source as promotable. Whether `ask` should instead mark the row `Exited` after a one-shot completes is tracked as a deferred decision.

## Out of scope / follow-ups

- Grid Enter-to-promote keyed on `host_mode` (UX; the substrate drives any PTY agent via `drive --mode interactive` today).
- Best-effort scrape of the newest `~/.codex/sessions` / gemini session record to populate `*_session_id` for a *fresh* `host` (promote already knows the UUID; fresh leaves it null).
- Typed `HostMode` enum / `ReadinessProbe` struct / relocating the interactive argv methods to `ProviderWithPty` (type-design polish).
- codex's experimental `app-server` + `--remote` attach model as a cleaner multi-client substrate than per-agent PTY.
