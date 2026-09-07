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

## How this was found, so nobody re-buys the wrong turns

The EPERM correlation hunt refuted five explanations with evidence. An untrusted project path: every blocked thread used the trusted canonical repo as cwd. The VS Code surface: present on blocked and working threads alike. The spawn originator: same. Spawn burst concurrency: rates too close to carry a conclusion. The app-server binary: one daemon served both the clean window and the failing one. A same-second correlation between blocked threads and a ChatGPT desktop plugin process stayed unexplained. It is a confound this mechanism does not need.

One proposal was refuted on lane evidence: dropping the headless `if !ctx.yolo` grant guard. That guard governs `codex exec` argv only, where yolo emits the bypass flag and no exec run is ever yolo-sandboxed. Its test states a true invariant and stays. The lane discriminator is sharp. 44 of 49 blocked rollouts carry the `fno-mail-inject` clientInfo of the app-server handshake. Zero carry `codex_exec`. Both `codex exec` rollouts in the same scan worked.

## Known limits

The `yolo` scalar still asks for more than the server delivers: a yolo thread runs `workspaceWrite` server-side with widened roots, not full access. The spawn client also spells its posture as a `yolo` boolean. A `permission_mode` of the same meaning sent under its own key is not read by the thread lane. Both are posture-fidelity gaps, not write gaps: with the grant, a worker at either posture can commit.
