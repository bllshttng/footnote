# zcode CLI trial: fno-driven sessions in the ZCode desktop

Trial executed 2026-09-26 against ZCode.app 3.14.3 (CLI bundle 0.16.9). The question: does a CLI session started by fno, not by the desktop app, appear in the desktop task list and its phone remote control, and after what refresh? This decides between a declared harness row, a desktop-visible lane, or skipping zcode.

## Result

YES on cold load. Two headless `-p` sessions run by fno in the desktop's open workspace appeared in the desktop task index after the desktop app restarted: both rows read `completed`, provider `glm`, keyed by their CLI session ids. No live pickup was observed: the task index stayed empty 60 seconds after creation and filled only on restart. Phone remote control rides the desktop's relay state (`webRemoteControlExternalRelayDevice` in `~/.zcode/v2/setting.json`), so it inherits the desktop list; confirming on an actual phone is the user's check.

## Mechanics, traced in source and confirmed live

- The standalone CLI has no workspace-identity source. `packages/bootstrap/src/zcode-protocol/official-mcp-auth-port.ts` states this in a comment, and both trial session rows landed in `~/.zcode/cli/db/db.sqlite` with `workspace_id` NULL.
- NULL is not a defect for local workspaces. The desktop's stored-summary load queries `listSessions({directory: <workspace path>, workspaceID: null, taskTypes: [interactive, fork, workflow_parent]})` (`packages/bootstrap/src/zcode-protocol/v4-bridge.ts`, `loadStoredSessionSummaries`); `interactive` is in `TASK_LIST_SESSION_TYPES` (`zcode-protocol-v4/task-list-session-membership.ts`).
- The task index (`~/.zcode/v2/tasks-index.sqlite`) is a sync cache fed by the desktop's own app-server; fno cannot write it and needs no write.
- Visibility boundary: only sessions whose `directory` equals the open workspace's path are listed. The desktop currently has `~/.zcode/workspace/default` open (purpose `conversation`). A worker running in any other directory is invisible until that directory is opened as a workspace; opening the footnote repo as a workspace would make in-repo sessions visible by the same query.
- The desktop owns its app-server's stdio, so fno cannot steer it; fno drives its own `-p` (or app-server) process against the shared db. For one-shot turns `-p --output-format json|stream-json` is the machine-readable surface and the one measured here; `app-server` adds resume, steering and live events and is the next measurement if fno needs more than one-shot.

## One-time setup that unlocks the headless run

The shipped bundle cannot run headless out of the box. Three config facts, all on this machine:

1. Provider config. The bundled CLI looks for `provider/zcode-builtin.json` beside the binary and then at `config/provider/zcode-builtin.json` relative to the entry (`apps/zcode-cli/packages/cli/src/provider-runtime-env.ts`); the packaged app carries neither. Run with `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` pointing at a valid copy (the source tree has one). The first successful run materializes the active copy under `~/.zcode/v2`'s cache.
2. Model selection. The default comes from `defaultModelSelection` in `~/.zcode/v2/provider_config.json` (`NodeModelSelectionConfigRepository`); the legacy `model.main` in `~/.zcode/cli/config.json` imports only on first creation of the personal file. Without it every turn dies with "Select a model before continuing" at `turnPhase: model_creation`. Set `{"providerId": "account:zai-individual-coding-plan", "modelId": "GLM-5.3-Flash"}`.
3. Identity pointer. The shared credential store held the account's api-key entry but not its identity key (`account-provider:<providerId>:identity`). Without the pointer the standalone registry marks the account not entitled and the account provider exits the registry. The repair derives the identity from the existing api-key key name; it was applied and the desktop app came back up healthy after a restart.

## Cost: same task, both lanes

Fixed task: reply with one exact word. Both lanes hit the same endpoint (`api.z.ai/api/anthropic`) with the same model (GLM-5.3-Flash).

| lane | input tokens | output tokens |
|---|---|---|
| zcode `-p --output-format json` | 20532 (15616 cache-read) | 34 |
| claude `-p` with zai-routed env | 31347 (64 cache-read) | 110 |

zcode's client overhead is roughly half of Claude Code's on a trivial turn. One sample each: treat it as a prompt-size delta (system prompt plus tool surface), not model tuning. The z.ai quota endpoint did not move across both runs (5h window 8.0% before and after, 1m 0.225%), so single-run cost sits below that endpoint's reporting granularity; provider-reported token counts are the honest per-run metric.

## Decision

- Declared harness row: still no. It needs a `db.sqlite` store reader for session identity, a callee-minted session binding, a loop adapter behind a three-continuation Stop cap (`MAX_STOP_HOOK_CONTINUATIONS`, `packages/core/src/runtime/methods/hooks.ts`), and there is no launch-time model flag. The undeclared lane delivers the user-visible win today at zero fno code.
- Desktop-visible lane: adopt for zai visibility. fno drives `zcode -p --output-format stream-json` with cwd inside a desktop-open workspace; sessions surface in the desktop and its phone remote control after a desktop restart.
- The original skip premise (CLI sessions cannot be seen remotely) is measured false, conditional on the setup facts above.

## Known limits

- No live refresh: an app restart is the only observed pickup path.
- No per-spawn model axis: model selection is config-file state; per-spawn isolation via `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE` is untested.
- Headless `-p` defaults to yolo permission mode (`run.ts`), and the CLI source carries no OS sandbox; spawned zcode workers are unsandboxed.
- The desktop restart in this trial ran with the app idle and no tasks.
