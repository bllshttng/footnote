#!/usr/bin/env bash
set -euo pipefail

# Test dynamic parallelization: file ownership map parsing + set intersection
# Tests the algorithm described in skills/do/references/dynamic-parallelization.md

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

PASS=0
FAIL=0

assert_verdict() {
  local test_name="$1"
  local expected="$2"
  local actual="$3"
  if [[ "$actual" == "$expected" ]]; then
    echo "  PASS: $test_name (got $actual)"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $test_name (expected $expected, got $actual)"
    FAIL=$((FAIL + 1))
  fi
}

# --- Parse file ownership map and check set intersection ---
# This implements the algorithm from dynamic-parallelization.md in bash
# so we can validate the logic independently.

parse_ownership_map() {
  local index_file="$1"
  # Extract lines from File Ownership Map table, output: task_id|file_path
  awk '
    /^## File Ownership Map/ { in_map=1; next }
    in_map && /^\|[- ]+\|/ { next }  # skip separator rows
    in_map && /^\| File/ { next }     # skip header
    in_map && /^\|/ {
      # Parse: | `file` | task | action |
      split($0, cols, "|")
      file = cols[2]; gsub(/^[ `]+|[ `]+$/, "", file)
      task = cols[3]; gsub(/^[ ]+|[ ]+$/, "", task)
      if (file != "" && task != "") {
        # Handle comma-separated tasks
        n = split(task, tasks, ",")
        for (i = 1; i <= n; i++) {
          t = tasks[i]; gsub(/^[ ]+|[ ]+$/, "", t)
          print t "|" file
        }
      }
      next
    }
    in_map && /^#/ { exit }  # next section
    in_map && /^$/ { next }  # blank line
  ' "$index_file"
}

check_wave_disjoint() {
  local index_file="$1"
  shift
  local tasks=("$@")

  # Parse ownership map into associative arrays
  local map_output
  map_output="$(parse_ownership_map "$index_file")"

  if [[ -z "$map_output" ]]; then
    echo "SKIP"
    return
  fi

  # Check all tasks exist in map (use awk for exact field match, not grep regex)
  for task in "${tasks[@]}"; do
    if ! printf "%s\n" "$map_output" | awk -F'|' -v t="$task" '$1 == t { found=1; exit } END { exit !found }'; then
      echo "SEQUENTIAL"
      return
    fi
  done

  # Check pairwise disjointness (use awk for exact field match)
  for ((i=0; i<${#tasks[@]}; i++)); do
    local files_i
    files_i="$(printf "%s\n" "$map_output" | awk -F'|' -v t="${tasks[$i]}" '$1 == t { print $2 }' | sort)"
    for ((j=i+1; j<${#tasks[@]}; j++)); do
      local files_j
      files_j="$(printf "%s\n" "$map_output" | awk -F'|' -v t="${tasks[$j]}" '$1 == t { print $2 }' | sort)"
      local overlap
      overlap="$(comm -12 <(echo "$files_i") <(echo "$files_j"))"
      if [[ -n "$overlap" ]]; then
        echo "SEQUENTIAL"
        return
      fi
    done
  done

  echo "PARALLEL"
}

# ===== Test 1: Disjoint files -> parallel eligible =====
echo "Test 1: Disjoint files"
cat > "$TMP_DIR/test1-index.md" <<'EOF'
## File Ownership Map

| File | Phase | Action |
|------|-------|--------|
| `src/auth.ts` | 1.1 | Modify |
| `src/billing.ts` | 1.2 | Create |
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test1-index.md" "1.1" "1.2")"
assert_verdict "Disjoint files -> parallel" "PARALLEL" "$VERDICT"

# ===== Test 2: Overlapping files -> sequential =====
echo "Test 2: Overlapping files"
cat > "$TMP_DIR/test2-index.md" <<'EOF'
## File Ownership Map

| File | Phase | Action |
|------|-------|--------|
| `src/routes.ts` | 1.1 | Modify |
| `src/routes.ts` | 1.2 | Modify |
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test2-index.md" "1.1" "1.2")"
assert_verdict "Overlapping files -> sequential" "SEQUENTIAL" "$VERDICT"

# ===== Test 3: Partial overlap (3 tasks) -> sequential =====
echo "Test 3: Partial overlap with 3 tasks"
cat > "$TMP_DIR/test3-index.md" <<'EOF'
## File Ownership Map

| File | Phase | Action |
|------|-------|--------|
| `src/auth.ts` | 1.1 | Modify |
| `src/billing.ts` | 1.2 | Create |
| `src/auth.ts` | 1.3 | Modify |
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test3-index.md" "1.1" "1.2" "1.3")"
assert_verdict "Partial overlap (C overlaps A) -> sequential" "SEQUENTIAL" "$VERDICT"

# ===== Test 4: Missing task in map -> sequential (conservative) =====
echo "Test 4: Missing task in map"
cat > "$TMP_DIR/test4-index.md" <<'EOF'
## File Ownership Map

| File | Phase | Action |
|------|-------|--------|
| `src/auth.ts` | 1.1 | Modify |
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test4-index.md" "1.1" "1.2")"
assert_verdict "Missing task -> sequential (conservative)" "SEQUENTIAL" "$VERDICT"

# ===== Test 5: No file ownership map -> skip =====
echo "Test 5: No file ownership map"
cat > "$TMP_DIR/test5-index.md" <<'EOF'
## Execution Strategy
```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    tasks: [1.1, 1.2]
```
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test5-index.md" "1.1" "1.2")"
assert_verdict "No map -> skip" "SKIP" "$VERDICT"

# ===== Test 6: Already parallel wave -> no change (algorithm only checks sequential) =====
echo "Test 6: Disjoint files in already-parallel wave"
# The algorithm only upgrades sequential to parallel. Already-parallel waves are
# left as-is. This test verifies the disjoint check still returns PARALLEL
# (the caller is responsible for not downgrading).
cat > "$TMP_DIR/test6-index.md" <<'EOF'
## File Ownership Map

| File | Phase | Action |
|------|-------|--------|
| `src/auth.ts` | 2.1 | Modify |
| `src/billing.ts` | 2.2 | Create |
| `src/notifications.ts` | 2.3 | Create |
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test6-index.md" "2.1" "2.2" "2.3")"
assert_verdict "Already-parallel wave stays parallel" "PARALLEL" "$VERDICT"

# ===== Test 7: Comma-separated tasks in map =====
echo "Test 7: Comma-separated task assignments"
cat > "$TMP_DIR/test7-index.md" <<'EOF'
## File Ownership Map

| File | Phase | Action |
|------|-------|--------|
| `src/shared.ts` | 1.1, 1.2 | Modify |
| `src/auth.ts` | 1.1 | Modify |
| `src/billing.ts` | 1.2 | Create |
EOF

VERDICT="$(check_wave_disjoint "$TMP_DIR/test7-index.md" "1.1" "1.2")"
assert_verdict "Comma-separated tasks create overlap -> sequential" "SEQUENTIAL" "$VERDICT"

# ===== Summary =====
echo ""
echo "Results: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
echo "All dynamic parallelization tests passed"
