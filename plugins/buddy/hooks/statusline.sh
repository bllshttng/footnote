#!/bin/bash
# The status line entry point. Most ticks change nothing on screen: the same session input, the
# same width, and the same buddy frame as the last run. Then this prints the last output and
# Python never starts. It uses only bash built-ins: under load, each extra process costs more
# than the whole check. The mod rewrites every frame at least every 10 s, so Python still runs
# that often and nothing stays stale for longer.
shopt -s extglob
IFS= read -r -d '' input
dir=.
[[ $0 == */* ]] && dir=${0%/*}
if [[ $input =~ \"session_id\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]]; then
  f="$dir/frames/${BASH_REMATCH[1]}"
  # Durations tick every second without changing what the line shows, so they stay out of the key.
  # The frame's own text is in the key: bash 3.2 compares file times in whole seconds, which is too
  # coarse for frames that land twice a second. read -d '' stops at end of file and reports a
  # failure there, so its status is not the test.
  frame= last=
  [[ -f $f.json ]] && IFS= read -r -d '' frame < "$f.json"
  key="$COLUMNS ${input//\"+([a-z_])duration_ms\"*([[:space:]]):*([[:space:]])+([0-9.])/} $frame"
  [[ -f $f.in ]] && IFS= read -r -d '' last < "$f.in"
  if [[ -f $f.out && $last == "$key" ]]; then
    IFS= read -r -d '' out < "$f.out"
    printf '%s' "$out"
    exit 0
  fi
  printf '%s' "$key" > "$f.in.next" 2>/dev/null
fi
printf '%s' "$input" | BUDDY_FAST=1 exec python3 "$dir/statusline.py"
