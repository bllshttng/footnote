#!/usr/bin/env bash
# Validates that the plugin declares a postinstall hook and that the
# hook exists and is executable. Does NOT run the installer (that would
# modify the user's PATH / Python env).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

test -f .claude-plugin/plugin.json || { echo "FAIL: .claude-plugin/plugin.json missing"; exit 1; }
test -f .claude-plugin/postinstall.sh || { echo "FAIL: .claude-plugin/postinstall.sh missing"; exit 1; }
test -x .claude-plugin/postinstall.sh || { echo "FAIL: .claude-plugin/postinstall.sh not executable"; exit 1; }

grep -q "postInstall" .claude-plugin/plugin.json \
  || { echo "FAIL: plugin.json missing postInstall field"; exit 1; }
grep -q ".claude-plugin/postinstall.sh" .claude-plugin/plugin.json \
  || { echo "FAIL: plugin.json postInstall does not point at postinstall.sh"; exit 1; }

# Sanity: the hook script references the uv -> pip -> error fallback chain.
for needle in "uv tool install" "pip install --user" "ERROR:"; do
  grep -q "$needle" .claude-plugin/postinstall.sh \
    || { echo "FAIL: postinstall.sh missing expected content: $needle"; exit 1; }
done

# ab-18563bcc US7: the hook prefers the published PyPI platform wheel BY NAME
# (binary-complete), guards it against the name collision / reserved placeholder
# via a version match, falls back to the bundled source, and reports which path
# it took so the user knows whether daemon-backed verbs will work.
for needle in \
  "uv tool install --force fno" \
  "uv tool uninstall fno" \
  "__version__" \
  "binary-complete" \
  "fno update --rust"; do
  grep -q "$needle" .claude-plugin/postinstall.sh \
    || { echo "FAIL: postinstall.sh missing US7 content: $needle"; exit 1; }
done

# The by-name install must be guarded by a version comparison, not unconditional.
grep -q 'SRC_VERSION' .claude-plugin/postinstall.sh \
  || { echo "FAIL: postinstall.sh by-name install is not version-guarded"; exit 1; }

# Syntax check: a broken postinstall silently no-ops the plugin install.
bash -n .claude-plugin/postinstall.sh \
  || { echo "FAIL: postinstall.sh has a syntax error"; exit 1; }

echo "PASS: postinstall hook declared, executable, and US7 binary-complete-preference wired"
