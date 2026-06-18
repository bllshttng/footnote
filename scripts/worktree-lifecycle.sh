#!/usr/bin/env bash
# Worktree lifecycle management
# Usage:
#   worktree-lifecycle.sh status                    # List all worktrees
#   worktree-lifecycle.sh cleanup [--older-than Nd] [--dry-run] [--prefix <prefix>]
#   worktree-lifecycle.sh archive <name>            # Keep branch, remove directory
set -uo pipefail

case "${1:-status}" in
    status)
        echo "Worktrees:"
        git worktree list --porcelain 2>/dev/null | while IFS= read -r line; do
            case "$line" in
                "worktree "*)
                    WT_PATH="${line#worktree }"
                    ;;
                "branch "*)
                    BRANCH="${line#branch refs/heads/}"
                    # Check target status
                    TARGET=""
                    if [[ -f "$WT_PATH/.fno/target-state.md" ]]; then
                        TARGET=$(grep '^status:' "$WT_PATH/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
                    elif [[ -L "$WT_PATH/.fno" && -f "$WT_PATH/.fno/target-state.md" ]]; then
                        TARGET=$(grep '^status:' "$WT_PATH/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
                    fi
                    # Last commit age
                    LAST=$(cd "$WT_PATH" 2>/dev/null && git log -1 --format="%cr" 2>/dev/null || echo "unknown")
                    printf "  %-30s | %-15s | target: %-12s | %s\n" "$BRANCH" "$LAST" "${TARGET:-none}" "$WT_PATH"
                    ;;
            esac
        done
        ;;

    cleanup)
        shift
        DAYS=7
        DRY_RUN=""
        PREFIX=""
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --older-than) DAYS="${2%d}"; shift 2 ;;
                --dry-run) DRY_RUN="true"; shift ;;
                --prefix) PREFIX="$2"; shift 2 ;;
                *) shift ;;
            esac
        done

        MAIN_DIR=$(git rev-parse --show-toplevel 2>/dev/null)
        REMOVED=0

        while IFS= read -r wt; do
            # Skip main repo
            [[ "$wt" == "$MAIN_DIR" ]] && continue

            # Filter by prefix if specified
            if [[ -n "$PREFIX" ]]; then
                BRANCH=$(cd "$wt" 2>/dev/null && git branch --show-current || echo "")
                [[ "$BRANCH" != ${PREFIX}* ]] && continue
            fi

            # Check age
            LAST_COMMIT=$(cd "$wt" 2>/dev/null && git log -1 --format="%ct" 2>/dev/null || echo 0)
            NOW=$(date +%s)
            AGE_DAYS=$(( (NOW - LAST_COMMIT) / 86400 ))

            if [[ $AGE_DAYS -ge $DAYS ]]; then
                # Check target
                STATUS=$(grep '^status:' "$wt/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
                if [[ "$STATUS" == "IN_PROGRESS" ]]; then
                    echo "  SKIP: $wt (active target session)"
                    continue
                fi

                BRANCH=$(cd "$wt" 2>/dev/null && git branch --show-current || echo "unknown")
                if [[ -n "$DRY_RUN" ]]; then
                    echo "  WOULD REMOVE: $wt ($AGE_DAYS days old, branch: $BRANCH)"
                else
                    git worktree remove --force "$wt" 2>/dev/null
                    echo "  REMOVED: $wt (branch $BRANCH preserved)"
                    REMOVED=$((REMOVED + 1))
                fi
            fi
        done < <(git worktree list --porcelain 2>/dev/null | grep "^worktree " | sed 's/^worktree //')

        [[ -z "$DRY_RUN" ]] && echo "Cleanup complete. Removed $REMOVED worktree(s)."
        ;;

    archive)
        NAME="${2:-}"
        if [[ -z "$NAME" ]]; then
            echo "Usage: worktree-lifecycle.sh archive <worktree-name>"
            exit 1
        fi

        WT=".claude/worktrees/$NAME"
        if [[ -d "$WT" ]]; then
            BRANCH=$(cd "$WT" && git branch --show-current)
            git worktree remove --force "$WT" 2>/dev/null
            echo "Archived: directory removed, branch $BRANCH preserved in git"
        else
            echo "Worktree not found: $WT"
            exit 1
        fi
        ;;

    *)
        echo "Usage: worktree-lifecycle.sh {status|cleanup|archive} [args]"
        exit 1
        ;;
esac
