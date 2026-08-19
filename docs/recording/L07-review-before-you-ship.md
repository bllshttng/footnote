# L07: Review before you ship

**Medium:** Narrated screen video

**The one thing:** Sigma is a broad internal panel, while peer is one cross-model opinion. Use the review depth that matches the diff's risk.

## Setup state

Run the shared setup in [README.md](README.md), then create one small committed change on a feature branch. The same diff must remain checked out for every beat.

## 1. Pin the diff under review

```run
git status --short
git diff --stat origin/main...HEAD
```

[capture-at-record]

Narration: "Both review modes read the current checkout. We show the exact diff first so every later finding has a visible object."

## 2. Run the sigma panel

```run
/fno:review sigma
```

[capture-at-record]

Narration: "Sigma sends the diff to six specialist roles and aggregates their findings. It earns its cost on large, security-sensitive, or hard-to-reason-about changes."

## 3. Ask one other model

```run
/fno:review peer codex
```

[capture-at-record]

Narration: "Peer asks one different model for a second opinion. It is cheaper and advisory by default, which fits a smaller or routine diff."

## 4. Read the head the panel reviewed

```run
fno review --sigma-last-head
git rev-parse HEAD
```

[capture-at-record]

Narration: "A review only covers the commit it inspected. If the two heads differ after a fix, the earlier verdict is stale and the final head needs review again."

## Cut list

- Keep the diff summary and both review commands uncut.
- Compress reviewer execution, but keep each reviewer name and every surviving finding visible.
- Keep the aggregate sigma verdict and peer verdict at normal speed.
- Keep both head values uncut so head-pinning is visible rather than asserted.

## Publish

Export the final file as `L07-review-before-you-ship.mp4`, then upload it to the tutorial release.

```run
gh release upload tutorials-v1 L07-review-before-you-ship.mp4 --clobber
gh release view tutorials-v1 --json assets --jq '.assets[] | select(.name == "L07-review-before-you-ship.mp4") | .url'
```

[capture-at-record]
