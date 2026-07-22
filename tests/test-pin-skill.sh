#!/usr/bin/env bash
# Self-test for scripts/pin-skill.sh (epic ab-0d05a9b7, group 5).
# Proves AC6-FR: unpin removes only pin-marked shims and leaves a
# user-authored skill of the same name intact. Also covers the net-new pin
# lifecycle, idempotent re-pin, the --replace deprecation path, and the
# sibling-preservation warning.
#
# Hermetic: runs the real script against a throwaway git repo so it never
# touches the live skills/ tree.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/pin-skill.sh"
MARKER='<!-- fno-pinned-skill -->'

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT
git -C "$SANDBOX" init -q
mkdir -p "$SANDBOX/skills"

pass=0
fail=0
ok()   { echo "PASS: $1"; pass=$((pass+1)); }
bad()  { echo "FAIL: $1"; fail=$((fail+1)); }

run() { ( cd "$SANDBOX" && bash "$SCRIPT" "$@" ); }

# 1. pin a net-new shortcut creates a marked shim.
run pin sigma --to "review sigma" >/dev/null
if [[ -f "$SANDBOX/skills/sigma/SKILL.md" ]] && grep -qF "$MARKER" "$SANDBOX/skills/sigma/SKILL.md"; then
  ok "pin creates a marked shim at skills/sigma/SKILL.md"
else
  bad "pin did not create a marked shim"
fi

# 2. the shim redirects to the router form (and uses no Skill() call).
if grep -qF '/review sigma' "$SANDBOX/skills/sigma/SKILL.md" \
   && ! grep -qE 'Skill\(' "$SANDBOX/skills/sigma/SKILL.md"; then
  ok "shim redirects to /review sigma with no Skill() call"
else
  bad "shim body wrong (missing redirect or contains Skill())"
fi

# 3. list reports the pin.
if run list | grep -qF '/sigma'; then
  ok "list reports the pinned shim"
else
  bad "list did not report the pin"
fi

# 4. re-pin is idempotent (no error, still marked).
if run pin sigma --to "review sigma" >/dev/null 2>&1 \
   && grep -qF "$MARKER" "$SANDBOX/skills/sigma/SKILL.md"; then
  ok "re-pin is idempotent"
else
  bad "re-pin failed"
fi

# 5. unpin removes the pin-marked shim.
run unpin sigma >/dev/null
if [[ ! -e "$SANDBOX/skills/sigma" ]]; then
  ok "unpin removes the marked shim folder"
else
  bad "unpin left the shim folder behind"
fi

# 6. AC6-FR: a user-authored skill (no marker) is NEVER removed by unpin.
mkdir -p "$SANDBOX/skills/triage"
printf -- '---\nname: triage\ndescription: "real skill"\n---\n\nhand-authored body\n' \
  > "$SANDBOX/skills/triage/SKILL.md"
if run unpin triage >/dev/null 2>&1; then
  bad "unpin removed a non-pin skill (AC6-FR violation: should refuse)"
else
  if [[ -f "$SANDBOX/skills/triage/SKILL.md" ]] \
     && grep -qF "hand-authored body" "$SANDBOX/skills/triage/SKILL.md"; then
    ok "unpin refuses and leaves a user-authored skill intact (AC6-FR)"
  else
    bad "unpin damaged a user-authored skill (AC6-FR violation)"
  fi
fi

# 7. pin refuses to clobber an existing non-pin skill without --replace.
if run pin triage --to "backlog triage" >/dev/null 2>&1; then
  bad "pin clobbered an existing non-pin skill without --replace"
else
  if grep -qF "hand-authored body" "$SANDBOX/skills/triage/SKILL.md"; then
    ok "pin refuses to clobber an existing non-pin skill (no --replace)"
  else
    bad "pin damaged a non-pin skill despite refusing"
  fi
fi

# 8. pin --replace deprecates an existing non-pin skill (writes only SKILL.md,
#    preserving siblings) and warns about the retained sibling.
mkdir -p "$SANDBOX/skills/operatorish/references"
printf -- '---\nname: operatorish\ndescription: "x"\n---\nbody\n' \
  > "$SANDBOX/skills/operatorish/SKILL.md"
echo "bundled-source" > "$SANDBOX/skills/operatorish/references/keep.md"
warn="$(run pin operatorish --to "do waves" --replace 2>&1 >/dev/null || true)"
if grep -qF "$MARKER" "$SANDBOX/skills/operatorish/SKILL.md" \
   && [[ -f "$SANDBOX/skills/operatorish/references/keep.md" ]]; then
  ok "pin --replace deprecates SKILL.md and preserves sibling files"
else
  bad "pin --replace did not preserve siblings / did not stamp marker"
fi
# unpin a --replace'd deprecation removes the shim SKILL.md but KEEPS the
# bundler-sourced sibling plumbing (rmdir only fires on an empty folder).
run unpin operatorish >/dev/null
if [[ ! -f "$SANDBOX/skills/operatorish/SKILL.md" ]] \
   && [[ -f "$SANDBOX/skills/operatorish/references/keep.md" ]]; then
  ok "unpin removes the shim SKILL.md but preserves sibling plumbing"
else
  bad "unpin destroyed sibling plumbing or left the shim behind"
fi

# 9. AC6-FR (regression): a user-authored skill that merely QUOTES the marker
#    string inline in its body must NOT be treated as a pin by unpin.
mkdir -p "$SANDBOX/skills/docskill"
printf -- '---\nname: docskill\ndescription: "documents pinning"\n---\n\nThe marker is %s used inline here.\n' \
  "$MARKER" > "$SANDBOX/skills/docskill/SKILL.md"
if run unpin docskill >/dev/null 2>&1; then
  bad "unpin removed a skill that only quotes the marker inline (AC6-FR violation)"
else
  if [[ -f "$SANDBOX/skills/docskill/SKILL.md" ]]; then
    ok "unpin refuses a skill that only quotes the marker inline (whole-line match)"
  else
    bad "unpin destroyed a skill that only quotes the marker inline"
  fi
fi

# 10. pin onto a pre-existing folder with plumbing but NO SKILL.md: writes the
#     shim, preserves siblings, and notes them (unpin keeps them) - no --replace.
mkdir -p "$SANDBOX/skills/halfskill/references"
echo "bundled-source" > "$SANDBOX/skills/halfskill/references/data.md"
note="$(run pin halfskill --to "do waves" 2>&1 >/dev/null || true)"
if grep -qF "$MARKER" "$SANDBOX/skills/halfskill/SKILL.md" \
   && echo "$note" | grep -qiF "preserved on unpin" \
   && [[ -f "$SANDBOX/skills/halfskill/references/data.md" ]]; then
  ok "pin onto a SKILL.md-less folder notes + preserves siblings"
else
  bad "pin onto a SKILL.md-less folder did not note/preserve siblings"
fi

# 11. a flag missing its value errors with a message (not a bare set -e abort).
err="$(run pin foo --to 2>&1 || true)"
if echo "$err" | grep -qiF "needs a value"; then
  ok "pin --to with no value errors with a diagnostic"
else
  bad "pin --to with no value aborted without a diagnostic"
fi

# 12. unpin rejects a path-traversal name (never builds a path outside skills/).
if run unpin ../evil >/dev/null 2>&1; then
  bad "unpin accepted a path-traversal name"
else
  ok "unpin rejects a path-traversal name"
fi

echo ""
echo "pin-skill self-test: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
