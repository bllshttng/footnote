#!/usr/bin/env bash
# Merge-posture ownership after x-3873: the per-run --allow-merge / --no-merge
# flags are gone from /target bg (AC5-EDGE). config.auto_merge.grant decides at
# target init, exactly as it does for every advance worker; the one per-run
# override is a typed message on the spawn:
#   fno agents spawn --node <id> '/fno:target --no-merge <id>'
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"
TMP="$(mktemp -d -t dispatch-grant-posture.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

MOCKBIN="$TMP/bin"
NODES_JSON="$TMP/nodes"
mkdir -p "$MOCKBIN" "$NODES_JSON"

cat > "$MOCKBIN/fno" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-} ${2:-}" in
  "backlog get")
    printf '{"id":"%s","status":"ready","slug":"grant-posture","cwd":"%s"}\n' "$NODE_ID" "$PWD"
    ;;
  "agents spawn-guard")
    printf '{"verdict":"dispatchable"}\n'
    ;;
  "agents name")
    printf 'target-%s\n' "$NODE_ID"
    ;;
  "agents spawn")
    printf '{"name":"target-%s","short_id":"deadbeef01","harness":"claude","status":"live"}\n' "$NODE_ID"
    ;;
  *) ;;
esac
MOCK
chmod +x "$MOCKBIN/fno"
NODE_ID="x-884f01"
export NODE_ID
export PATH="$MOCKBIN:$PATH"

echo "== the shell launcher takes no per-run posture flag =="
for flag in --allow-merge --no-merge; do
  dispatch_out="$(bash "$DISPATCH" --dry-run $flag "$NODE_ID" 2>&1)" && rc=0 || rc=$?
  [[ "$rc" -eq 2 ]] || { echo "FAIL: $flag must exit 2 as an unknown flag (rc=$rc): $dispatch_out"; exit 1; }
  grep -q "failed: $flag reason=\"unknown flag\"" <<<"$dispatch_out" \
    || { echo "FAIL: refusal must name $flag: $dispatch_out"; exit 1; }
  ! grep -q "would run" <<<"$dispatch_out" || { echo "FAIL: $flag must launch nothing"; exit 1; }
done
echo "PASS: --allow-merge / --no-merge are unknown flags; nothing launches"

echo "== the launch carries no posture either way: the grant decides worker-side =="
dispatch_out="$(bash "$DISPATCH" --dry-run "$NODE_ID" 2>&1)"
grep -q "would run: fno agents spawn --node $NODE_ID --substrate thread --name target-$NODE_ID" <<<"$dispatch_out" \
  || { echo "FAIL: preview argv is not the bare door: $dispatch_out"; exit 1; }
! grep -q -- "--no-merge" <<<"$dispatch_out" \
  || { echo "FAIL: a no-merge carrier must not be injected by the launcher"; exit 1; }
! grep -q -- "--allow-merge" <<<"$dispatch_out" \
  || { echo "FAIL: no allow-merge carrier rides the spawn"; exit 1; }
echo "PASS: the launcher passes the node and nothing posture-shaped"

echo "== the grant is read at target init, inside the door's resolver =="
# resolve_node_spawn is the ONE preference resolver the door's node-seeded
# branch and the advance path share; the grant call must live there, never in
# a shell launcher.
node_dispatch="$REPO_ROOT/cli/src/fno/agents/node_dispatch.py"
grep -q "auto_merge_grant(settings_obj)" "$node_dispatch" \
  || { echo "FAIL: resolve_node_spawn no longer reads auto_merge_grant"; exit 1; }
! grep -q "auto_merge_grant" "$DISPATCH" \
  || { echo "FAIL: the shell launcher must not read the grant itself"; exit 1; }
echo "PASS: the grant is read once, in the shared resolver"

echo "PASS: dispatch and the door resolve grant posture from config, worker-side"
