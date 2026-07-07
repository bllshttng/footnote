#!/usr/bin/env bash
# attest-model.sh - SessionStart guard (a) Layer 1: model/provider env coherence.
#
# Catches the x-db50 bug class: ANTHROPIC_MODEL names a non-Anthropic model
# (e.g. a glm-* routing target) but ANTHROPIC_BASE_URL is empty or an
# anthropic.com host, so the request silently falls back to the primary
# Anthropic model. Detectable at SessionStart with zero API calls.
#
# Advisory only: prints a plain-text warning to stdout (SessionStart stdout is
# injected as additionalContext) and always exits 0. An un-routed session
# (no ANTHROPIC_MODEL) is coherent by definition and prints nothing. It also
# records the resolved intended identity to a per-session sidecar that the
# PostToolUse drift check (Layer 2, context-monitor.js) reads.
set -uo pipefail

# session_id from stdin (SessionStart payload). Skip the read when stdin is a
# TTY (manual invocation) so the hook never blocks. Fail open on any trouble.
SESSION_ID=""
if [ ! -t 0 ] && command -v jq >/dev/null 2>&1; then
  STDIN_JSON="$(cat 2>/dev/null || true)"
  SESSION_ID="$(printf '%s' "$STDIN_JSON" | jq -r '.session_id // empty' 2>/dev/null || true)"
fi

MODEL="${ANTHROPIC_MODEL:-}"
BASE="${ANTHROPIC_BASE_URL:-}"
TOKEN="${ANTHROPIC_AUTH_TOKEN:-}"

# Provider from the manifest when a target session owns this cwd, else interactive.
PROVIDER="interactive"
GUARD_LIB="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/scripts/lib/target-guard.sh"
if [[ -f "$GUARD_LIB" ]]; then
  # shellcheck source=../scripts/lib/target-guard.sh
  source "$GUARD_LIB" 2>/dev/null || true
  _p="$(target_state_field provider 2>/dev/null || true)"
  [[ -n "$_p" ]] && PROVIDER="$_p"
fi

# base_url host (empty when unset): strip scheme, path, and port.
BASE_HOST=""
if [[ -n "$BASE" ]]; then
  BASE_HOST="${BASE#*://}"; BASE_HOST="${BASE_HOST%%/*}"; BASE_HOST="${BASE_HOST%%:*}"
fi

# Record the resolved intended identity for Layer 2 + post-hoc audit (best effort).
if [[ -n "$SESSION_ID" ]]; then
  SIDE="$HOME/.claude/.fno-attest-${SESSION_ID}.json"
  TS="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo '')"
  printf '{"model":"%s","base_url_host":"%s","provider":"%s","ts":"%s"}\n' \
    "$MODEL" "$BASE_HOST" "$PROVIDER" "$TS" > "$SIDE" 2>/dev/null || true
fi

# Coherence is only meaningful when a model is explicitly routed.
[[ -z "$MODEL" ]] && exit 0

# Anthropic models start with "claude"; anything else is a routed foreign model.
case "$MODEL" in
  claude*) exit 0 ;;   # coherent: Anthropic model, any base
esac

# Non-Anthropic model. If the base URL is empty or an anthropic.com host, the
# request silently falls back to the primary Anthropic model (the x-db50 bug).
# Match the host exactly or as a subdomain - a bare *anthropic.com glob would
# also match e.g. notanthropic.com.
if [[ -z "$BASE_HOST" || "$BASE_HOST" == "anthropic.com" || "$BASE_HOST" == *.anthropic.com ]]; then
  echo "⚠️  MODEL ROUTING DRIFT: ANTHROPIC_MODEL='${MODEL}' names a non-Anthropic model but ANTHROPIC_BASE_URL is ${BASE_HOST:-unset} (Anthropic). Requests will silently fall back to the primary Anthropic model. Fix the routing env or unset ANTHROPIC_MODEL before relying on this session."
  exit 0
fi

# Routed to a real non-Anthropic base. Flag an Anthropic OAuth token where the
# routed provider expects its own API key (the x-db50 OAuth-scrub failure).
case "$TOKEN" in
  sk-ant-oat*)
    echo "⚠️  MODEL ROUTING WARNING: routed to '${MODEL}' at ${BASE_HOST} but ANTHROPIC_AUTH_TOKEN looks like an Anthropic OAuth token (sk-ant-oat…). A routed lane usually needs that provider's API key; verify the token was swapped for this lane."
    ;;
esac
exit 0
