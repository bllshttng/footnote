#!/usr/bin/env bash
# check-hook-exec-bit.sh - CI gate: a hook the harness runs must be runnable.
#
# A hook command like "${CLAUDE_PLUGIN_ROOT}/hooks/foo.sh" is exec'd directly,
# so a file committed at mode 100644 fails with "Permission denied" and the
# hook silently never runs. That failure is invisible in the direction that
# matters: a nudge that never fires looks exactly like nothing to report.
#
# Checks git's INDEX mode, not the filesystem, because the index is what ships.
# Scope: every hooks/*.sh named in a hook manifest. A file reached only through
# an interpreter ("bash path/to.sh") does not strictly need the bit, but it is
# still held to 100755 - one rule, and consistency with its 40-odd siblings.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

manifests=(hooks/hooks.json hooks/codex-hooks.json)
referenced=$(python3 - "${manifests[@]}" <<'PY'
import json, re, sys

seen = set()
for path in sys.argv[1:]:
    try:
        blob = open(path).read()
    except FileNotFoundError:
        continue
    # Hook manifests nest command strings at varying depths; a scan over the
    # raw text finds every reference without tracking the schema.
    for m in re.finditer(r'hooks/[A-Za-z0-9._/-]+\.sh', blob):
        seen.add(m.group(0))
print("\n".join(sorted(seen)))
PY
)

[ -n "$referenced" ] || { echo "check-hook-exec-bit: no hook scripts referenced; manifests moved?" >&2; exit 1; }

bad=0
for f in $referenced; do
  git ls-files --error-unmatch "$f" >/dev/null 2>&1 || continue
  mode=$(git ls-files -s "$f" | awk '{print $1}')
  if [ "$mode" != "100755" ]; then
    echo "FAIL $f is $mode, must be 100755 (a hook manifest runs it)" >&2
    bad=1
  fi
done

if [ "$bad" -ne 0 ]; then
  cat >&2 <<'EOF'

Fix: chmod +x <file> && git add <file>
(or: git update-index --chmod=+x <file>)
EOF
  exit 1
fi

echo "check-hook-exec-bit: ok ($(echo "$referenced" | wc -l | tr -d ' ') referenced hook scripts)"
