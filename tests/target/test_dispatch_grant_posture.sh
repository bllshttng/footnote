#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"
NORMALIZE="$REPO_ROOT/skills/agent/scripts/normalize.sh"
TMP="$(mktemp -d -t dispatch-grant-posture.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

MOCKBIN="$TMP/bin"
PROJECT="$TMP/project"
mkdir -p "$MOCKBIN" "$PROJECT/.fno"

cat > "$MOCKBIN/fno" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-} ${2:-}" in
  "config get")
    if [[ "${3:-}" == "auto_merge.grant" ]]; then
      printf '%s\n' "$PWD" >> "$CALL_LOG"
      cat "$PWD/.fno/auto_merge" 2>/dev/null || printf 'none\n'
    fi
    ;;
  "backlog get")
    printf '{"id":"%s","status":"ready","cwd":"%s","_resolved_cwd":"%s"}\n' "$NODE_ID" "$PROJECT" "$PROJECT"
    ;;
  "agents spawn-guard")
    printf '{"verdict":"dispatchable"}\n'
    ;;
  "agents dispatch"|"dispatch resolve")
    if [[ "${1:-}" == "agents" ]]; then
      printf '{"harness":"claude","substrate":"bg","command":"/target --no-merge {id}"}\n'
    else
      node=""; posture=""; command="/target {id}"
      args=("$@")
      for ((i = 0; i < ${#args[@]}; i++)); do
        case "${args[$i]}" in
          --node) node="${args[$((i + 1))]}" ;;
          --merge-posture) posture="${args[$((i + 1))]}" ;;
          --command) command="${args[$((i + 1))]}" ;;
        esac
      done
      command="${command//\{id\}/$node}"
      case "$posture" in
        allow) ;;
        no-merge) command="$command --no-merge" ;;
        from-config)
          grant="$(cat "$PWD/.fno/auto_merge" 2>/dev/null || printf 'none')"
          [[ "$(printf '%s' "$grant" | tr -d '[:space:]')" == "dispatch" ]] \
            || command="$command --no-merge"
          ;;
        *) command="$command --no-merge" ;;
      esac
      printf '{"harness":"claude","command":"%s"}\n' "$command"
    fi
    ;;
  "dispatch family")
    printf 'family\n'
    ;;
  "agents name")
    printf 'target-%s\n' "$NODE_ID"
    ;;
  "agents spawn")
    printf '{"name":"target-%s","short_id":"deadbeef01","harness":"claude","status":"live"}\n' "$NODE_ID"
    ;;
  *)
    ;;
esac
MOCK
chmod +x "$MOCKBIN/fno"
export PROJECT
NODE_ID="x-884f01"
export NODE_ID
CALL_LOG="$TMP/config.log"
export CALL_LOG
export PATH="$MOCKBIN:$PATH"

field() {
  printf '%s\n' "$1" | awk -F= -v key="$2" '$1 == key { sub(/^[^=]*=/, ""); print; exit }'
}

echo dispatch > "$PROJECT/.fno/auto_merge"
normalize_out="$(cd "$PROJECT" && bash "$NORMALIZE" --input "$NODE_ID")"
[[ "$(field "$normalize_out" allow_merge)" == 1 ]]
normalize_out="$(cd "$PROJECT" && bash "$NORMALIZE" --input "$NODE_ID" --no-merge)"
[[ "$(field "$normalize_out" allow_merge)" == 0 ]]
# Positive marker: the config-backed run really called the documented read;
# the explicit flag run above must not add another read.
[[ "$(wc -l < "$CALL_LOG" | tr -d '[:space:]')" -eq 1 ]]

dispatch_out="$(bash "$DISPATCH" --here --dry-run "$NODE_ID" 2>&1)"
grep -q "'/target $NODE_ID'" <<<"$dispatch_out"
! grep -q "'/target $NODE_ID --no-merge'" <<<"$dispatch_out"

rm "$PROJECT/.fno/auto_merge"
normalize_out="$(cd "$PROJECT" && bash "$NORMALIZE" --input "$NODE_ID")"
[[ "$(field "$normalize_out" allow_merge)" == 0 ]]

dispatch_out="$(bash "$DISPATCH" --here --dry-run "$NODE_ID" 2>&1)"
grep -q "'/target $NODE_ID --no-merge'" <<<"$dispatch_out"

: > "$CALL_LOG"
normalize_out="$(cd "$PROJECT" && bash "$NORMALIZE" --input "$NODE_ID" --allow-merge)"
[[ "$(field "$normalize_out" allow_merge)" == 1 ]]
[[ ! -s "$CALL_LOG" ]]

dispatch_out="$(bash "$DISPATCH" --here --dry-run --allow-merge "$NODE_ID" 2>&1)"
! grep -q 'no-merge' <<<"$dispatch_out"
[[ ! -s "$CALL_LOG" ]]

normalize_out="$(cd "$PROJECT" && env PATH="/usr/bin:/bin" bash "$NORMALIZE" --input "$NODE_ID")" || true
# AC8 fail-closed: with fno absent the family ask refuses the whole normalize
# loud (no allow_merge field at all); the one thing it must never do is grant.
[[ "$(field "$normalize_out" allow_merge)" != 1 ]]

echo "PASS: dispatch and normalize resolve grant posture from config and fail closed"
