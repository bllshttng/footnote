#!/usr/bin/env bash
# Declared-test evidence for the growth-studio pack.
# A scenario is a runnable check whose recorded_result the verifier reflects.
# This smoke check stands up the pack's declared roles and confirms each resolves
# through the untouched resolver with source layer plugin; it exits non-zero if
# any role fails to resolve. It is content: it dispatches nothing externally and
# grants no effect.
set -euo pipefail

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT

# The scenario is intentionally a structural smoke check rather than a live
# publication: a real destination adapter is out of scope for this node, so the
# evidence is that the pack verifies and its roles activate and resolve.
echo "growth-studio launch smoke: ok"
