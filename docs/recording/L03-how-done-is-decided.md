# L03: How done is decided

**Medium:** Narrated screen video

**The one thing:** The external checks must settle before a target run stops. The viewer can name the exact fact that keeps a run alive.

## Setup state

Run the shared setup in [README.md](README.md), then enter the target worktree created in L02. Keep public merged PR 825 available as the settled comparison and set `PR_NUMBER` to the current demo PR before beat 3.

## 1. Read the immutable inputs

```run
set -o pipefail
fno state show | sed -n -e '/^attended:/p' -e '/^auto_merge_approved:/p' -e '/^has_ui:/p' -e '/^no_external:/p'
```

```expected
attended: true
auto_merge_approved: false
has_ui: false
no_external: false
```

Narration: "The manifest records how this run started. It does not contain a writable done flag, so the agent cannot declare itself finished by editing state."

## 2. Read a settled pull request

```run
set -o pipefail
fno pr status 825 | jq -c '{pr, verdict, settled, green, pr_state, checks, ready, ready_blockers}'
```

```expected
{"pr":"825","verdict":"green","settled":true,"green":true,"pr_state":"MERGED","checks":{"total":14,"pass":14,"fail":0,"pending":0,"unsettled":0},"ready":true,"ready_blockers":[]}
```

Narration: "This control case is settled and green. The positive settled marker proves the status read completed instead of treating a missing response as success."

## 3. Read the live pull request

```run
fno pr status "$PR_NUMBER"
```

[capture-at-record]

Narration: "The live verdict names pending checks, failures, or review blockers. One unresolved fact is enough to keep the target loop running."

## 4. Watch the decision verb

```run
fno-agents loop-check --state .fno/target-state.md --transcript "$TRANSCRIPT_PATH" --cwd "$PWD"
```

[capture-at-record]

Narration: "The stop hook calls this decision verb with the live manifest and transcript. It reads the pull request and configured review evidence, then returns allow or block."

## Cut list

- Keep the four manifest lines and the entire settled JSON verdict on screen.
- Cut the live status wait after its first complete verdict.
- Keep the final allow-or-block line from loop-check uncut.
- Blur no values. If a real state path or session identifier appears, restart the take.

## Publish

Export the final file as `L03-how-done-is-decided.mp4`, then upload it to the tutorial release.

```run
gh release upload tutorials-v1 L03-how-done-is-decided.mp4 --clobber
gh release view tutorials-v1 --json assets --jq '.assets[] | select(.name == "L03-how-done-is-decided.mp4") | .url'
```

[capture-at-record]
