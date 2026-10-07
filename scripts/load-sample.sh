#!/usr/bin/env bash
# scripts/load-sample.sh - one host-tick load reading, for load investigations.
#
# Reads host CPU ticks (iostat on macOS, /proc/stat on Linux), load average,
# and the process table ONCE for counts and group names. It never sums
# `ps %CPU`: that figure is a decaying per-process lifetime average, blind to
# short-lived churn, and it read 610% while `top` read 0% idle in the
# 2026-10-03 sample - an undercount of about 600 points on a saturated box.
#
# Output: one line, stable key=value form:
#   load_sample cores=12 load1=12.3 load5=11.2 load15=9.1 busy_pct=97.0 \
#     runnable=104 processes=1168 top_groups=rustc:4,python3:15,WindowServer:1
set -euo pipefail

case "$(uname -s)" in
Darwin)
    cores=$(sysctl -n hw.ncpu)
    loadavg=$(sysctl -n vm.loadavg) # { 12.3 11.2 9.1 }
    loadavg=${loadavg#\{}
    loadavg=${loadavg%\}}
    read -r load1 load5 load15 _ <<EOF
$loadavg
EOF
    # iostat -c 2: the first sample is since-boot; the second is a real
    # delta. Device columns (variable in count) sit before the CPU fields,
    # and the line ends `us sy id` followed by three load averages, so the
    # CPU fields are read from the END, never by position from the front.
    iostat_line=$(iostat -c 2 | tail -1)
    busy_pct=$(printf '%s\n' "$iostat_line" | awk '{ printf "%.1f", $(NF - 5) + $(NF - 4) }')
    ;;
Linux)
    cores=$(nproc)
    read -r load1 load5 load15 _ </proc/loadavg
    # Two /proc/stat reads one second apart; busy = 100 * (1 - idle/total).
    stat_line() { rg '^(cpu) ' /proc/stat; }
    before=$(stat_line)
    sleep 1
    after=$(stat_line)
    busy_pct=$(python3 - "$before" "$after" <<'PY'
import re
import sys

def ticks(line):
    parts = [int(x) for x in line.split()[1:]]
    idle = parts[3] + parts[4]
    return sum(parts), idle

before, after = map(ticks, sys.argv[1:3])
total = after[0] - before[0]
idle = after[1] - before[1]
busy = 100.0 * (1.0 - idle / total) if total else 0.0
print(f"{busy:.1f}")
PY
)
    ;;
*)
    echo "load-sample: unsupported platform $(uname -s); use top/iostat by hand" >&2
    exit 2
    ;;
esac

# One ps pass for counts and groups: runnable begins with R (the census's
# own rule); groups are basename counts, never CPU sums.
ps_out=$(ps -eo state=,comm=)
runnable=$(awk '$1 ~ /^R/ {n++} END {print n + 0}' <<<"$ps_out")
processes=$(awk 'END {print NR}' <<<"$ps_out")
top_groups=$(awk '{
    $1 = ""
    sub(/^ +/, "")
    n = split($0, f, "/")
    name = f[n]
    gsub(/[[:space:]]+$/, "", name)
    count[name]++
} END { for (k in count) printf "%d %s\n", count[k], k }' <<<"$ps_out" \
    | sort -rn | head -5 | awk '{
    c = $1
    $1 = ""
    sub(/^ /, "")
    printf "%s%s:%s", sep, $0, c
    sep = ","
}')

echo "load_sample cores=$cores load1=$load1 load5=$load5 load15=$load15 busy_pct=$busy_pct runnable=$runnable processes=$processes top_groups=$top_groups"
