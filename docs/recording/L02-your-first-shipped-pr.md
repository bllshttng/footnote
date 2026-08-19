# L02: Your first shipped PR

**Medium:** Narrated screen video

**The one thing:** You give `/fno:target` one sentence, leave the agent to work, and return to a PR whose completion gate has checked the result.

## Setup state

Run the shared setup in [README.md](README.md) before this script. Keep the agent session and the terminal pointed at `/Users/Shared/footnote-recording-demo/repo`.

## 1. Prove the demo is isolated

```run
pwd
fno whoami
fno status
```

[capture-at-record]

Narration: "The demo has its own repository and Footnote state. These commands prove which project, agent, and run we are about to use."

## 2. State the feature in one sentence

```run
fno target start "Add a troubleshooting note for expired tokens"
```

[capture-at-record]

Narration: "That sentence is the brief. Target creates an isolated worktree, binds the run, and carries the work through planning, implementation, review, and pull request creation."

## 3. Leave while the run works

```run
fno target status
```

[capture-at-record]

Narration: "The useful part is the unattended middle. The terminal can close while the run keeps its worktree, claim, and finish line."

## 4. Come back to the pull request

```run
fno status
PR_NUMBER="$(gh pr view --json number --jq .number)"
gh pr view "$PR_NUMBER" --web
```

[capture-at-record]

Narration: "When we return, status points at the active run and GitHub opens the pull request it produced. We did not copy files or reconstruct agent state."

## 5. Read the gate that decided done

```run
fno target status
fno pr status "$PR_NUMBER"
```

[capture-at-record]

Narration: "Done is not a confident sentence from the agent. The gate reads the current PR, CI result, configured reviews, and local attestation before it lets the run finish."

## Cut list

- Keep beats 1 and 2 uncut so the viewer sees the exact input and isolated starting state.
- Compress the unattended middle to eight seconds, with the real elapsed time printed on screen.
- Keep one implementation action, the test result, and the PR creation receipt at normal speed.
- Keep beats 4 and 5 uncut so the return and completion proof remain credible.

## Publish

Export the final file as `L02-your-first-shipped-pr.mp4`, then upload it to the tutorial release.

```run
gh release upload tutorials-v1 L02-your-first-shipped-pr.mp4 --clobber
gh release view tutorials-v1 --json assets --jq '.assets[] | select(.name == "L02-your-first-shipped-pr.mp4") | .url'
```

[capture-at-record]

Current README sentence:

`Pre-launch (open-source readiness). Screencast coming. Built in the open and dogfooded daily: footnote ships footnote.`

Replacement sentence, with the asset URL as the only blank:

`Pre-launch (open-source readiness). [Watch the screencast](________). Built in the open and dogfooded daily: footnote ships footnote.`

Do not edit the public README until the asset URL resolves from a fresh clone.
