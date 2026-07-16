#!/usr/bin/env bash
# Pre-removal audit gate for the skill consolidation pass.
#
# Rejects PRs that have stale references in production code paths to skills
# being cut, demoted, merged, or renamed. Cuts and demotions delete skill directories;
# merges relocate content from skills/target-preflight and skills/target-postmortem
# into skills/target/references/. If a hook still sources a removed script or a
# skill body still references a removed sub-skill, the autonomous loop breaks
# silently. This gate catches that class of failure before it lands on main.
#
# Allowlisted scopes: design docs, plan folders, reason docs, the memory log,
# and CHANGELOG. Those are prose / historical record, not active code.
#
# Skill names that are substrings of other tokens are matched as full path
# segments only (e.g. /distill/ or /distill, never inside a longer identifier).

set -euo pipefail

CUT_SKILLS=(distill megaspec tower-play tower-watch copy-this)
DEMOTED_SKILLS=(token-doctor codemap git-worktrees)
MERGED_SKILLS=(target-preflight target-postmortem)
ALL_RETIRED=("${CUT_SKILLS[@]}" "${DEMOTED_SKILLS[@]}" "${MERGED_SKILLS[@]}")

# Production code paths. Anything outside these directories is treated as
# allowlisted by default (design docs, prose, top-level READMEs, etc.).
SCAN_PATHS=(
  hooks
  scripts
  agents
  skills
  .claude-plugin
  cli/src/fno
)

# Explicit allowlist for paths inside the scan roots that contain prose
# references rather than active code. The allowlist is matched as a prefix
# on the file's path relative to the repo root.
ALLOWLIST_PATHS=(
  internal/fno/plans/2026-05-14-skills-consolidation
  internal/fno/plans/2026-05-14-skills-consolidation-impl
  internal/fno/reason/
  docs/CHANGELOG.md
  scripts/ci/check-no-stale-skill-refs.sh
  cli/tests/integration/test_consolidation_audit.py
  # CLI wrappers for demoted skills intentionally reference the old skill
  # names in docstrings and at their canonical scripts/<name>/ source paths;
  # they are the NEW pattern, not stale callers of the OLD pattern.
  cli/src/fno/codemap_cli/
  cli/src/fno/tokens/
  cli/src/fno/worktree_cli/
  scripts/codemap/
  scripts/diagnostics/
  # Historical LOC-ratchet ledger: records past PRs verbatim, including the
  # retired `fno inbox send` / `fno agents send` verbs in their reason text.
  scripts/ci/loc-ratchet-trajectory.yaml
)

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

# Validate skill names contain no regex metacharacters before building the
# pattern. A malformed entry should error loudly, not silently match
# everything.
for skill in "${ALL_RETIRED[@]}"; do
  if [[ "$skill" =~ [^a-zA-Z0-9_-] ]]; then
    echo "AUDIT ERROR: malformed skill name '$skill' (only [A-Za-z0-9_-] allowed)" >&2
    exit 2
  fi
done

is_allowlisted() {
  local file="$1"
  for prefix in "${ALLOWLIST_PATHS[@]}"; do
    if [[ "$file" == "$prefix"* ]]; then
      return 0
    fi
  done
  return 1
}

fail=0
declare -a FAILURES

for skill in "${ALL_RETIRED[@]}"; do
  # A "stale reference" is one of:
  #   - removed skill directory: skills/<name>/<anything>
  #   - slash command: /<name> followed by whitespace, paren, end of line
  #   - Skill() invocation: Skill("<name>") or Skill('<name>')
  #
  # Intentionally NOT matched:
  #   - .fno/<name>.md style artifact paths - <name>.<ext> indicates the
  #     output file, which keeps the same name even after a skill is demoted
  #     to a CLI verb. The `.` after the name is the discriminator: a slash
  #     command never has a file extension immediately after.
  #   - bare-word usages in stage lists (e.g. impeccable's `distill, extract`)
  #   - `abilities:<name>` alias form (false-positives on slash-command matchers
  #     that route to commands/<name>.md for a cut skill whose command
  #     surface is intentionally preserved)
  #   - bare quoted strings "<name>" without `Skill(` context - false-positives
  #     on Path() builders and Typer subcommand name= registrations
  #   - canonical scripts/<name>/ paths after demotion (allowlisted explicitly)
  pattern="(skills/${skill}/)|(/${skill}([^A-Za-z0-9_.-]|$))|(Skill\([\"']${skill}[\"']\))"

  # Run grep over the scan paths. Use -I to skip binaries, -r recursive,
  # -E for extended regex, -n for line numbers. Distinguish:
  #   rc=0: at least one match found (real stale ref)
  #   rc=1: no matches (clean - skip this skill)
  #   rc>=2: real grep error (malformed regex, unreadable file, missing
  #          scan path). With `set -euo pipefail` and a blanket `|| true`
  #          this would silently produce AUDIT PASS - a green CI that
  #          proved nothing. Fail loudly instead.
  set +e
  raw=$(grep -IrEn "$pattern" "${SCAN_PATHS[@]}" 2>/dev/null)
  rc=$?
  set -e
  if [[ $rc -gt 1 ]]; then
    echo "AUDIT ERROR: grep failed with rc=$rc on skill '$skill'" >&2
    echo "  pattern: $pattern" >&2
    echo "  scan_paths: ${SCAN_PATHS[*]}" >&2
    exit 2
  fi
  [[ -z "$raw" ]] && continue

  # Filter out allowlisted paths AND filter out skill_dir/* self-references
  # (the skill being removed still exists at this point in the audit cycle;
  # references inside its own directory are not stale, they will vanish
  # with the directory itself).
  filtered=""
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    file="${line%%:*}"
    # Self-reference filter: a path under skills/<skill>/ refers to itself
    if [[ "$file" == "skills/${skill}/"* ]]; then
      continue
    fi
    if is_allowlisted "$file"; then
      continue
    fi
    filtered+="$line"$'\n'
  done <<<"$raw"

  if [[ -n "$filtered" ]]; then
    FAILURES+=("$skill")
    echo "AUDIT FAIL: stale reference to '$skill' in:"
    echo "$filtered" | sed 's/^/  /'
    echo ""
    fail=1
  fi
done

# --- Renamed skills (old -> new) -------------------------------------------
# A folder rename (e.g. skills/agents -> skills/agent, ab-12dd2a5d) leaves the
# OLD name dead, but - unlike a cut - keeps a live sibling whose name is a
# prefix of unrelated tokens: the `fno agents` plural mesh CLI and the
# cli/src/fno/agents/ package path. The generic /<name> alternative used
# above would false-positive on those, so renamed skills are matched ONLY by
# their two unambiguous stale forms:
#   - old skill dir path:        skills/<old>/    (trailing slash is the boundary)
#   - old skill slash/alias name: abilities:<old>  (whole-token boundary)
# Both discriminate the retired skill from `fno agents` (space) and from the
# generic "(skills/agents)" dir-pair prose in docs (no trailing slash).
RENAMED_OLD_NAMES=(agents)
for old in "${RENAMED_OLD_NAMES[@]}"; do
  if [[ "$old" =~ [^a-zA-Z0-9_-] ]]; then
    echo "AUDIT ERROR: malformed renamed skill name '$old'" >&2
    exit 2
  fi
  pattern="(skills/${old}/)|(abilities:${old}([^A-Za-z0-9_-]|\$))"
  set +e
  raw=$(grep -IrEn "$pattern" "${SCAN_PATHS[@]}" 2>/dev/null)
  rc=$?
  set -e
  if [[ $rc -gt 1 ]]; then
    echo "AUDIT ERROR: grep failed with rc=$rc on renamed skill '$old'" >&2
    echo "  pattern: $pattern" >&2
    exit 2
  fi
  [[ -z "$raw" ]] && continue
  filtered=""
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    file="${line%%:*}"
    if is_allowlisted "$file"; then
      continue
    fi
    filtered+="$line"$'\n'
  done <<<"$raw"
  if [[ -n "$filtered" ]]; then
    FAILURES+=("$old (renamed)")
    echo "AUDIT FAIL: stale reference to renamed skill '$old' (now skills/agent / fno:agent) in:"
    echo "$filtered" | sed 's/^/  /'
    echo ""
    fail=1
  fi
done

# --- Retired command surfaces (ab-cee91152) --------------------------------
# Messaging consolidated into the `fno mail` namespace: the `fno inbox <verb>`
# surface and the `fno agents send` verb are deleted clean (no shim). A stale
# caller in production code would break at runtime, so reject any surviving
# invocation. The inbox pattern is verb-anchored so a documentation mention of
# the retired namespace that carries no verb does not false-positive;
# `fno agents send` is the exact removed command form.
CUT_SURFACE_PATTERNS=(
  'fno inbox (send|unread|ack|reply|list|drain|triage|status|lint|view|migrate-bus)'
  'fno agents send'
)
for pat in "${CUT_SURFACE_PATTERNS[@]}"; do
  set +e
  raw=$(grep -IrEn "$pat" "${SCAN_PATHS[@]}" 2>/dev/null)
  rc=$?
  set -e
  if [[ $rc -gt 1 ]]; then
    echo "AUDIT ERROR: grep failed with rc=$rc on cut-surface pattern '$pat'" >&2
    echo "  scan_paths: ${SCAN_PATHS[*]}" >&2
    exit 2
  fi
  [[ -z "$raw" ]] && continue
  filtered=""
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    file="${line%%:*}"
    if is_allowlisted "$file"; then
      continue
    fi
    filtered+="$line"$'\n'
  done <<<"$raw"
  if [[ -n "$filtered" ]]; then
    FAILURES+=("$pat")
    echo "AUDIT FAIL: stale reference to a retired messaging surface (now \`fno mail\`):"
    echo "$filtered" | sed 's/^/  /'
    echo ""
    fail=1
  fi
done

if [[ $fail -eq 0 ]]; then
  echo "AUDIT PASS: no stale references to cut/demoted/merged/renamed skills or retired surfaces."
else
  echo "AUDIT FAIL: ${#FAILURES[@]} stale-reference group(s): ${FAILURES[*]}"
  echo "Fix the listed file:line citations, then re-run."
fi

exit $fail
