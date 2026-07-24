# fno agents — codex provider

`fno agents ask --harness codex` spawns and follows up with OpenAI's `codex` CLI under fno's name registry, per-agent flock, and events.jsonl substrate — the same coordination primitives the claude path uses. Both create and follow-up are supported.

Parent: [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md). Sibling: [fno-agents-list-logs.md](fno-agents-list-logs.md).

## Surface

```bash
# Create a codex agent pinned to the current cwd:
fno agents ask worker-mig --harness codex --cwd /Users/foo/proj 'write the schema migration'

# Follow up on the same session (cwd is ignored; codex sessions are cwd-pinned):
fno agents ask worker-mig 'switch to drizzle, not prisma'

# Dangerous mode: --dangerously-bypass-approvals-and-sandbox in place of --sandbox workspace-write:
fno agents ask worker-bootstrap --harness codex --yolo 'scaffold a Next.js app with auth'

# LLM orchestrator dispatch with a from-name advertised to the worker:
fno agents ask codex-helper --harness codex --from-name orchestrator-main 'review the migration PR'
```

The `--yolo` flag is provider-agnostic in spec but a no-op for claude (`claude --bg` has no equivalent; it emits a one-line stderr note and continues).

## Module layout

```
cli/src/fno/agents/
├── cli.py                       ← --yolo flag wiring
├── dispatch.py                  ← codex create + resume routes
└── providers/
    └── codex.py                 ← create() + resume() + argv helpers
cli/scripts/smoke/
└── capture-codex-jsonl.sh       ← smoke capture for event-vocabulary pinning
cli/tests/agents/
├── fixtures/codex-jsonl-sample.jsonl   ← committed real-codex capture
├── fixtures/fake-codex-hang.sh         ← signal-handling test shim
├── test_providers_codex_argv.py        ← argv + AST-regression tests
├── test_providers_codex_create.py      ← create unit tests
├── test_providers_codex_resume.py      ← resume unit tests
├── test_dispatch_codex.py              ← dispatch-layer tests
├── test_codex_signal_handling.py       ← real-subprocess SIGINT/timeout tests
└── test_codex_integration_smoke.py     ← CODEX_SMOKE-gated live tests
```

## Event vocabulary pinning

The parser does NOT guess codex's JSONL event-type strings. A smoke run of `scripts/smoke/capture-codex-jsonl.sh` against the live `codex` CLI (0.130.0 at design time) saves the captured stream to a committed fixture, and the distinct event types are pinned into a module-level constants dict:

```python
_EVENT_TYPES = {
    "session":       "thread.started",
    "complete":      "turn.completed",
    "item_envelope": "item.completed",
}

_ITEM_TYPES = {
    "message": "agent_message",   # has .item.text
    "error":   "error",           # has .item.message; SOFT, not always fatal
}
```

Parser code references these by key only. An AST regression test (`test_no_event_type_literal_strings_outside_constants_block`) walks the codex.py source and rejects any sentinel string literal outside the constants dict assignments (excluding docstrings). Drift in codex's vocabulary surfaces as a `NoSessionIdError` carrying the actual `types_seen` set (warn-on-drift forensics), NOT as a silent failure with `session_id=None`.

## Subprocess lifecycle

`stderr=subprocess.STDOUT` merges codex's stderr into the same pipe the parser drains. Without the merge, a large stderr write fills the kernel pipe buffer, blocks the child on its next stderr write, and the JSONL stream silently stalls.

`start_new_session=True` places codex in its own process group so timeout-driven SIGTERM / SIGKILL propagates to its sandbox subshells. A flat `proc.terminate()` killed only the wrapper bash and left `sleep` orphans; the read loop then blocked on a never-EOF'd pipe. The watchdog timer sends SIGTERM via `os.killpg(os.getpgid(proc.pid), SIGTERM)`, schedules a 2-second follow-up SIGKILL timer, and `_wait_with_grace()` returns `(exit_code, sigkill_escalated)` so the caller can distinguish a clean wait from a force-kill.

`stdin=subprocess.DEVNULL` ensures codex can never block on a confirmation prompt; combined with `--sandbox workspace-write` or `--dangerously-bypass-approvals-and-sandbox`, this prevents non-interactive deadlock.

## Resume semantics

`codex exec resume <session_id>` does NOT accept `-C`/`--cd` (verified against codex 0.130.0 `--help`). Codex sessions are cwd-pinned by codex's own session storage; resume uses the parent process's cwd to filter sessions. The harness enforces this by:

1. Storing the create-time cwd as `existing.cwd` in the registry.
2. Passing `popen_cwd=existing.cwd` to `Popen` on resume.
3. Ignoring the call-time cwd entirely on the follow-up path.

`codex exec resume` also does NOT accept `--sandbox`; `sandbox_flag_resume(yolo)` returns only `--dangerously-bypass-approvals-and-sandbox` (or nothing) so the inherited sandbox mode applies. The argv test pins both contracts.

### Headless followup vs interactive resume

There are two distinct ways back into a codex agent's conversation, and they map to different verbs:

- **Headless followup** — `fno agents ask <existing-name> "..."` runs `codex exec resume <uuid> --json "..."`, captures the reply, and returns. This is the path the "Resume semantics" rules above describe.
- **Interactive resume** — `fno agents resume <name>` runs `codex resume <uuid>` (no `exec`, no `--json`), which `os.execvp`-replaces the process and drops you into codex's full interactive TUI on the agent's transcript.

`fno agents ask` always creates codex agents via `codex exec`, which codex records with `source: "exec"` (hardcoded at `codex-rs/exec/src/lib.rs`, not configurable). The bare `codex resume` picker filters to `INTERACTIVE_SESSION_SOURCES` (`cli`, `vscode`, `atlas`, `chatgpt`) and so hides `exec` sessions. **However**, `codex resume <uuid>` with an explicit UUID bypasses the picker entirely (`resume_picker = false`) and loads the thread by id with no source filter — so interactive resume works for exec-created agents just the same. `fno agents resume` always passes the UUID, so the picker's source filter never applies. There is no need (and no codex-supported way) to record a session as `source: "cli"` at create time to make it interactively resumable.

## Failure-mode taxonomy

| Failure | Detection | Exit code | Caller experience |
|---|---|---|---|
| codex not on PATH | `FileNotFoundError` on Popen | 127 (provider) / 14 (PATH check) | error message + clean exit |
| 0-event JSONL stream | `session_id is None` after read | 11 | `NoSessionIdError` with `types_seen` surfaced in events.jsonl |
| Wall-clock timeout | watchdog timer fires | 15 | `CodexTimeoutError` |
| Non-zero exit + no reply | exit != 0 and `last_msg == ""` | passes codex's exit code | `CodexInvocationError` with output.jsonl pointer |
| Force-kill (SIGKILL escalation) | `_wait_with_grace` returns `(code, True)` | 1 (or codex's exit if non-zero) | `CodexInvocationError` regardless of any captured reply |
| Tee open failure (EACCES, ENOSPC) | wrap in try/except inside `_run_codex` | 12 | `CodexInvocationError` with stderr WARN |
| Tee write failure | swallow + warn per `(errno, strerror)` mode | n/a (non-fatal) | stderr WARN once per distinct mode; user-facing reply still delivered |
| Registry write failure post-create | `update_registry` raises | 12 | `lock_handle.detach()` to hold the flock so next caller sees manual-cleanup signal |

## From-name injection

Codex has no envelope analogous to claude's `<cross-session-message from-name="...">`. The harness prepends a plain-text bracket prefix to the prompt:

```
[from: orchestrator-main]

<original message>
```

Whether the model attends to the prefix as instructional framing is empirical. Smoke testing against codex 0.130.0 shows the model does NOT routinely echo the marker back in its reply — the prefix reaches the model but is not surfaced through the JSONL events (which describe response, not input). If a future model release demonstrates the prefix is being stripped, the fallback is the AGENTS.md injection pattern. The from-name validator is shared with the claude follow-up path: non-empty, ≤128 chars, no XML-unsafe characters (`"`, `<`, `>`, `&`).

## Audit trail

Every codex dispatch emits matched `agent_ask_started` / `agent_ask_done` (or `agent_followup_*`) events to `~/.fno/events.jsonl`. Each carries `yolo: bool` so a forensic review can answer "which asks ran under the dangerous bypass?" without re-running the dispatch. Failure modes emit `agent_ask_failed` with a `stage` discriminator (`codex-no-session`, `codex-timeout`, `codex-subprocess`, `registry-write`) for downstream autocorrect consumption.

## Open questions

1. **From-name attention.** Whether the model meaningfully attends to `[from: ...]` is empirical; smoke does not gate on it. Future work: track per-from-name response patterns and consider AGENTS.md injection if attention is unreliable.
2. **`--output-last-message` as an alternative to JSONL parsing.** codex's `-o` flag writes the last assistant message to a file. A two-source-of-truth design could capture session_id from JSONL and last_msg from the `-o` file. The current JSONL-only path is simpler and works.
3. **Cross-cwd resume.** codex's `--all` disables cwd filtering. If a future feature wants resume from a different cwd than the create-time cwd, that's the escape hatch; we do not use it today.

## Inherited design rules (from the parent substrate)

- Synchronous semantics (`AskResult.async_=False`).
- Provider mismatch rejection on follow-up.
- Hard-fail orphan path (no `--force-recreate`).
- Per-agent flock around provider invocation; serializes parallel asks on the same name.
- Atomic-rename registry writes preserve `codex_session_id` across all follow-ups.
