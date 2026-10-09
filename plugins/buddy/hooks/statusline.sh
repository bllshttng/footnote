#!/bin/bash
# The status line entry point. Most ticks change nothing on screen: the same session input, the
# same width, and the same buddy frame as the last run. Then this prints the last output and
# Python never starts. It uses only bash built-ins: under load, each extra process costs more
# than the whole check. Every HITS-th tick still runs Python, which drops a frame from a session
# that stopped drawing and stamps the heartbeat the mod reads.
HITS=25
IFS= read -r -d '' input
dir=.
[[ $0 == */* ]] && dir=${0%/*}
session='"session_id"[[:space:]]*:[[:space:]]*"([^"]*)"'
# A regex match runs in C. An extglob substitution here took seconds of CPU on a 1.5 KB input in bash 3.2.
duration='^(.*)"[a-z_]*duration_ms"[[:space:]]*:[[:space:]]*[0-9.]+(.*)$'
if [[ $input =~ $session ]]; then
  f="$dir/frames/${BASH_REMATCH[1]}"
  # Durations tick every second without changing what the line shows, so they stay out of the key.
  rest=$input
  while [[ $rest =~ $duration ]]; do rest="${BASH_REMATCH[1]}${BASH_REMATCH[2]}"; done
  # The frame's own text is in the key: bash 3.2 compares file times in whole seconds, which is too
  # coarse for frames that land twice a second. read -d '' stops at end of file and reports a
  # failure there, so its status is not the test.
  frame= last= hits=0
  [[ -f $f.json ]] && IFS= read -r -d '' frame < "$f.json"
  key="$COLUMNS $rest $frame"
  [[ -f $f.in ]] && IFS= read -r -d '' last < "$f.in"
  [[ -f $f.hits ]] && read -r hits < "$f.hits"
  if [[ -f $f.out && $last == "$key" && $hits -lt $HITS ]]; then
    printf '%s' $((hits + 1)) > "$f.hits"
    IFS= read -r -d '' out < "$f.out"
    printf '%s' "$out"
    exit 0
  fi
  printf '0' > "$f.hits" 2>/dev/null
  # Each run names its own key file, so two runs at once cannot pair one's key with the other's output.
  printf '%s' "$key" > "$f.in.$$" 2>/dev/null && export BUDDY_KEY_FILE="$f.in.$$"
fi
printf '%s' "$input" | exec python3 "$dir/statusline.py"
