# L12: Worktrees

**Medium:** Asciinema cast

**The one thing:** Worktree commands create an isolated checkout, report its live state, preview cleanup, preserve its branch on archive, and expose claims separately.

## Setup state

Run the shared setup in [README.md](README.md). The demo repository must have no existing branch or worktree named `lesson12-sandbox`.

## 1. Read the policy

```run
fno worktree policy --repo . --harness codex
```

```expected
external
base=/Users/Shared/footnote-recording-demo/state/worktrees
```

## 2. Ensure the isolated checkout

```run
set -o pipefail
fno worktree ensure --repo . --name lesson12-sandbox --harness codex 2>&1 | tee "$DEMO_ROOT/l12-worktree.txt"
DEMO_WT="$(tail -1 "$DEMO_ROOT/l12-worktree.txt")"
```

```expected
worktree ensure: policy=external (footnote) requested=harness-native degraded=true; worktree at /Users/Shared/footnote-recording-demo/state/worktrees/repo/lesson12-sandbox
/Users/Shared/footnote-recording-demo/state/worktrees/repo/lesson12-sandbox
```

## 3. Read its status

```run
set -o pipefail
fno worktree status --json | jq -c '.worktrees[] | select(.branch == "feature/lesson12-sandbox") | {branch,path,target}'
```

```expected
{"branch":"feature/lesson12-sandbox","path":"/Users/Shared/footnote-recording-demo/state/worktrees/repo/lesson12-sandbox","target":"none"}
```

## 4. Preview merged cleanup

```run
set -o pipefail
fno worktree cleanup --merged --prefix feature/lesson12 | sed -n -e '/^would-archive/p' -e '/^Summary:/p' | sed "s#$DEMO_ROOT#\$DEMO_ROOT#g" | tr -s ' '
```

```expected
would-archive feature/lesson12-sandbox $DEMO_ROOT/state/worktrees/repo/lesson12-sandbox
Summary: 1 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty, 0 live-session, 0 processes, 0 salvage-failed, 0 needs-confirmation, 0 app-owned, 0 permanent), 0 failed [dry-run: no changes made; pass --apply to execute]
```

## 5. Create a disposable claim

```run
fno claim acquire recording:lesson12 --holder recording-cast --ttl 15m --json | jq -c '{key,holder}'
```

```expected
{"key":"recording:lesson12","holder":"recording-cast"}
```

## 6. List and inspect that claim

```run
fno claim list --prefix recording: --json | jq -c 'map({key,state,holder})'
fno claim status recording:lesson12 --json | jq -c '{key,state,holder}'
```

```expected
[{"key":"recording:lesson12","state":"live","holder":"recording-cast"}]
{"key":"recording:lesson12","state":"live","holder":"recording-cast"}
```

## 7. Archive the worktree and release the claim

```run
fno worktree archive "$DEMO_WT"
fno claim release recording:lesson12 --holder recording-cast
fno claim status recording:lesson12 --json | jq -c '{key,state}'
```

```expected
Archived: directory removed, branch feature/lesson12-sandbox preserved in git
released: recording:lesson12
{"key":"recording:lesson12","state":"free"}
```

## Cut list

- Keep the policy, ensured path, and status row uncut.
- Keep the cleanup preview visible long enough to read the dry-run marker.
- Keep the live claim list and single-claim status together.
- Keep the archive receipt, release receipt, and final free state uncut.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L12-worktrees.cast
asciinema upload L12-worktrees.cast
```

[capture-at-record]
