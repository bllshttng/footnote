# L13: Spawn a peer agent

**Medium:** Narrated screen video

**The one thing:** A peer has its own live session on the same repository, and you can observe, enter, message, and stop it by name.

## Setup state

Run the shared setup in [README.md](README.md). If a previous rehearsal left `demo-reviewer` registered, remove that demo worker before the take.

## 1. Spawn the peer

```run
fno agents spawn "Inspect the current diff and report one risk" --name demo-reviewer --harness codex
```

[capture-at-record]

Narration: "Spawn creates a persistent peer and returns its real receipt. The harness flag chooses the CLI binary, not the model vendor."

## 2. Observe without interrupting

```run
fno agents list
fno agents peek demo-reviewer --lines 12
```

[capture-at-record]

Narration: "List proves the peer joined the roster. Peek reads its transcript without sending a prompt or changing the worker's task."

## 3. Enter the live session

```run
fno agents attach demo-reviewer
```

[capture-at-record]

Narration: "Attach enters the existing session instead of starting another agent. Exit the attached view after the worker's current turn completes."

## 4. Ask a follow-up

```run
fno agents ask demo-reviewer "Which file supports that risk?"
```

[capture-at-record]

Narration: "Ask sends a message to the named peer and prints its reply. Creation and follow-up are separate verbs, so a typo cannot silently launch a new worker."

## 5. Stop the peer

```run
fno agents stop demo-reviewer
```

[capture-at-record]

Narration: "Stop ends the underlying session by name. The final roster read must show that the worker is no longer live."

## Cut list

- Keep the spawn receipt and first live roster row uncut.
- Compress the peer's analysis, but keep its first concrete risk at normal speed.
- Keep the attach transition and follow-up reply uncut.
- Keep the stop receipt and final status visible together.

## Publish

Export the final file as `L13-spawn-a-peer-agent.mp4`, then upload it to the tutorial release.

```run
gh release upload tutorials-v1 L13-spawn-a-peer-agent.mp4 --clobber
gh release view tutorials-v1 --json assets --jq '.assets[] | select(.name == "L13-spawn-a-peer-agent.mp4") | .url'
```

[capture-at-record]
