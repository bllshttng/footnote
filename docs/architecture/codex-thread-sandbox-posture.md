# Codex thread lane: sandbox posture and the git grant

What sandbox posture a codex worker resolves to on each launch lane. What that worker can write. Why the git common dir rides every `turn/start` on the thread lane. Settled 2026-09-06 after 36 of 65 codex threads that day died creating their feature branch with `Unable to create .git/refs/heads/<branch>.lock: Operation not permitted`.

Read this with [codex-thread-driver.md](codex-thread-driver.md), which records the transport and the 2026-08-28 live probe that showed `turn/start` honors `sandboxPolicy.writableRoots` while `thread/start` ignores a policy object.

## Two lanes, two meanings for yolo

fno launches a codex worker two ways, and the same `yolo` boolean means a different thing on each.

| | headless (`codex exec`) | thread (app-server) |
|---|---|---|
| carrier | argv | JSON-RPC frames |
| yolo emits | `--dangerously-bypass-approvals-and-sandbox` | `"sandbox": "danger-full-access"` scalar on `thread/start` |
| yolo effect | the sandbox is really gone; grants are moot and correctly withheld | the scalar reaches the server, but the worker still runs under the server's `workspaceWrite` default |
| git grant | `codex_git_writable_args` on the bounded posture, one shared resolver | `granted_roots` on every posture, same resolver |

The trap is the asymmetry in the middle row. On the headless lane, withholding the git grant under yolo is correct: a bypassed process has no sandbox to grant against. On the thread lane, withholding the policy does not remove a sandbox. A `turn/start` without `sandboxPolicy` means "keep whatever the server already had", and what the server already had was `workspaceWrite` with the project `.git` read-only. The worker was sandboxed with every grant suppressed, the one combination that cannot write `.git` at all. That is also why the failure was always EPERM and never EEXIST: lock contention reports `File exists`, a permission denial does not.

The machine-level `sandbox_mode = "danger-full-access"` in `~/.codex/config.toml` does not save a thread either. `thread/start` takes an explicit scalar, and the measured behavior keeps the server default for the sandbox object the lane never sends.

## The grant

Every thread now carries `granted_roots` on every `turn/start`. The roots are the caller's state dirs plus the repo's git common dir. The exec lane grants with the same `provider::git_common_dir` resolver. The roots are additive to the workspace. When the server reports a resolved posture, the policy echoes it, so nothing else about the posture changes. Both resolvers are fail-open: an unresolvable root is skipped, never a failed spawn.

The grant rides every turn rather than only the first, because a turn-level policy becomes the thread default and a resumed thread re-resolves its posture. Sending it per turn makes resume carry it for free.

## Network access

Bounded thread turns carry `networkAccess: true`. The keeper socket and `gh` egress share one seatbelt switch. With network left off, a directory grant cannot claim a node. Every graph-keeper call dies on `graph store unavailable (unreachable): [Errno 1] Operation not permitted` before the worker changes anything. Operator ruling 2026-09-10: grant network on the turn policy and keep `workspaceWrite`.

The switch was located with `codex sandbox` on 2026-09-13, driven from outside the lane. With `sandbox_workspace_write.network_access = false`, a Python AF_UNIX connect to the store socket under `~/.fno/` returned errno 1. `gh api user` did not connect. With `network_access = true`, the connect succeeded. `gh api user --jq .login` printed the login from the keychain token. Controls held both ways. Outside the sandbox the connect succeeded. With network off, a write outside the workspace was denied, and curl did not resolve a host.

The live-server arms prove the turn carrier. They ran on 2026-09-14 on codex-cli 0.154.0. The setup: one private `codex app-server --listen stdio://`. One thread started with the `workspace-write` scalar and `approvalPolicy: never`. Then two `turn/start` frames, each carrying a full `sandboxPolicy` with `writableRoots: []`. Outcomes are read from `item/completed` command outputs, never from model prose. A nonce file per arm is the control:

- `networkAccess: false`: the AF_UNIX connect returned errno 1 and the nonce file was written. `gh api user --jq .login` printed `bllshttng` with exit 0.
- `networkAccess: true`: the connect returned 0. `gh api user --jq .login` printed `bllshttng`. The nonce file was written.

The false arm carries one unexplained reading. It contradicts the 2026-09-13 seatbelt run: the same network-off posture that refused the unix socket let `gh api user` reach the GitHub API. A loopback-proxy path is plausible and unverified. Do not build on either side of that contradiction. The unix-socket refusal is the measured fact this lane rides.

The scalar posture reading moved with the server version. On 2026-08-28 a `thread/start` with the `workspace-write` scalar resolved `networkAccess: false`. On 2026-09-14 the same scalar resolved `networkAccess: true` on codex-cli 0.154.0. This machine sets `[sandbox_workspace_write] network_access = true` in `~/.codex/config.toml`. The per-turn frame is what the lane controls. The false arm shows it overrides the resolved posture in the restricting direction too.

The exec and resume lanes and the pane TUI still take network from `~/.codex/config.toml`. They read the config file or build their own policy. fno sends them no sandbox object.

## How this was found, so nobody re-buys the wrong turns

The EPERM correlation hunt refuted five explanations with evidence. An untrusted project path: every blocked thread used the trusted canonical repo as cwd. The VS Code surface: present on blocked and working threads alike. The spawn originator: same. Spawn burst concurrency: rates too close to carry a conclusion. The app-server binary: one daemon served both the clean window and the failing one. A same-second correlation between blocked threads and a ChatGPT desktop plugin process stayed unexplained. It is a confound this mechanism does not need.

One proposal was refuted on lane evidence: dropping the headless `if !ctx.yolo` grant guard. That guard governs `codex exec` argv only, where yolo emits the bypass flag and no exec run is ever yolo-sandboxed. Its test states a true invariant and stays. The lane discriminator is sharp. 44 of 49 blocked rollouts carry the `fno-mail-inject` clientInfo of the app-server handshake. Zero carry `codex_exec`. Both `codex exec` rollouts in the same scan worked.

## Posture fidelity (was: Known limits)

Both posture-fidelity gaps the section above named are closed.

The spawn client no longer spells its posture as a `yolo` boolean on the thread lane. `resolve_thread_posture` resolves the typed pair - sandbox half plus approval half - and both halves ride `thread/start`, `thread/resume`, and every `turn/start`. A `permission_mode` of `read-only:on-request` launches read-only with approvals on request, and the exact string is stamped on the row as `requested_permission_mode` (schema v35) so a resume replays the request, not a derived name. Fail-closed refusals name the codex vocabulary: an unknown sandbox half, an unknown approval half, or both keys at once refuse the spawn instead of degrading to bounded.

The per-turn policy never fabricates a posture. It echoes the server's RESOLVED posture with only the roots widened (`workspaceWrite`), or - when the server names no sandbox - builds from the recorded request, stamped `turn_policy_source: requested` on the row. Requested, resolved, and current-turn policy are therefore three distinguishable readings on every row. Nothing narrows a deliberately bounded run, and nothing invents a bounded posture for a full-access thread: a resolved `dangerFullAccess` posture is echoed unchanged, and an unresolved full-access request builds a `dangerFullAccess` policy rather than a `workspaceWrite` one. A resolved `readOnly` posture is echoed without any widening.

The yolo scalar caveat stands as a SERVER fact, not an fno gap: a thread started with the `danger-full-access` scalar may still run `workspaceWrite` server-side. fno names what it asked for (`sandbox_posture`), what the server resolved (`resolved_sandbox`), and what each turn carries (`granted_writable_roots` + `turn_policy_source`), so the gap is measurable instead of silent.

The pre-launch sandbox probe judges the worker's OWN requested posture (never a hardcoded `workspace-write`), and proves access with a harmless canary write inside the granted roots beside the `gh` and git ref-lock checks. Its negative control closes the detector gap: a write OUTSIDE the granted roots must fail, and when that control succeeds the verdict is `unknown`, never `reachable` - a detector that cannot fail has proved nothing. Exit 85 and the `sandbox-probe:` marker are unchanged.

## The default scope (an operator decision, recorded open)

Footnote reads no `~/.codex/config.toml` and writes none. The one key that sets a codex worker's default posture is `config.agents.defaults.harness.codex.permission_mode` (and its per-verb overlays), and it is UNSET by default: a worker launched with nothing named keeps the lane's bounded default, `workspace-write:never`. The key sits in the existing harness-overlay precedence line documented in [role-based-model-routing.md](role-based-model-routing.md).

No default widens any existing posture. A configured `bypassPermissions` (a claude-only token) refused by name on a codex lane rather than degrading open to an unnamed posture. The machine-wide alternative - `sandbox_mode = "danger-full-access"` in `~/.codex/config.toml` - stays an open operator decision this project does not make for them.

## Native subagent inheritance

A Codex native subagent inherits the parent thread's current effective posture: there is no separate child knob upstream, so the parent's per-turn policy is what a child runs under. fno records the parent's effective policy on the row (`turn_policy_source` plus the resolved posture); when that reading is `unknown`, a child's inherited posture reads `unknown` too and is never reported as permitted. The roster names the parent thread id beside the inherited posture rather than deriving a second answer.
