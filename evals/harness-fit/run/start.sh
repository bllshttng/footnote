#!/bin/bash
# Start both runs detached, with the load sampler. Safe to run again after a crash or
# reboot: finished arms and lanes are skipped, killed jobs continue in place.
#   bash evals/harness-fit/run/start.sh [<arm>...]   (default arms: claude-code opencode pi terminus-2)
# Stop everything:   touch "$WS/logs/stop" and kill the pids this prints.
HERE="$(cd "$(dirname "$0")" && pwd)"
WS="${HARNESS_FIT_WS:-$HOME/evals-workspace/harness-fit}"
mkdir -p "$WS/logs"
docker info > /dev/null 2>&1 || { echo "docker is not answering; start it first"; exit 2; }
set -a; . "$HOME/.fno/.env"; set +a
[ -x "${FNO_AGENTS_BIN:-}" ] || { echo "FNO_AGENTS_BIN is unset or not executable; run setup-imac.sh"; exit 2; }
bindir="$(dirname "$FNO_AGENTS_BIN")"
export PATH="$bindir:$PATH"
arms=("$@")
[ ${#arms[@]} -eq 0 ] && arms=(claude-code opencode pi terminus-2)
cd "$WS" || exit 1
rm -f logs/stop
nohup bash -c 'while [ ! -f logs/stop ]; do set -- $(sysctl -n vm.loadavg | tr -d "{}"); echo "{\"at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\", \"load1\": $1, \"load5\": $2, \"load15\": $3}" >> logs/load.jsonl; sleep 60; done' \
  > /dev/null 2>&1 < /dev/null &
echo "load sampler pid $!"
nohup python3 "$HERE/drive_run0.py" "${arms[@]}" > logs/run0-driver.out 2>&1 < /dev/null &
echo "run0 pid $!"
nohup uv run --quiet --with pyyaml python "$HERE/drive_run1.py" > logs/run1-driver.out 2>&1 < /dev/null &
echo "run1 pid $!"
