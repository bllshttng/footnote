#!/usr/bin/env bash
# Validates that the plugin installer is wired through the SessionStart
# front-door hook (Claude Code never runs a plugin.json postInstall key), and
# that the installer exists and is executable. Does NOT run the installer (that
# would modify the user's PATH / Python env).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

test -f .claude-plugin/plugin.json || { echo "FAIL: .claude-plugin/plugin.json missing"; exit 1; }
test -f .claude-plugin/postinstall.sh || { echo "FAIL: .claude-plugin/postinstall.sh missing"; exit 1; }
test -x .claude-plugin/postinstall.sh || { echo "FAIL: .claude-plugin/postinstall.sh not executable"; exit 1; }

# Claude Code ignores these keys at load AND install time, so they wire nothing.
if grep -q '"postInstall"' .claude-plugin/plugin.json; then
  echo "FAIL: plugin.json carries postInstall, which Claude Code never runs; the installer is wired from hooks/frontdoor-nudge-session-start.sh"
  exit 1
fi
if grep -q '"policy"' .claude-plugin/marketplace.json; then
  echo "FAIL: .claude-plugin/marketplace.json carries policy, an unknown field to claude plugin validate --strict"
  exit 1
fi
grep -q ".claude-plugin/postinstall.sh" hooks/frontdoor-nudge-session-start.sh \
  || { echo "FAIL: hooks/frontdoor-nudge-session-start.sh does not start .claude-plugin/postinstall.sh"; exit 1; }
grep -q "context-run.sh claude-session-start" hooks/hooks.json \
  || { echo "FAIL: hooks/hooks.json does not run context-run.sh claude-session-start"; exit 1; }
grep -q "frontdoor-nudge-session-start.sh" hooks/context-hooks.json \
  || { echo "FAIL: hooks/context-hooks.json does not wire frontdoor-nudge-session-start.sh"; exit 1; }
# A plugin-only install has no fno-agents, so context-run.sh must reach the hook without it.
grep -q "frontdoor-nudge-session-start.sh" hooks/context-run.sh \
  || { echo "FAIL: hooks/context-run.sh does not run the front-door hook when fno-agents is missing"; exit 1; }

if claude plugin validate --help >/dev/null 2>&1; then
  claude plugin validate . --strict >/dev/null 2>&1 \
    || { echo "FAIL: claude plugin validate . --strict exits nonzero"; exit 1; }
else
  echo "SKIP: claude CLI absent, strict manifest validation not run"
fi

# Sanity: the hook script references the uv -> pip -> error fallback chain.
for needle in "uv tool install" "pip install --user" "ERROR:"; do
  grep -q "$needle" .claude-plugin/postinstall.sh \
    || { echo "FAIL: postinstall.sh missing expected content: $needle"; exit 1; }
done

# ab-18563bcc US7: the hook prefers the published PyPI platform wheel BY NAME
# (binary-complete), guards it against the name collision / reserved placeholder
# via a version match, falls back to the bundled source, and reports which path
# it took so the user knows whether daemon-backed verbs will work. The version
# source is plugin.json - release.yml stamps it per channel, and it is the one
# version field the plugin channels keep distinct (x-503d).
for needle in \
  'uv tool install --force --compile-bytecode "$@"' \
  "uv tool uninstall fno" \
  "plugin.json" \
  "plugin_channel" \
  "plugin_version_matches" \
  "binary-complete" \
  "fno doctor update --rust"; do
  grep -q "$needle" .claude-plugin/postinstall.sh \
    || { echo "FAIL: postinstall.sh missing US7 content: $needle"; exit 1; }
done

# The by-name install must be guarded by a version comparison, not unconditional.
grep -q 'SRC_VERSION' .claude-plugin/postinstall.sh \
  || { echo "FAIL: postinstall.sh by-name install is not version-guarded"; exit 1; }

# x-538e AC2-HP/AC2-EDGE: the receipt proves the advertised command - the
# front-door path, native mux, and Python forwarding - and a missing front
# door is a NAMED incomplete install with its repair, never silent.
for needle in \
  "verify_frontdoor" \
  "fno front door:" \
  "fno mux ls" \
  "incomplete install" \
  "cargo install fno"; do
  grep -q "$needle" .claude-plugin/postinstall.sh \
    || { echo "FAIL: postinstall.sh missing front-door receipt content: $needle"; exit 1; }
done

# The idempotency skip must require the front door too: a same-version install
# missing the mux (pre-x-538e wheel) must not take the skip.
grep -q 'command -v fno >' .claude-plugin/postinstall.sh \
  || { echo "FAIL: postinstall.sh idempotency guard does not require the fno front door"; exit 1; }

# Syntax check: a broken postinstall silently no-ops the plugin install.
bash -n .claude-plugin/postinstall.sh \
  || { echo "FAIL: postinstall.sh has a syntax error"; exit 1; }

echo "PASS: installer wired from the SessionStart front-door hook, executable, and US7 binary-complete-preference wired"
