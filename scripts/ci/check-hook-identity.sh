#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

header_present() {
  awk 'NR == 2 && /^# fno hook: [[:alpha:]][[:alnum:]_ -]* - [[:alnum:]].+$/ { found = 1 } END { exit !found }' "$1"
}

if [[ "${1:-}" == "--self-test" ]]; then
  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT
  printf '%s\n' '#!/usr/bin/env bash' 'echo unmarked' > "$tmpdir/missing.sh"
  printf '%s\n' '#!/usr/bin/env bash' '# fno hook: Stop - identity fixture' > "$tmpdir/marked.sh"
  printf '%s\n' '#!/usr/bin/env bash' '# old comment' '# fno hook: Stop - late identity fixture' > "$tmpdir/late.sh"
  printf '%s\n' '#!/usr/bin/env bash' '# fno hook: - purpose without an event' > "$tmpdir/malformed.sh"
  if header_present "$tmpdir/missing.sh"; then
    printf '%s\n' 'check-hook-identity: self-test accepted a missing header' >&2
    exit 1
  fi
  if header_present "$tmpdir/late.sh"; then
    printf '%s\n' 'check-hook-identity: self-test accepted a header after another comment' >&2
    exit 1
  fi
  if header_present "$tmpdir/malformed.sh"; then
    printf '%s\n' 'check-hook-identity: self-test accepted a header without an event' >&2
    exit 1
  fi
  if ! header_present "$tmpdir/marked.sh"; then
    printf '%s\n' 'check-hook-identity: self-test rejected a valid header' >&2
    exit 1
  fi
  printf '%s\n' 'check-hook-identity: self-test ok'
  exit 0
fi

if [[ -n "${1:-}" ]]; then
  printf 'check-hook-identity: unknown argument: %s\n' "$1" >&2
  exit 2
fi

count=0
failed=0
for file in hooks/*.sh; do
  [[ -f "$file" ]] || continue
  count=$((count + 1))
  if ! header_present "$file"; then
    printf 'FAIL %s: missing "# fno hook: <event> - <purpose>" as the first comment after the shebang\n' "$file" >&2
    failed=1
  fi
done

if [[ "$count" -eq 0 ]]; then
  printf '%s\n' 'check-hook-identity: no hooks/*.sh files found' >&2
  exit 1
fi
if [[ "$failed" -ne 0 ]]; then
  exit 1
fi

printf 'check-hook-identity: ok (%s shell hooks)\n' "$count"
