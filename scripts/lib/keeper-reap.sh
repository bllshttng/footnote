# Shared by the mux test proofs: reap this run's own pane keepers.
# A pane keeper outlives a server kill by design, so test teardown must
# kill every fno-agents-worker --pane naming the run's temp directory
# before that directory is removed. Source this file, then call
# reap_tmp_keepers <dir> from the EXIT cleanup.

keeper_pids_for_tmp() {
  ps -axo pid=,command= 2>/dev/null | awk -v tmp="$1" '
    index($0, tmp) && $2 ~ /fno-agents-worker$/ && $3 == "--pane" { print $1 }
  '
}

reap_tmp_keepers() {
  local tmp_dir="$1"
  local pids
  pids="$(keeper_pids_for_tmp "$tmp_dir")"
  for pid in $pids; do
    kill -9 "$pid" 2>/dev/null || true
  done
  for _ in {1..100}; do
    pids="$(keeper_pids_for_tmp "$tmp_dir")"
    [ -z "$pids" ] && return 0
    sleep 0.05
  done
  echo "FAIL: pane keepers still name $tmp_dir: $pids" >&2
  return 1
}
