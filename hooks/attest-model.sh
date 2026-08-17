#!/usr/bin/env bash
# attest-model.sh - SessionStart guard (a) Layer 1: model/provider env coherence.
#
# Catches the x-db50 bug class across ALL FIVE model vars: any of
# ANTHROPIC_MODEL or the four ANTHROPIC_DEFAULT_<TIER>_MODEL vars names a
# non-Anthropic model (e.g. a glm-* routing target) while ANTHROPIC_BASE_URL
# is empty or an anthropic.com host, so every call on that tier errors rather
# than degrading. A tier default alone (the haiku var with ANTHROPIC_MODEL
# unset) is the exact shape that breaks the small tier, so reading
# ANTHROPIC_MODEL only never saw it. Bedrock and Vertex bail out first: they
# serve Anthropic models under ids that do not start with "claude-" and leave
# the base URL unset, so the coherence question does not apply there.
#
# Advisory only: prints a plain-text warning to stdout (SessionStart stdout is
# injected as additionalContext) and always exits 0. An env with no foreign
# model var is coherent by definition and prints nothing. It also records the
# resolved intended identity to a per-session sidecar that the PostToolUse
# drift check (Layer 2, spend-drift-monitor.js) reads; the sidecar keeps
# recording ANTHROPIC_MODEL only, because a tier default is not the running
# model and Layer 2 compares against the running model.
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
  SIDE="${FNO_HOME:-$HOME/.fno}/attest/${SESSION_ID}.json"
  mkdir -p "$(dirname "$SIDE")" 2>/dev/null || true
  TS="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo '')"
  printf '{"model":"%s","base_url_host":"%s","provider":"%s","ts":"%s"}\n' \
    "$MODEL" "$BASE_HOST" "$PROVIDER" "$TS" > "$SIDE" 2>/dev/null || true
fi

# Bedrock / Vertex: Anthropic models under ids that do not start with "claude-"
# (us.anthropic.claude-...) and no ANTHROPIC_BASE_URL, so the coherence checks
# below would warn on a correct setup. Bail before them.
_env_truthy() {
  case "$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')" in
    ""|0|false|no|off) return 1 ;;
    *) return 0 ;;
  esac
}
if _env_truthy "${CLAUDE_CODE_USE_BEDROCK:-}" || _env_truthy "${CLAUDE_CODE_USE_VERTEX:-}"; then
  exit 0
fi

# The five model vars, one list with the Python MODEL_ENV_KEYS
# (cli/src/fno/agents/model_routing.py); a Python test pins the two lists
# equal, so a new tier cannot land in one and not the other.
MODEL_ENV_VARS=(
  ANTHROPIC_MODEL
  ANTHROPIC_DEFAULT_OPUS_MODEL
  ANTHROPIC_DEFAULT_SONNET_MODEL
  ANTHROPIC_DEFAULT_HAIKU_MODEL
  ANTHROPIC_DEFAULT_FABLE_MODEL
)

# Collect every var naming a foreign model. Anthropic ids start with "claude-"
# and the bare tier aliases resolve to Anthropic models, so both are coherent.
OFFENDERS=""
for VAR in "${MODEL_ENV_VARS[@]}"; do
  VAL="${!VAR:-}"
  [[ -z "$VAL" ]] && continue
  case "$VAL" in
    claude-*|opus|sonnet|haiku|fable) continue ;;
  esac
  OFFENDERS+="${OFFENDERS:+, }${VAR}='${VAL}'"
done

# A foreign model only drifts when the endpoint is Anthropic's: empty base or
# an anthropic.com host. Match the host exactly or as a subdomain - a bare
# *anthropic.com glob would also match e.g. notanthropic.com. A foreign base
# serves those model ids, so a real route never warns.
if [[ -n "$OFFENDERS" ]] && { [[ -z "$BASE_HOST" ]] || [[ "$BASE_HOST" == "anthropic.com" ]] || [[ "$BASE_HOST" == *.anthropic.com ]]; }; then
  echo "⚠️  MODEL ROUTING DRIFT: ${OFFENDERS} name a non-Anthropic model but ANTHROPIC_BASE_URL is ${BASE_HOST:-unset} (Anthropic), so every call on those tiers errors rather than degrading. This env was inherited, usually from a long-lived claude background daemon started from a shell that held those exports; no config edit clears a running daemon. Pin the tier defaults in ~/.claude/settings.json env (that wins over an inherited value) or restart the daemon (that terminates every session under it)."
  exit 0
fi

# Routed to a real non-Anthropic base. Flag an Anthropic OAuth token where the
# routed provider expects its own API key (the x-db50 OAuth-scrub failure).
if [[ -n "$MODEL" ]]; then
  case "$MODEL" in
    claude-*) ;;
    *)
      case "$TOKEN" in
        sk-ant-oat*)
          echo "⚠️  MODEL ROUTING WARNING: routed to '${MODEL}' at ${BASE_HOST} but ANTHROPIC_AUTH_TOKEN looks like an Anthropic OAuth token (sk-ant-oat…). A routed lane usually needs that provider's API key; verify the token was swapped for this lane."
          ;;
      esac
      ;;
  esac
fi
exit 0
