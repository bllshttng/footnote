#!/usr/bin/env bash
# Block known-hazardous constructs in fenced bash blocks under skills/ (x-f47f).
#
# Skill snippets are executed by the harness under the OPERATOR's shell against
# whatever grep is on PATH, and they are unrunnable by construction - no CI can
# execute a fenced block, so a bashism ships unnoticed until it misfires. Each
# class below has already shipped a bug that degraded into a plausible NO-OP
# rather than an error, which is why they are worth a bespoke gate rather than
# waiting for shellcheck (which does not model shell-to-shell divergence).
#
# Classes:
#   unquoted-conditional-expansion  ${VAR:+--flag "$VAR"} unquoted, body has
#                                   whitespace. bash word-splits it into two
#                                   argv entries; zsh does not, so the command
#                                   receives one joined argument and rejects it.
#   empty-grep-alternation          grep pattern with an empty alternative
#                                   ('null|'). GNU/BSD grep accept it; ugrep
#                                   errors, so the filter yields nothing and
#                                   reads as "no data".
#   bare-timeout                    `timeout N` with no gtimeout fallback.
#                                   macOS ships no timeout binary, so the
#                                   watcher silently never runs.
#   single-spelling-stat            stat -c (GNU) or stat -f (BSD) alone.
#   fno-mutation-swallowed          a mutating fno verb ending in `|| true`,
#                                   which swallows a real rejection.
#
# Escape hatch: `# lint-ok: <class>` on the offending line or the line above.
# Every use is visible in review.
#
# Usage: check-skill-snippets.sh [scan-root]   (default: <repo>/skills)
set -euo pipefail

ROOT="${1:-}"
if [[ -z "$ROOT" ]]; then
  REPO_ROOT=""
  if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    REPO_ROOT="$git_root"
  else
    REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  fi
  ROOT="$REPO_ROOT/skills"
fi

if [[ ! -d "$ROOT" ]]; then
  echo "check-skill-snippets: no such directory: $ROOT" >&2
  exit 2
fi

FILES=$(find "$ROOT" -type f -name '*.md' | sort)
if [[ -z "$FILES" ]]; then
  # A skills tree with zero markdown passes vacuously (Boundaries).
  echo "check-skill-snippets: no markdown under $ROOT (vacuous pass)"
  exit 0
fi

FINDINGS=$(printf '%s\n' "$FILES" | xargs awk '
function report(cls, hint,   _) {
  # lint-ok on this line or the previous one suppresses the finding.
  if (line ~ ("# lint-ok:[ \t]*" cls) || prev ~ ("# lint-ok:[ \t]*" cls)) return
  printf "%s:%d: %s\n    %s\n    %s\n", FILENAME, FNR, cls, line, hint
  found = 1
}

# Body of a ${VAR:+...} starting at position p (the char after ":+"), read to
# the matching close brace with depth counting so a nested ${...} is included.
function body_of(s, p,   depth, i, c, out) {
  depth = 1; out = ""
  for (i = p; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c == "{") depth++
    else if (c == "}") { depth--; if (depth == 0) return out }
    out = out c
  }
  return out
}

# True when the expansion at position p is NOT inside double quotes. Quote
# counting restarts at the last $( because a command substitution opens a fresh
# quoting context - the reason ROUTING="$(cmd ${V:+--flag "$V"})" is a hazard
# even though the line has an opening quote before it.
function unquoted_at(s, p,   seg, q) {
  seg = substr(s, 1, p - 1)
  while (match(seg, /\$\(/)) seg = substr(seg, RSTART + 2)
  q = gsub(/"/, "\"", seg)
  return (q % 2) == 0
}

/^[ \t]*```/ {
  if (inblk) inblk = 0
  else if ($0 ~ /^[ \t]*```(bash|sh|zsh|shell)[ \t]*$/) inblk = 1
  prev = $0
  next
}

!inblk { prev = $0; next }

{
  line = $0

  # 1. unquoted ${VAR:+...} whose body carries whitespace.
  # RSTART/RLENGTH are globals that any nested match() clobbers, so snapshot
  # them before calling a helper - otherwise the cursor never advances.
  rest = line; base = 0
  while (match(rest, /\$\{[A-Za-z_][A-Za-z_0-9]*:\+/)) {
    r = RSTART; l = RLENGTH
    abs = base + r
    if (body_of(line, abs + l - 1) ~ /[ \t]/ && unquoted_at(line, abs))
      report("unquoted-conditional-expansion", \
        "portable: args=(); [[ -n \"$V\" ]] && args+=(--flag \"$V\"); cmd \"${args[@]+\"${args[@]}\"}\"")
    base += r + l - 1
    rest = substr(rest, r + l)
  }

  # 2. empty regex alternation in a grep pattern
  if (line ~ /(^|[ \t;|&(])grep([ \t]|$)/ &&
      (line ~ /["'"'"'][^"'"'"']*\|["'"'"']/ || line ~ /["'"'"']\|/ || line ~ /\|\|[^ \t]*["'"'"']/))
    report("empty-grep-alternation", \
      "ugrep rejects an empty (sub)expression; filter jq-side with select(. != null and . != \"\")")

  # 3. bare timeout with no gtimeout fallback
  if (line ~ /(^|[ \t;|&(])timeout[ \t]+[0-9]/ && line !~ /gtimeout/)
    report("bare-timeout", \
      "macOS has no timeout binary: TO=$(command -v timeout || command -v gtimeout)")

  # 4. single-spelling stat
  if (line ~ /(^|[ \t;|&(=$])stat[ \t]+-c/ && line !~ /stat[ \t]+-f/)
    report("single-spelling-stat", "GNU-only: try stat -f (BSD) as a fallback")
  if (line ~ /(^|[ \t;|&(=$])stat[ \t]+-f/ && line !~ /stat[ \t]+-c/)
    report("single-spelling-stat", "BSD-only: try stat -c (GNU) as a fallback")

  # 5. a mutating fno verb whose failure is swallowed
  if (line ~ /\|\|[ \t]*true[ \t]*$/ &&
      line ~ /fno (backlog (idea|advance|reconcile|update|done|rank|capture add|session add|batch)|retro run|carveout (add|resolve)|plan reconcile-status|pr sync-canonical|skill-diff|worktree archive|state set|agents (stop|rm))/)
    report("fno-mutation-swallowed", \
      "best-effort mutation must be visible: verb || { echo \"...failed\" >&2; FAILURES+=(\"<verb>\"); }")

  prev = line
}

END { exit 0 }
' 2>/dev/null || true)

if [[ -n "$FINDINGS" ]]; then
  echo "ERROR: hazardous shell constructs in skill snippets:" >&2
  echo "" >&2
  echo "$FINDINGS" >&2
  echo "" >&2
  echo "Each class has shipped a silent no-op. Fix, or annotate the line with" >&2
  echo "'# lint-ok: <class>' when the construct is genuinely safe here." >&2
  exit 1
fi

echo "no hazardous snippet constructs under $ROOT"
