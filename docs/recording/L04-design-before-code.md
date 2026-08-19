# L04: Design before code

**Medium:** Narrated screen video

**The one thing:** Think investigates primary sources and writes cited findings before planning or implementation begins.

## Setup state

Run the shared setup in [README.md](README.md). Keep the demo repository clean so every file created during the take belongs to this investigation.

## 1. Ask what is true

```run
/fno:think "What owns upload retries?"
```

[capture-at-record]

Narration: "The default brief asks what the repository and first-party sources prove. Think writes one cited findings file and does not change production code."

## 2. Ask how it breaks

```run
/fno:think what-if "How can upload retries fail?"
```

[capture-at-record]

Narration: "What-if follows the real error and concurrency paths. An imagined failure is not a finding until a primary source supports it."

## 3. Read the system through several lenses

```run
/fno:think panel "Should uploads retry automatically?"
```

[capture-at-record]

Narration: "Panel keeps the same evidence standard while several lenses inspect the code they own. Their claims still point back to files, lines, or first-party URLs."

## 4. Dispatch the investigation

```run
fno backlog idea "Explore upload failure modes" --size S
fno think dispatch explore-upload-failure-modes --json
```

[capture-at-record]

Narration: "Dispatch moves a backlog-backed investigation to a background worker and prints its receipt. The current session can continue without pretending the research already finished."

## Cut list

- Keep each prompt and the final findings path uncut.
- Compress source-reading pauses, but leave one cited claim from each mode at normal speed.
- Keep the dispatch receipt on screen long enough to read its worker identity and status.
- Cut any editor view that contains uncited scratch prose.

## Publish

Export the final file as `L04-design-before-code.mp4`, then upload it to the tutorial release.

```run
gh release upload tutorials-v1 L04-design-before-code.mp4 --clobber
gh release view tutorials-v1 --json assets --jq '.assets[] | select(.name == "L04-design-before-code.mp4") | .url'
```

[capture-at-record]
