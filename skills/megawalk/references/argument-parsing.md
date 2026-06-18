# Argument Parsing

**Load when:** dispatching ANY megawalk invocation. The parser runs before any subcommand routes and is responsible for graph-ID resolution and the removed-form redirects.

Before dispatching to any subcommand, apply this normalization:

1. **Empty ARGUMENTS:** enter the loop and pick the top ready task. Bare `/megawalk` is the canonical entry point (no subcommand needed).

2. **Graph-ID:** if the first positional matches `^ab-[0-9a-f]{8}$`, branch on whether the node is an **epic** (has transitive children) or a **leaf** (C2, ab-facfaade):

   ```bash
   source "${SKILL_DIR}/scripts/lib/graph-resolve.sh"
   # Epic detection (single invocation): `ready --parent <id>` exits 0 for
   # any real node and prints "no children under <id>" to stderr iff the
   # node has no children at all. Merge stderr (2>&1) so one call yields
   # both the exit code and the message. exit 0 AND no "no children" => epic;
   # a missing node (non-zero exit) or a leaf falls through to single-node
   # execution, preserving resolve_arg's typo-warning path.
   if OUT=$(fno backlog ready --parent "$FIRST_POSITIONAL" --all 2>&1) \
      && ! printf '%s' "$OUT" | grep -q "no children under"; then
       # Epic: drain the subtree feature-by-feature, then fall back to the
       # broader queue. This IS the loop entry; do not also call /target.
       fno megawalk --epic "$FIRST_POSITIONAL"
   else
       # Leaf node (or unknown ID): execute this specific node.
       RESOLVED=$(resolve_arg "$FIRST_POSITIONAL")
       # ... dispatch to /target M "$RESOLVED"
   fi
   ```

   `resolve_arg` echoes unchanged args back with a stderr warning on unknown IDs, so the downstream "plan path does not exist" error fires cleanly for typos.

3. **Removed-form redirects (hard fail, exit 1):**

   ```bash
   # `continue` was removed; bare /megawalk replaces it. Anchor on the first
   # positional so `continue` inside a quoted `--title "Don't continue"`
   # or similar doesn't spuriously trigger.
   if [[ "$FIRST_POSITIONAL" == "continue" ]]; then
       echo "Error: 'continue' was removed. Use bare /megawalk - it enters the loop from campaign state. Modifiers still work without a subcommand (e.g. /megawalk parallel, /megawalk auto-merge)." >&2
       exit 1
   fi

   # `next` was removed; bare /megawalk enters the loop directly. The alias
   # added surface area without functional value. Anchor on first positional.
   if [[ "$FIRST_POSITIONAL" == "next" ]]; then
       echo "Error: 'next' was removed. Use bare /megawalk - it enters the loop directly. Modifiers still work without a subcommand (e.g. /megawalk parallel, /megawalk auto-merge)." >&2
       exit 1
   fi

   # `adopt --batch` was removed; multi-path + glob is the same result.
   # Require the subcommand to actually be `adopt` AND `--batch` to appear
   # as a distinct token, so `--title "--batch foo"` can't match.
   if [[ "$FIRST_POSITIONAL" == "adopt" ]] && [[ " $ARGUMENTS " == *" --batch "* ]]; then
       echo "Error: 'adopt --batch' was removed. Use multi-path (adopt a b c) or shell glob (adopt plans/*.md)." >&2
       exit 1
   fi

   # Top-level `vision.md` moved under the roadmap subcommand. Detect the
   # first positional being an existing .md file AND not a subcommand that
   # legitimately takes a path (adopt, roadmap).
   if [[ "$FIRST_POSITIONAL" != "adopt" ]] && [[ "$FIRST_POSITIONAL" != "roadmap" ]] \
      && [[ "$FIRST_POSITIONAL" == *.md ]] && [[ -f "$FIRST_POSITIONAL" ]]; then
       echo "Error: vision docs now go under '/megawalk roadmap <path>'. Run: /megawalk roadmap $FIRST_POSITIONAL" >&2
       exit 1
   fi
   ```

4. Otherwise, dispatch to the subcommand named by the first positional (`roadmap`, `adopt`, `status`, `defer`, `cancel`, `retro`). If no subcommand matches (e.g. a bare modifier like `parallel`, `once`, `council`, `auto-merge`, `auto-continue`), treat the invocation as bare and enter the loop with the modifier applied.

   The `auto-continue` modifier is special: it does not map to an `fno-agents loop run` flag (the walk still dies on `NoWork`; the merge event drives the next dispatch). Before entering the loop it ARMS the campaign by writing the per-project marker `mkdir -p .fno && touch .fno/.auto-continue-armed` and echoing the arming (AC2-UI), so `fno backlog advance` (fired later by reconcile / post-merge) dispatches the next now-unblocked node after each merge. See [SKILL.md](../SKILL.md) "Explicit flags (ac-compat)".

## Why anchor on FIRST_POSITIONAL

Substring-matching `continue`, `next`, or `--batch` against the entire `$ARGUMENTS` string would false-trigger on quoted `--title` values, commit messages, or PR descriptions that happen to contain those words. The first-positional anchor is the difference between a parser and a substring scan.

The vision-doc redirect uses both "first positional matches *.md" AND "is an existing file" so a path-shaped string that doesn't exist on disk falls through to the normal "plan does not exist" error path rather than redirecting to roadmap. This keeps typos noisy.

See [megawalk-migration.md](megawalk-migration.md) for the migration history of these removed forms.
