# L10: Capture and shape work

**Medium:** Asciinema cast

**The one thing:** One backlog item can move from a sentence through discovery, prioritization, deferral, replacement, and completion without hand-editing the graph.

## Setup state

Run the shared setup in [README.md](README.md) against a fresh demo state. The `recording10` project name isolates selection from nodes created by other lessons.

## 1. Capture the idea

```run
set -o pipefail
fno backlog idea "Shape demo release checklist" --size S --project recording10 | tee "$DEMO_ROOT/l10-node.json" | jq -c '{title}'
DEMO_NODE="$(jq -r .id "$DEMO_ROOT/l10-node.json")"
```

```expected
{"title":"Shape demo release checklist"}
```

## 2. Read the node

```run
fno backlog get "$DEMO_NODE" | jq -c '{status,slug,title,priority,size,project}'
```

```expected
{"status":"idea","slug":"shape-demo-release-checklist","title":"Shape demo release checklist","priority":"p2","size":"S","project":"recording10"}
```

## 3. Add priority and detail

```run
fno backlog update "$DEMO_NODE" --priority p1 --details "Show one safe recovery path." >/dev/null
fno backlog get "$DEMO_NODE" | jq -c '{status,slug,priority,details}'
```

```expected
{"status":"idea","slug":"shape-demo-release-checklist","priority":"p1","details":"Show one safe recovery path."}
```

## 4. Find it by description

```run
fno backlog find "release checklist" --project recording10 --json | jq -c 'map({status,slug,title,priority,project})'
```

```expected
[{"status":"idea","slug":"shape-demo-release-checklist","title":"Shape demo release checklist","priority":"p1","project":"recording10"}]
```

## 5. Select the next idea

```run
fno backlog next --project recording10 --include-ideas | jq -c '{slug,title,priority,project}'
```

```expected
{"slug":"shape-demo-release-checklist","title":"Shape demo release checklist","priority":"p1","project":"recording10"}
```

## 6. List the same work pool

```run
fno backlog ready --project recording10 --include-ideas --json | jq -c 'map({slug,title,priority,project})'
```

```expected
[{"slug":"shape-demo-release-checklist","title":"Shape demo release checklist","priority":"p1","project":"recording10"}]
```

## 7. Defer it

```run
fno backlog defer "$DEMO_NODE" --reason "Pause the demo" >/dev/null
fno backlog get "$DEMO_NODE" | jq -c '{status,slug,deferred_reason}'
```

```expected
{"status":"deferred","slug":"shape-demo-release-checklist","deferred_reason":"Pause the demo"}
```

## 8. Return it to the active pool

```run
fno backlog undefer "$DEMO_NODE" >/dev/null
fno backlog get "$DEMO_NODE" | jq -c '{status,slug,deferred_reason}'
```

```expected
{"status":"idea","slug":"shape-demo-release-checklist","deferred_reason":null}
```

## 9. Replace the original scope

```run
set -o pipefail
fno backlog idea "Replace demo release checklist" --size S --project recording10 2>"$DEMO_ROOT/l10-dedup.txt" | tee "$DEMO_ROOT/l10-replacement.json" | jq -c '{title}'
REPLACEMENT_NODE="$(jq -r .id "$DEMO_ROOT/l10-replacement.json")"
fno backlog supersede "$REPLACEMENT_NODE" --replaces "$DEMO_NODE" --reason "Use the clearer scope" >/dev/null
fno backlog find "release checklist" --project recording10 --json | jq -c 'map({status,slug,title})'
```

```expected
{"title":"Replace demo release checklist"}
[{"status":"superseded","slug":"shape-demo-release-checklist","title":"Shape demo release checklist"},{"status":"idea","slug":"replace-demo-release-checklist","title":"Replace demo release checklist"}]
```

## 10. Close the disposable replacement

```run
set -o pipefail
fno backlog done "$REPLACEMENT_NODE" --force --reason "Disposable recording state" 2>&1 | sed -E 's/demo-[0-9a-f]{4,8}/demo-ID/g'
fno backlog get "$REPLACEMENT_NODE" | jq -c '{status,slug,title}'
```

```expected
Warning: force flag set on advisory node demo-ID (reason: Disposable recording state); no PR refs to check.
Marked demo-ID done
{"status":"done","slug":"replace-demo-release-checklist","title":"Replace demo release checklist"}
```

## 11. Render the board without opening a browser

```run
export FNO_NO_OPEN=1
fno backlog view | sed "s#$DEMO_ROOT#\$DEMO_ROOT#g"
```

```expected
$DEMO_ROOT/state/graph.html
```

## 12. Read the project summary

```run
fno backlog status --project recording10 | sed -n -e '/^Project:/p' -e '/^Progress:/p' -e '/^demo-/p' | sed -E 's/demo-[0-9a-f]{4,8}/demo-ID/g' | tr -s ' '
```

```expected
Project: recording10
Progress: 1/2 done | 0 claimed | 0 ready | 0 blocked
demo-ID Shape demo release checklist superseded p1 $0.00 -
demo-ID Replace demo release checkli done p2 $0.00 -
```

## Cut list

- Keep all twelve command names and their first result lines uncut.
- Cut JSON fields not selected by the script.
- Keep the defer, undefer, supersede, and done status transitions at normal speed.
- Keep the rendered board path and final two-row summary visible together.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L10-capture-and-shape-work.cast
asciinema upload L10-capture-and-shape-work.cast
```

[capture-at-record]
