# fno agents — gemini provider commands

The gemini provider lets you spawn and follow up with Google's `gemini` CLI under fno's name registry, per-agent flock, and events.jsonl substrate. It rests on three substrate refactors that generalize the per-provider shapes (the reachability-probe base class, the lock-and-entry context manager, and batched reconcile writes) so a new provider plugs in without touching the shared base.

Parent: [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md). Siblings: [fno-agents-codex-provider.md](fno-agents-codex-provider.md), [fno-agents-lifecycle.md](fno-agents-lifecycle.md).

## Surface

```bash
# Create a gemini agent pinned to the current cwd:
fno agents ask worker-A --provider gemini "draft the migration"

# Follow up on the same session (cwd is ignored; gemini sessions are cwd-pinned):
fno agents ask worker-A "switch to zod"

# Yolo: --yolo passes through to gemini's `-y` flag (sandbox bypass):
fno agents ask worker-bootstrap --provider gemini --yolo "scaffold a Next.js app"

# LLM orchestrator dispatch with a from-name advertised to the worker:
fno agents ask gemini-helper --provider gemini --from-name orchestrator-main "review the migration"
```

No new top-level verbs. The `--provider gemini` value is the only user-visible delta from the codex provider.

## Module layout

```
cli/src/fno/agents/
├── dispatch.py                  # + _gemini_create_path / _gemini_followup_path,
│                                # + reconcile gemini branch, refactored stop/rm
│                                # to use with_agent_lock_and_entry
└── providers/
    ├── base.py                  # + ReachabilityProbeError (lifted)
    ├── claude.py                # ClaudeReachabilityProbeError -> subclass alias
    ├── codex.py                 # SessionIndexReadError -> subclass alias
    └── gemini.py                # NEW: create() + resume() + reachability probe
scripts/
└── lint-flock-pattern.sh        # NEW: forbids hold_agent_lock + _resolve_registry_entry
                                 # co-occurrence outside with_agent_lock_and_entry
cli/scripts/smoke/
└── capture-gemini-json.sh       # NEW: schema-pinning smoke capture
cli/tests/agents/
├── fixtures/gemini-json-sample.json     # committed real-gemini capture
├── fixtures/gemini-smoke-findings.md    # runtime-discovery resolution doc
├── fixtures/fake-gemini-hang.sh         # signal-handling test shim
├── test_provider_base.py                # 19 lift + alias tests
├── test_with_agent_lock_and_entry.py    # 6 context-manager tests
├── test_reconcile_batched.py            # 6 batched-write tests
├── test_provider_gemini.py              # 26 provider unit tests
├── test_dispatch_gemini.py              # 12 routing unit tests
├── test_gemini_signal_handling.py       # 4 real-subprocess SIGTERM/SIGINT tests
├── test_gemini_integration_smoke.py     # 4 end-to-end + drift-detector tests
├── test_gemini_from_name_marker.py      # 1 GEMINI_SMOKE-gated marker test
└── test_dispatch_gemini_lifecycle.py    # 12 stop/rm/reconcile/attach tests
```

## The three substrate refactors

### 1. `ReachabilityProbeError` lift

Previously, each provider defined its own tri-state probe error class (`ClaudeReachabilityProbeError` in `claude.py`, `SessionIndexReadError` in `codex.py`). Adding gemini would have meant a third class with the same shape. The lift moves the contract to `providers/base.py`:

```python
class ReachabilityProbeError(RuntimeError):
    def __init__(self, *, provider: str, reason: str) -> None:
        super().__init__(f"{provider} reachability probe inconclusive: {reason}")
        self.provider = provider
        self.reason = reason
```

`ClaudeReachabilityProbeError` and `SessionIndexReadError` landed as deprecated subclass aliases (each emitting a construction-time `DeprecationWarning`) that survived one release cycle. They have since been removed in the planned follow-up; the file-tree note above reflects their original landing. Every provider's probe now raises `ReachabilityProbeError` directly with its `provider` tag.

`dispatch.py::reconcile_agents` catch sites now catch the base class so future providers plug in without any `base.py` edits.

### 2. `with_agent_lock_and_entry` extraction

Previously, `stop_agent` and `rm_agent` open-coded the pre-flock + `hold_agent_lock` + post-flock re-read pattern. The pre-flock snapshot was a TOCTOU seed — a future contributor that forgot the post-lock re-read would silently operate on stale `provider` / `short_id`.

The new context manager encapsulates the correct shape:

```python
@contextmanager
def with_agent_lock_and_entry(name: str, ...) -> Iterator[tuple[LockHandle, AgentEntry]]:
    _resolve_registry_entry(name)          # pre-flock validation; result discarded
    with hold_agent_lock(name, ...) as lh:
        existing = _resolve_registry_entry(name)  # post-lock re-read
        yield (lh, existing)
```

The pre-flock snapshot is intentionally NOT yielded; callers MUST use the post-lock entry. The CI lint at `scripts/lint-flock-pattern.sh` forbids `hold_agent_lock` + `_resolve_registry_entry` co-occurrence in any `dispatch.py` function outside the helper.

### 3. Batched `reconcile_agents`

Previously, `reconcile_agents` called `update_registry` once per status flip — a registry with N orphaned codex agents triggered N atomic write cycles. The refactor accumulates updates in `pending_updates: dict[str, AgentEntry]` and applies them via one `update_registry` call:

```python
pending_updates: dict[str, AgentEntry] = {}
for entry in entries:
    ...
    if entry.status != new_status:
        pending_updates[entry.name] = <updated AgentEntry>

if pending_updates:
    def _apply(current): return [pending_updates.get(e.name, e) for e in current]
    update_registry(_apply)
```

Atomicity: SIGINT mid-loop or an OSError mid-write either commits ALL pending updates or NONE. The dict shape (vs `list[tuple]`) makes last-writer-wins explicit and prevents a future bug where two probes queue updates for the same name.

## The gemini provider

`providers/gemini.py` is structurally a clone of `providers/codex.py` with three cleavages captured at runtime discovery:

### Single-blob JSON parser

```python
# providers/gemini.py
stdout_text = proc.stdout.read()
session_id, reply = _parse_response(stdout_text)
```

Gemini emits ONE JSON object at EOF, not a per-line stream like codex. `_parse_response` is `json.loads(stdout_text)` after `proc.wait()`. Schema drift (missing keys, non-string `session_id`) raises `GeminiParseError` with the raw 200-char head.

### stderr on a SEPARATE pipe (divergence from codex's STDOUT-merge)

Gemini emits `Ripgrep is not available`, MCP issues, and skill-conflict warnings to stderr at startup. Merging into stdout would corrupt the JSON parse. We use `stderr=subprocess.PIPE` and drain via `_drain_stderr` AFTER `proc.wait()`, teeing to the same `output.jsonl` for forensic continuity.

### `--skip-trust` is unconditional

Gemini refuses to run in headless mode unless the workspace is trusted. We pass `--skip-trust` automatically so the fno user never sees gemini's interactive trust prompt.

### Pinned schema constants

```python
_GEMINI_KEYS = {
    "session": "session_id",   # snake_case
    "reply": "response",
    "stats": "stats",
}
```

The smoke script `scripts/smoke/capture-gemini-json.sh` regenerates the fixture against a live gemini binary. The drift-detector test (`test_pinned_keys_match_fixture`) fails loudly if `_GEMINI_KEYS` diverges from the captured shape.

### Reachability probe

```python
def gemini_session_reachable(session_id: str, cwd: Path) -> bool:
    """Tri-state probe per the lifted ReachabilityProbeError contract.

    Checks ~/.gemini/tmp/<cwd-basename>/chats/session-*-<short-uuid>.jsonl.
    Verifies the full UUID via first-line read. PermissionError raises
    ReachabilityProbeError (provider="gemini", reason=<errno>); a missing
    chats dir also raises (inconclusive) so reconcile preserves status
    instead of mass-orphaning every gemini agent on a fresh host.
    """
```

## Runtime-confirmed behavior

The gemini integration was developed against gemini CLI 0.42.0 and the following invariants were confirmed at runtime (regenerable via the smoke script):

- `gemini -p --resume <uuid>` from the registry-recorded cwd round-trips the same UUID — this means the registry can pin a session id at create time and resume it later.
- Gemini supports a sandbox via `-y/--yolo` and `--approval-mode {default,auto_edit,yolo,plan}`. `--yolo` is the live pass-through path.
- A `--resume <uuid>` invocation from a different cwd than where the session was created fails with `Invalid session identifier`. The provider's `resume()` uses the registry-recorded cwd, NOT the call-time cwd.

## Lifecycle parity

| Verb | Behavior | Notes |
|------|----------|-------|
| stop | no-op between asks; emits `agent_stopped` event | Mirror of codex; PTY signal-the-pgid is not yet wired |
| rm | deletes registry row; preserves on-disk session files | Mirror of codex; gemini owns `~/.gemini/tmp/` |
| reconcile | tri-state probe via `gemini_session_reachable` | Same code path as claude / codex through the lifted base class |
| attach | exit 13 + placeholder hint (interactive attach not yet supported) | `agent_attach_refused` event with `provider="gemini"` |

## Forward compatibility

Adding a fourth provider (e.g. `opencode`) requires zero `base.py` edits — the lifted `ReachabilityProbeError`, `with_agent_lock_and_entry`, and batched `reconcile_agents` already absorb a new provider's tri-state probe + lifecycle verbs. The `dispatch.py` routing extends by one elif clause.

The deprecated alias removal is a separate follow-up. New code should `from fno.agents.providers.base import ReachabilityProbeError` rather than the legacy class names.

## See also

- [fno-agents-codex-provider.md](fno-agents-codex-provider.md) — codex provider (mirror reference)
- [fno-agents-lifecycle.md](fno-agents-lifecycle.md) — stop/rm/reconcile/attach surface
- [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md) — the dispatch substrate every provider hooks into
