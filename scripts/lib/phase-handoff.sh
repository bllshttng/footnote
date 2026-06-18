#!/usr/bin/env bash
# phase-handoff.sh - Shell library for target phase handoff artifacts
#
# Usage: source this file from any target phase script, then call:
#   ph_write <phase> <session_id> <yaml_payload>
#   ph_read <phase> <session_id>
#   ph_read_latest <phase>
#   ph_list <session_id>
#
# Artifacts are stored at: .fno/artifacts/handoff/{phase}-{session_id}.md
# The handoff/ subdirectory namespaces away from gate-attestation artifacts.

# Size cap: roughly 500 tokens. 2000 chars is a conservative proxy.
PH_MAX_CHARS=2000

# Resolve artifact directory relative to cwd (where target runs)
_ph_dir() {
  echo ".fno/artifacts/handoff"
}

_ph_path() {
  local phase="$1"
  local session_id="$2"
  echo "$(_ph_dir)/${phase}-${session_id}.md"
}

# ph_write <phase> <session_id> <yaml_payload>
#
# Writes a handoff artifact atomically (tmp + rename).
# Enforces the 2000-char size cap; truncates if over budget.
# Refuses to overwrite an existing artifact for the same phase+session.
# Returns 0 on success, non-zero on error.
# Prints one-line status to stderr so stdout stays clean for target signals.
ph_write() {
  local phase="$1"
  local session_id="$2"
  local payload="$3"

  local artifact_dir
  artifact_dir="$(_ph_dir)"
  local artifact_path
  artifact_path="$(_ph_path "$phase" "$session_id")"
  local tmp_path="${artifact_path}.tmp"

  # Refuse overwrite
  if [[ -f "$artifact_path" ]]; then
    echo "ph_write: WARN: artifact already exists for ${phase}/${session_id} - refusing overwrite (phase ran twice?)" >&2
    return 1
  fi

  # Ensure directory exists
  mkdir -p "$artifact_dir"

  # Trap to remove the tmp file if the process is killed before mv completes.
  # After successful mv the trap is cleared so it doesn't act on the moved file.
  trap 'rm -f "$tmp_path"' EXIT INT TERM

  # Build full content: frontmatter wrapper + payload
  local timestamp
  # Fallback to local time without -u if `date -u` is unavailable on this platform.
  timestamp="$(date -u +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || date +"%Y-%m-%dT%H:%M:%SZ")"

  local header
  header="---
phase: ${phase}
session_id: ${session_id}
timestamp: ${timestamp}
${payload}
---"

  # Enforce size cap
  local char_count
  char_count="${#header}"
  if [[ "$char_count" -gt "$PH_MAX_CHARS" ]]; then
    # Truncate payload to fit within cap, leaving room for header skeleton and marker
    # Skeleton overhead: ~100 chars for phase/session_id/timestamp lines + markers
    local skeleton_overhead=100
    local marker="# truncated at approx 500 tokens - context budget exceeded"
    local marker_len="${#marker}"
    local allowed=$(( PH_MAX_CHARS - skeleton_overhead - marker_len ))
    if [[ "$allowed" -lt 0 ]]; then
      allowed=0
    fi
    local truncated_payload="${payload:0:$allowed}"
    header="---
phase: ${phase}
session_id: ${session_id}
timestamp: ${timestamp}
${truncated_payload}
---
${marker}"
  fi

  # Atomic write: tmp then rename
  printf '%s\n' "$header" > "$tmp_path" || {
    echo "ph_write: ERROR: failed to write tmp file at $tmp_path" >&2
    return 1
  }
  mv "$tmp_path" "$artifact_path" || {
    echo "ph_write: ERROR: failed to rename tmp to $artifact_path" >&2
    rm -f "$tmp_path"
    trap - EXIT INT TERM
    return 1
  }

  # Clear the trap now that tmp has been successfully renamed.
  trap - EXIT INT TERM

  echo "ph_write: wrote ${phase} artifact for session ${session_id}" >&2
  return 0
}

# ph_read <phase> <session_id>
#
# Reads the artifact frontmatter and emits it to stdout.
# Returns non-zero if artifact does not exist.
ph_read() {
  local phase="$1"
  local session_id="$2"
  local artifact_path
  artifact_path="$(_ph_path "$phase" "$session_id")"

  if [[ ! -f "$artifact_path" ]]; then
    echo "ph_read: no artifact for ${phase}/${session_id} - proceeding with reduced context" >&2
    return 1
  fi

  # Validate that both --- frontmatter markers are present before parsing.
  # Without both markers the awk extractor silently returns empty output,
  # causing callers to proceed with zero context.
  local marker_count
  marker_count=$(grep -c '^---$' "$artifact_path" 2>/dev/null || echo 0)
  if [[ "$marker_count" -lt 2 ]]; then
    echo "ph_read: warning: $artifact_path missing frontmatter markers ($marker_count found)" >&2
    return 1
  fi

  # Extract frontmatter block (between first pair of --- markers)
  # and emit as-is (YAML). Callers can pipe to `yq` or parse manually.
  awk '/^---/{found++; if(found==2){exit}; next} found==1{print}' "$artifact_path"
  return 0
}

# ph_read_latest <phase>
#
# Reads from the most recent artifact for the given phase (by mtime).
# Useful for cross-session resume where session_id may have changed.
ph_read_latest() {
  local phase="$1"
  local artifact_dir
  artifact_dir="$(_ph_dir)"

  if [[ ! -d "$artifact_dir" ]]; then
    echo "ph_read_latest: no artifact directory found" >&2
    return 1
  fi

  # Find the newest file matching phase-*.md.
  # NOTE: ls -t sorts by mtime which can be wrong if a newer ph_write was refused
  # (refuse-overwrite path) or if the filesystem has low mtime resolution. When
  # correctness is critical, call ph_read with an explicit session_id instead.
  local latest
  latest="$(ls -t "${artifact_dir}/${phase}"-*.md 2>/dev/null | head -1)"

  if [[ -z "$latest" ]]; then
    echo "ph_read_latest: no artifacts found for phase ${phase}" >&2
    return 1
  fi

  # Validate frontmatter markers before parsing (same guard as ph_read).
  local marker_count
  marker_count=$(grep -c '^---$' "$latest" 2>/dev/null || echo 0)
  if [[ "$marker_count" -lt 2 ]]; then
    echo "ph_read_latest: warning: $latest missing frontmatter markers ($marker_count found)" >&2
    return 1
  fi

  # Extract frontmatter
  awk '/^---/{found++; if(found==2){exit}; next} found==1{print}' "$latest"
  return 0
}

# ph_list <session_id>
#
# Lists all phase artifact filenames for a given session.
# Outputs one filename per line to stdout. Empty output if none found.
ph_list() {
  local session_id="$1"
  local artifact_dir
  artifact_dir="$(_ph_dir)"

  if [[ ! -d "$artifact_dir" ]]; then
    return 0
  fi

  # List files matching *-{session_id}.md, basename only
  find "$artifact_dir" -maxdepth 1 -name "*-${session_id}.md" -exec basename {} \; 2>/dev/null | sort
  return 0
}
