# zcode CLI trial: fno-driven sessions in the ZCode desktop

Trial executed 2026-09-26 against ZCode.app 3.14.3 (CLI bundle 0.16.9). The question: does a CLI session started by fno appear in the desktop task list and its phone remote control? After what refresh? The answer decides between a declared harness row, a desktop-visible lane, or skipping zcode.

## Result

YES on cold load. Two headless `-p` sessions run by fno appeared in the desktop task index after an app restart. Both rows read `completed`, provider `glm`, keyed by their CLI session ids. No live pickup: the index stayed empty 60 seconds after creation. It filled only on restart. Phone remote control rides the desktop relay (`webRemoteControlExternalRelayDevice` in `~/.zcode/v2/setting.json`). It inherits the desktop list. Confirming on an actual phone is the user's check.

## Mechanics, traced in source and confirmed live

- The standalone CLI has no workspace-identity source. `official-mcp-auth-port.ts` states this in a comment. Both trial rows landed in `~/.zcode/cli/db/db.sqlite` with `workspace_id` NULL.
- NULL is not a defect for local workspaces. The desktop's summary load queries `listSessions` with `directory` set to the workspace path and `workspaceID: null`. Task types come from `TASK_LIST_SESSION_TYPES`, and `interactive` is included.
- The task index (`~/.zcode/v2/tasks-index.sqlite`) is a sync cache. The desktop's own app-server feeds it. fno cannot write it and needs no write.
- Visibility boundary: only sessions whose `directory` equals the open workspace path get listed. The desktop currently has `~/.zcode/workspace/default` open. A worker in any other directory is invisible there. Opening the footnote repo as a workspace makes in-repo sessions visible by the same query.
- The desktop owns its app-server's stdio. fno cannot steer that process. fno drives its own `-p` or app-server process against the shared db. One-shot turns measured here used `-p --output-format json` and `stream-json`. When fno needs resume, steering or live events, `app-server` is the next measurement.

## One-time setup that unlocks the headless run

The shipped bundle cannot run headless out of the box. Three config facts unlock it on this machine:

1. Provider config. The CLI looks for `provider/zcode-builtin.json` beside the binary, then relative to the entry (`provider-runtime-env.ts`). The packaged app carries neither. Run with `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` pointing at a valid copy. The source tree has one. The first successful run materializes the active copy under the `~/.zcode/v2` cache.
2. Model selection. The default comes from `defaultModelSelection` in `~/.zcode/v2/provider_config.json`. The legacy `model.main` imports only on first creation of that file. Without a default every turn dies with "Select a model before continuing" at `turnPhase: model_creation`. Set `{"providerId": "account:zai-individual-coding-plan", "modelId": "GLM-5.3-Flash"}`.
3. Identity pointer. The credential store held the account api-key but not its identity key (`account-provider:<providerId>:identity`). Without the pointer the registry marks the account not entitled. The account provider then exits the registry. The repair derives the identity from the existing key name. It was applied, and the desktop app came back healthy.

## Cost: same task, both lanes

Fixed task: reply with one exact word. Both lanes hit `api.z.ai/api/anthropic` with GLM-5.3-Flash.

| lane | input tokens | output tokens |
|---|---|---|
| zcode `-p --output-format json` | 20532 (15616 cache-read) | 34 |
| claude `-p` with zai-routed env | 31347 (64 cache-read) | 110 |

zcode's client overhead is roughly half of Claude Code's on a trivial turn. One sample each. Treat the delta as prompt size (system prompt plus tool surface), not model tuning. The z.ai quota endpoint did not move across both runs. The 5h window read 8.0% before and after, the 1m window 0.225%. Single-run cost sits below that endpoint's granularity. Provider-reported token counts are the honest per-run metric.

## Decision

- Declared harness row: still no. It needs a `db.sqlite` store reader, a callee-minted session binding, and a loop adapter behind the three-continuation Stop cap (`hooks.ts`). There is no launch-time model flag. The undeclared lane delivers the user-visible win today at zero fno code.
- Desktop-visible lane: adopt for zai visibility. fno drives `zcode -p --output-format stream-json` with cwd inside a desktop-open workspace. Sessions surface in the desktop and its phone remote control after a restart.
- The original skip premise measured false: CLI sessions can be seen remotely, conditional on the setup facts above.

## Known limits

- No live refresh: an app restart is the only observed pickup path.
- No per-spawn model axis: model selection is config-file state. Per-spawn isolation via `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE` is untested.
- Headless `-p` defaults to yolo permission mode (`run.ts`). The CLI source carries no OS sandbox. Spawned zcode workers are unsandboxed.
- The desktop restart in this trial ran with the app idle and no tasks.
