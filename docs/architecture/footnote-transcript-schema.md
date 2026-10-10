# footnote session record, v1

Every session file carries the session id. Under `~/.fno/sessions/<project-slug>/` a session is a record file beside a sidecar dir, Claude Code style. The slug is the canonical checkout's space slug, or `_none` outside a repo:

- `<fno_id>.jsonl` is the record. It is append-only, mode 0600, and each line is fsynced. Child transcripts later land inside the sidecar dir as `<child_id>.jsonl`.
- `<fno_id>/` is the sidecar dir. It refuses a second create: an id names one session, never a reused one.
- `<fno_id>/<fno_id>.lock` holds an exclusive `flock` for the life of the one writer.
- `<fno_id>/<fno_id>.index.db` holds one content-free row per line (`seq`, `record_id`, `offset`, `len`, `turn`, `tool_call_uid`). Its event_store journal is `<fno_id>/<fno_id>.index.jsonl`, so the store path maps to the db. When an index write fails, the transcript stays the record.
- `<fno_id>/<tool_call_uid>.out` holds a tool output over 8 KiB, whole. The uid is already unique, so spill files sit directly in the dir.
- `<fno_id>/<fno_id>.diag.log` holds diagnostics, rotated to `<fno_id>.diag.log.1`. The API key is redacted. It is never part of the record.

## Envelope

`{"v": 1, "seq": n, "id": "<uuid>", "ts": "<RFC3339 ms>", "session_id": "<fno_id>", "type": "<type>", "data": {...}}`, plus `"ignorable": true` on a log-only record.

`seq` starts at 0 and rises by one per line. `id` is a fresh v4 UUID, never a hash. A reader refuses an unknown `type` that is not `ignorable`. A new type does not raise `v`.

## Types

| type | data |
|---|---|
| `header` | `session_id`, `cwd`, `harness`, `commit`, `branch`, `fno_version`, `parent_session_id`, `forked_from` (reserved, null), `node`, `plugin_root`, `model`, `wire`, `system_prompt`, `hook_sources` (`[{path, kind, status, error}]`, status absent, loaded or broken), `permission_mode`, `permission_mode_source` (launch_spec, `settings:<path>`, built_in), `permission_sources` (`[{path, status, allow, deny, ask}]`, rule counts per file) |
| `turn_context` | `turn`, `model`, `cwd`, `spent_usd`, `wall_secs`, `cost_cap_usd`, `wall_cap_secs`, `permission_mode` |
| `user_input` | `text`, `origin` (operator, hook, skill) |
| `model_request` | `turn`, `provider_id`, `endpoint_host`, `wire`, `route`, `requested_model`, `message_count`, `estimated_input_tokens`, `body_sha256`. Never the body, key or headers. |
| `model_attempt` | ignorable. `attempt`, `status`, `error_class`, `detail`, `latency_ms`, `usage` |
| `model_response` | `turn`, `provider_response_id`, `reported_model`, `content` (Anthropic blocks), `stop_reason`, `latency_ms`, `attempts` |
| `usage` | `turn`, `provider_response_id`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `cost_usd` |
| `tool_call` | `turn`, `tool_call_uid` (footnote's), `provider_tool_call_id` (the wire's), `name`, `raw_input`, `input`, `parse_error` |
| `effect_decision` | `tool_call_uid`, `effect_class`, `effect_capable`, `verdict`, `source` (parse_error, hook_load, deny_rule, hook, allow_rule, mode, effect_class, default), `rule`, `reason`, `hook_decision`, `mode`, `settings_path`, `principal` |
| `tool_start` | `tool_call_uid` |
| `tool_result` | `tool_call_uid`, `disposition` (none, applied, unknown), `is_error`, `model_text`, `size`, `sha256`, `spill_path` |
| `hook_decision` | `event`, `source` (SessionStart only: startup, resume, compact), `verdict`, `rule`, `reason`, `context` |
| `hook_run` | ignorable. One failed observer run: `event`, `command`, `source`, `exit`, `duration_ms`, `error` |
| `compaction` | `summary`, `tokens_before`, `tokens_after`, `trigger`, `retained_tail`, `messages`, `plan_path`, `plan_anchor`, `plan_anchor_sha256`, `plan_anchor_missing`, `replaced_seq_range` |
| `terminal` | `state` (done, budget, refused, interrupted, error), `reason`, `turns`, `tool_calls`, `input_tokens`, `output_tokens`, `cost_usd` |

`model_text` is exactly what the model saw: at most 30,000 bytes, plus the spill path when the output was longer. Storing it in the record keeps one rule true: the next request rebuilds from the transcript alone (`resume::build_request`), byte for byte, and its sha256 equals `body_sha256`.

## Launch spec

The supervisor writes one `LaunchSpec` (`crates/fno-agents/src/footnote_transcript.rs`) to the binary's stdin. Its `v` is 2. The binary refuses any other `v` with exit 2 and the install command. So a stale binary never runs a newer spec. v2 adds `permission_mode`: `default`, `acceptEdits`, `plan`, `dontAsk` or `bypassPermissions`, from the capability row's `permission_modes`. Null runs the binary's own default. A resume sends the mode the registry row recorded at spawn.

## Resume

Resume reads from the last `compaction` record forward. For a `tool_call` with no `tool_result`:

- no `tool_start`: the call never ran, so it runs now, through PreToolUse again.
- `tool_start` on Read, Glob, Grep or Skill: it runs again.
- `tool_start` on anything else: it never runs again. A `tool_result` with disposition `unknown` tells the model.
