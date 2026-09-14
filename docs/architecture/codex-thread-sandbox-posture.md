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

The switch was located with `codex sandbox` on 2026-09-13, driven from outside the lane. With `sandbox_workspace_write.network_access = false`, a Python AF_UNIX connect to `~/.fno/graph.json.store.sock` returned errno 1. `gh api user` did not connect. With `network_access = true`, the connect succeeded. `gh api user --jq .login` printed the login from the keychain token. Controls held both ways. Outside the sandbox the connect succeeded. With network off, a write outside the workspace was denied, and curl did not resolve a host.

The live-server arms prove the turn carrier. They ran on 2026-09-14 on codex-cli 0.154.0. The setup: one private `codex app-server --listen stdio://`. One thread started with the `workspace-write` scalar and `approvalPolicy: never`. Then two `turn/start` frames, each carrying a full `sandboxPolicy` with `writableRoots: []`. Outcomes are read from `item/completed` command outputs, never from model prose. A nonce file per arm is the control:

- `networkAccess: false`: the AF_UNIX connect returned errno 1 and the nonce file was written. `gh api user --jq .login` printed `bllshttng` with exit 0.
- `networkAccess: true`: the connect returned 0. `gh api user --jq .login` printed `bllshttng`. The nonce file was written.

The false arm carries one unexplained reading. It contradicts the 2026-09-13 seatbelt run: the same network-off posture that refused the unix socket let `gh api user` reach the GitHub API. A loopback-proxy path is plausible and unverified. Do not build on either side of that contradiction. The unix-socket refusal is the measured fact this lane rides.

The scalar posture reading moved with the server version. On 2026-08-28 a `thread/start` with the `workspace-write` scalar resolved `networkAccess: false`. On 2026-09-14 the same scalar resolved `networkAccess: true` on codex-cli 0.154.0. This machine sets `[sandbox_workspace_write] network_access = true` in `~/.codex/config.toml`. The per-turn frame is what the lane controls. The false arm shows it overrides the resolved posture in the restricting direction too.

The exec and resume lanes and the pane TUI still take network from `~/.codex/config.toml`. They read the config file or build their own policy. fno sends them no sandbox object.

## How this was found, so nobody re-buys the wrong turns

The EPERM correlation hunt refuted five explanations with evidence. An untrusted project path: every blocked thread used the trusted canonical repo as cwd. The VS Code surface: present on blocked and working threads alike. The spawn originator: same. Spawn burst concurrency: rates too close to carry a conclusion. The app-server binary: one daemon served both the clean window and the failing one. A same-second correlation between blocked threads and a ChatGPT desktop plugin process stayed unexplained. It is a confound this mechanism does not need.

One proposal was refuted on lane evidence: dropping the headless `if !ctx.yolo` grant guard. That guard governs `codex exec` argv only, where yolo emits the bypass flag and no exec run is ever yolo-sandboxed. Its test states a true invariant and stays. The lane discriminator is sharp. 44 of 49 blocked rollouts carry the `fno-mail-inject` clientInfo of the app-server handshake. Zero carry `codex_exec`. Both `codex exec` rollouts in the same scan worked.

## Known limits

The `yolo` scalar still asks for more than the server delivers: a yolo thread runs `workspaceWrite` server-side with widened roots, not full access. The spawn client also spells its posture as a `yolo` boolean. A `permission_mode` of the same meaning sent under its own key is not read by the thread lane. Both are posture-fidelity gaps, not write gaps: with the grant, a worker at either posture can commit.

The pre-launch sandbox probe judges the `~/.codex/config.toml` posture, not the turn frame. On a network-off config it can refuse a bounded thread spawn that the turn-level network grant supports.
