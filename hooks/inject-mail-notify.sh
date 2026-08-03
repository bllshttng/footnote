#!/usr/bin/env bash
# hooks/inject-mail-notify.sh -- durable mail delivery at the turn boundary.
#
# The hidden CLI verb owns rendering, UserPromptSubmit JSON serialization,
# stdout flush, and only then cursor acknowledgement. This shell layer relays
# that already-valid envelope directly so there is no second capture or write
# boundary between visible delivery and acknowledgement. Silent when there is
# no harness identity or mail; a portable two-second timeout bounds a hung
# binary; failures never block the turn.

set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

with_timeout 2 fno mail notify-self 2>/dev/null || true

exit 0
