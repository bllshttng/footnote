# Size Routing

Automatically determines the target size flag (-S/-M/-L) for each roadmap task based on task attributes. This replaces the v1 behavior where every task got the same ceremony level.

## Routing Algorithm

```
1. Check task metadata for explicit size: field
   If present: use it. Done.

2. Check if plan exists (task.plan_path is set):
   Count phase files in plan directory (NN-*.md, excluding 00-INDEX.md)
   phase_count = number of phase files

3. Compute base size from attributes:

   If phase_count exists:
     phase_count == 1         -> S
     phase_count in [2, 3]    -> M
     phase_count >= 4         -> L

   Else if estimated_points exists:
     points in [1, 3]         -> S
     points in [4, 8]         -> M
     points >= 9              -> L

   Else:
     Default to M (safe middle ground)

4. Apply domain modifier (see below)

5. Log: "Task {id} routed to -{size} (reason: {attribute}={value})"
```

## Domain Modifiers

Some domains are inherently heavier or lighter:

| Domain | Modifier | Reason |
|--------|----------|--------|
| `infrastructure` | +1 size | Infra mistakes are expensive to fix |
| `security` | +1 size | Security needs adversarial review |
| `migration` | +1 size | Migrations need extra verification |
| `ui` | no change | Standard routing |
| `code` | no change | Standard routing |
| `docs` | -1 size | Docs rarely need full ceremony |

+1 means S becomes M, M becomes L, L stays L.
-1 means L becomes M, M becomes S, S stays S.

## Priority Order

1. Explicit `size:` in task metadata (always wins)
2. Phase count from existing plan (if plan_path exists)
3. Estimated points from roadmap-generator
4. Default M

Phase count takes precedence over points because it's a more concrete signal. A 2-point task with a 4-phase plan is legitimately complex.

## Integration with Target

The size flag is passed as an argument to target:

```bash
/target -{size} {plan_path}
```

Target's size profile (from size-profiles.md) handles all the toggle resolution. Expedition doesn't need to know what -S/-M/-L means internally.

## Size Routing Function

For use in tests and the skill:

```bash
# route_size(estimated_points, domain, plan_path)
# Returns: S, M, or L
route_size() {
  local points="${1:-0}" domain="${2:-code}" plan_path="${3:-}"
  local base="M"

  # 1. Check plan phase count
  if [[ -n "$plan_path" && -d "$plan_path" ]]; then
    local phase_count
    phase_count=$(ls "$plan_path"/[0-9][0-9]-*.md 2>/dev/null | grep -cv '00-INDEX.md' || echo 0)
    if (( phase_count >= 4 )); then base="L"
    elif (( phase_count >= 2 )); then base="M"
    elif (( phase_count >= 1 )); then base="S"
    else base="M"  # no phase files, fall through to points
    fi
  else
    # 2. Use estimated points
    if (( points >= 9 )); then base="L"
    elif (( points >= 4 )); then base="M"
    elif (( points >= 1 )); then base="S"
    else base="M"  # no points, default
    fi
  fi

  # 3. Apply domain modifier
  case "$domain" in
    infrastructure|security|migration) base=$(size_up "$base") ;;
    docs) base=$(size_down "$base") ;;
  esac

  echo "$base"
}

size_up() {
  case "$1" in S) echo "M" ;; M) echo "L" ;; *) echo "L" ;; esac
}

size_down() {
  case "$1" in L) echo "M" ;; M) echo "S" ;; *) echo "S" ;; esac
}
```
