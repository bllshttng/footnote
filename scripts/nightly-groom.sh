#!/usr/bin/env bash
# DEPRECATED shim. Grooming is one pipeline behind one verb: `fno backlog groom`.
#
# That verb now runs the mechanical legs this script used to sequence (archive,
# reconcile, maintain --apply, relatedness build) under its daily claim, then
# dispatches the judgment worker. The old proposal digest is gone: the worker
# re-derives today's proposals from the live graph, so nothing can go stale.
#
# The old proposal digest under the state dir is no longer written or read by
# anything and can be deleted; see docs/backlog-usage.md.
# This shim ships one release, then goes away.
set -u

echo "nightly-groom.sh is DEPRECATED - use \`fno backlog groom\` (it now runs the mechanical legs too)." >&2
echo "Install the daily cadence once with: fno backlog groom --install-agent" >&2

exec "${FNO:-fno}" backlog groom "$@"
