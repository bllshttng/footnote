# Gemini smoke-discovery findings

Captured against **gemini CLI 0.42.0** on macOS 25.3.0 (darwin/arm64).
The smoke script that produced this is at
`cli/scripts/smoke/capture-gemini-json.sh`; the JSON fixture is at
`cli/tests/agents/fixtures/gemini-json-sample.json`.

This document records the runtime-discovery findings that the gemini
provider module pins as constants (`_GEMINI_KEYS`, the reachability
probe layout, the `--skip-trust` requirement) and the design decisions
that survived smoke testing.

## OQ1 — Does `gemini -p --resume <id>` return the same session id?

**YES** (Locked Decision 9 holds). Verified via:

```
$ UUID=cedb6b44-d140-4fa4-86f1-3b3e7aed339d
$ cd /tmp/gemini-test
$ gemini --skip-trust -p "what was the magic word?" --resume "$UUID" --output-format json
{"session_id": "cedb6b44-d140-4fa4-86f1-3b3e7aed339d", "response": "The magic word is **TURNIP**.", ...}
```

`--resume <uuid>` round-trips the same UUID provided it was the
session id of a session that exists in `--list-sessions` for the
current cwd.

**Caveat:** `--resume <uuid>` from a DIFFERENT cwd than where the
session was created fails with:

```
Error resuming session: Invalid session identifier "<UUID>".
  Use --list-sessions to see available sessions, then use
  --resume {number}, --resume {uuid}, or --resume latest.
```

Implication for the provider module: `gemini.resume()` MUST run with
cwd set to the registry-recorded `cwd` field (NOT the call-time cwd).
This mirrors codex behavior (Locked Decision 1) and is asserted in
AC5-EDGE.

## OQ2 — Does `[from: <name>]` reach the model context?

**Deferred to Wave 2.3 smoke marker test** (`@pytest.mark.smoke`,
gated by `GEMINI_SMOKE=1`). The test prompts the model to echo its
from-name annotation; assertion is the substring of the marker
appears in the response. Test lives in
`cli/tests/agents/test_gemini_from_name_marker.py`. AC7-HP (smoke
boundary) and AC7-ERR (loud failure if absent) are pinned there.

## OQ3 — Does `gemini -p` without `--session-id` auto-resume the latest cwd-bound session, or always create a fresh one?

**ALWAYS CREATES A FRESH SESSION.** Verified via:

```
$ cd /tmp/gemini-test
$ gemini --skip-trust -p "remember the magic word: TURNIP" --output-format json
{"session_id": "cedb6b44-d140-4fa4-86f1-3b3e7aed339d", "response": "I will remember...", ...}

$ gemini --skip-trust -p "what was the magic word?" --output-format json
{"session_id": "<DIFFERENT-UUID>", "response": "I don't have prior context...", ...}
```

Each invocation without `--resume` gets a fresh session_id even with
identical cwd + prompt content. Implication for the provider module:
fno MUST pass either `--session-id <uuid>` on create OR `--resume <uuid>`
on follow-up. The "auto-resume latest" path is a non-option.

## OQ4 — JSON schema (`--output-format json`)

**SPEC AMENDMENT REQUIRED.** The design doc assumed `sessionId`
(camelCase) for the session id field; the actual schema uses
`session_id` (snake_case). Top-level shape:

```json
{
  "session_id": "<uuid>",
  "response": "<assistant text>",
  "stats": {
    "models": {
      "<model-name>": {
        "api": {"totalRequests": int, "totalErrors": int, "totalLatencyMs": int},
        "tokens": {"input": int, "output": int, "cached": int, ...}
      }
    },
    "tools": {...},
    "files": {...}
  }
}
```

The provider module's `_GEMINI_KEYS` constants block MUST pin
`session_id` (snake_case). Internal gemini storage at
`~/.gemini/tmp/<cwd-basename>/chats/session-*.jsonl` uses
`sessionId` (camelCase), but that file format is private and not
part of the fno contract.

Empty-reply distinction (silent-failure-hunter row 2 in the design
doc): if the model produced no text, `response` is `""` (empty
string), not `null`. A `null` response would indicate a gemini-side
error and surfaces to stderr with the model's error field intact.

## OQ5 — Does gemini have a sandbox mode?

**YES** — `gemini -s/--sandbox` exists, plus a richer
`--approval-mode {default,auto_edit,yolo,plan}` selector and the
legacy `-y/--yolo` shorthand (equivalent to `--approval-mode yolo`).

Implication for the provider module: `--yolo` is a real
pass-through. AC6-NA (no-sandbox graceful degrade) does NOT apply for
gemini; AC6-HP (yolo flag passes through) is the live path. Mirror
codex's `--yolo` plumbing.

## OQ6 — Session lifetime

Sessions persist at:

```
~/.gemini/tmp/<cwd-basename>/chats/session-<YYYY-MM-DDTHH-MM>-<short-uuid>.jsonl
```

`<cwd-basename>` is the basename of the cwd when the session was
created. `<short-uuid>` is the first 8 hex characters of the
session_id.

**Lifetime:** indefinite — sessions persist until the operator
deletes them (via `gemini --delete-session <index>` or by removing
the file directly). There is no auto-expiry mechanism observed in
gemini 0.42.0.

**Implication for the reachability probe:** check the existence of
a session file matching the recorded session_id's short prefix in
the cwd-pinned project directory. Tri-state:

- File present + readable + matches full UUID → `True` (live).
- File missing → `False` (orphaned).
- `PermissionError` on stat / parent dir unreadable →
  `ReachabilityProbeError(provider="gemini", reason=<errno>)`.

## OQ7 — Old class aliases removed in this PR or follow-up?

**FOLLOW-UP PR** (Locked Decision 10). The
`ClaudeReachabilityProbeError` / `SessionIndexReadError` aliases
land as deprecated subclasses in this PR (Wave 1.1) and drop in a
separate PR after one release cycle (~4 weeks). Rationale: minimize
churn for any third-party importers that pinned to the old names.

## Additional findings (not in original OQ list)

### stderr contamination on stdout-captured JSON

Gemini prints structural warnings to **stderr** during startup:

- `Ripgrep is not available. Falling back to GrepTool.`
- `MCP issues detected. Run /mcp list for status.`
- Skill conflict warnings (one per overridden skill in the
  per-user skills directory).

These do NOT corrupt stdout's JSON, but they DO mean the provider
module MUST NOT merge stderr into stdout (the way codex does via
`stderr=subprocess.STDOUT`). Gemini's stderr is large enough that
a stdout-merge would corrupt the JSON parse.

**Implication:** `gemini.create()` uses `stderr=subprocess.PIPE`
(separate), drains stderr concurrently via a background thread,
and tees stderr lines to `output.jsonl` alongside the stdout JSON
blob. The structural-warning noise lands in the tee for forensics;
the dispatch layer's `--quiet` flag (if added later) suppresses it
to the operator's terminal.

### Trusted-folders interactive prompt blocks headless

Gemini refuses to run in headless mode unless the workspace is
trusted. To bypass:

- `gemini --skip-trust ...` (explicit flag), OR
- `GEMINI_CLI_TRUST_WORKSPACE=true gemini ...` (env var).

**Implication:** `gemini.create()` and `gemini.resume()` MUST pass
`--skip-trust` automatically. The fno user does NOT see or interact
with gemini's trust-folder prompt.

### --resume takes UUID, number, or "latest"

Gemini's `--resume` accepts three forms:

1. `--resume <full-uuid>` — resume by session id (the fno pattern).
2. `--resume <integer>` — resume by index (1-based) in --list-sessions output.
3. `--resume latest` — resume the most recent session.

fno only uses form 1. Forms 2 + 3 are operator-facing affordances.

## Pinned constants block

The following will land in `cli/src/fno/agents/providers/gemini.py`
as `_GEMINI_KEYS`:

```python
_GEMINI_KEYS = {
    "session": "session_id",
    "reply": "response",
    "stats": "stats",
}
```

A future drift in any of these keys will fail the smoke test in
Wave 2.3 (`test_gemini_integration_smoke.py::test_pinned_keys_match_runtime_capture`).
