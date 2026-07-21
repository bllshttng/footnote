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
#                                   watcher silently never runs. gtimeout is
#                                   only a partial escape (it needs Homebrew
#                                   coreutils, and the fallback must branch on
#                                   the empty case); a builtin watchdog needs
#                                   neither binary.
#   single-spelling-stat            stat -c (GNU) or stat -f (BSD) alone.
#   guarded-empty-array-arg         "${a[@]+"${a[@]}"}" on an empty array: bash
#                                   expands to no arguments, zsh to one EMPTY
#                                   argument (measured argc 2 vs 3).
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

AWK_ERR="$(mktemp)"
# No `|| true` here: an awk that crashes (a regex dialect this program does not
# use, a broken PATH awk) would otherwise yield empty FINDINGS and print the
# success line - a gate whose whole thesis is "silent no-ops ship bugs" being a
# silent no-op itself. Its status is captured and hard-fails below.
set +e
FINDINGS=$(printf '%s\n' "$FILES" | xargs awk -v Q=\' '
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

# True when the expansion at position p is NOT inside double quotes.
#
# A command substitution opens a FRESH quoting context, so ROUTING="$(cmd
# ${V:+--flag "$V"})" is a hazard despite the opening quote earlier on the line.
# But only an OPEN one: a $( ... ) that already closed must restore the outer
# context, else X="$(foo)"; cmd ${V:+--flag "$V"} reads as quoted and the hazard
# is missed. So walk the prefix with a stack rather than resetting at the last $(.
function unquoted_at(s, p,   i, c, dq, sp, stack) {
  dq = 0; sp = 0
  for (i = 1; i < p; i++) {
    c = substr(s, i, 1)
    if (c == "\\") { i++; continue }
    if (c == "$" && substr(s, i + 1, 1) == "(") {
      stack[++sp] = dq; dq = 0; i++; continue
    }
    if (c == ")" && sp > 0) { dq = stack[sp--]; continue }
    if (c == "\"") dq = !dq
  }
  return !dq
}

# Fence state is per FILE. Without this an unclosed fence in one document would
# leak "inside a block" into every file after it (one awk process reads them all),
# turning the rest of the tree into false positives.
FNR == 1 { inblk = 0; prev = "" }

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
        "portable: if [ -n \"$V\" ]; then cmd --flag \"$V\"; else cmd; fi")
    base += r + l - 1
    rest = substr(rest, r + l)
  }

  # 2. empty regex alternation in a grep pattern. Q is the single-quote char,
  # passed in via -v: embedding one literally would need the '"'"' dance, which
  # closes the shell quoting mid-program and turns any following metacharacter
  # into a shell token. Fitting hazard for this file to avoid rather than dodge.
  #
  # The leading-alternative form requires the quote to sit where an argument
  # OPENS (after whitespace, = or an open paren), because a closing quote
  # followed by | is just an unspaced shell pipeline, not an empty alternation.
  QC = "[\"" Q "]"
  if (line ~ /(^|[ \t;|&(])grep([ \t]|$)/ &&
      (line ~ (QC "[^\"" Q "]*\\|" QC) || line ~ ("[ \t=(]" QC "\\|") || line ~ ("\\|\\|[^ \t]*" QC)))
    report("empty-grep-alternation", \
      "ugrep rejects an empty (sub)expression; filter jq-side with select(. != null and . != \"\")")

  # 3. bare timeout with no gtimeout fallback
  if (line ~ /(^|[ \t;|&(])timeout[ \t]+[0-9]/ && line !~ /gtimeout/)
    report("bare-timeout", \
      "macOS has neither timeout nor gtimeout: bound with builtins - cmd & w=$!; (sleep N; kill $w 2>/dev/null) & wait $w")

  # 4. single-spelling stat
  if (line ~ /(^|[ \t;|&(=$])stat[ \t]+-c/ && line !~ /stat[ \t]+-f/)
    report("single-spelling-stat", "GNU-only: try stat -f (BSD) as a fallback")
  if (line ~ /(^|[ \t;|&(=$])stat[ \t]+-f/ && line !~ /stat[ \t]+-c/)
    report("single-spelling-stat", "BSD-only: try stat -c (GNU) as a fallback")

  # 5b. the guarded empty-array expansion "${a[@]+"${a[@]}"}". Added to survive
  # bash 3.2 + set -u, but on an EMPTY array zsh expands it to one empty
  # argument where bash expands it to none - measured: bash argc=2, zsh argc=3.
  # Plain "${a[@]}" is identical across shells but errors under bash set -u, so
  # no array form is portable. Branch instead.
  if (line ~ /\[@\][+]"\$\{[A-Za-z_][A-Za-z_0-9]*\[@\]\}"/)
    report("guarded-empty-array-arg", \
      "zsh passes one EMPTY arg here; branch instead: if [ -n \"$V\" ]; then cmd --flag \"$V\"; else cmd; fi")

  # 5. a mutating fno verb whose failure is swallowed. A trailing comment must
  # not defeat the anchor - `... || true   # best effort` is the same hazard.
  if (line ~ /\|\|[ \t]*true[ \t]*(#.*)?$/ &&
      line ~ /fno (backlog (idea|advance|reconcile|update|done|rank|capture add|session add|batch)|retro run|carveout (add|resolve)|plan reconcile-status|pr sync-canonical|skill-diff|worktree archive|state set|agents (stop|rm))/)
    report("fno-mutation-swallowed", \
      "best-effort mutation must be visible: verb || { echo \"...failed\" >&2; FAILURES+=(\"<verb>\"); }")

  prev = line
}

END { exit 0 }
' 2>"$AWK_ERR")
AWK_RC=$?
set -e

if [[ $AWK_RC -ne 0 ]]; then
  echo "ERROR: the snippet scanner itself failed (awk exit $AWK_RC)." >&2
  echo "A crashed lint is a red build, not a skipped check." >&2
  cat "$AWK_ERR" >&2
  rm -f "$AWK_ERR"
  exit 2
fi
rm -f "$AWK_ERR"

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
