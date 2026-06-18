#!/usr/bin/env bash
# disk-space.sh - Check available disk space in $HOME partition
# Contract: stdout one line "disk-space {pass|fail|warn|unknown} {message}"
# Exit: always 0
# Thresholds: <1GB -> fail, <5GB -> warn, >=5GB -> pass

set -euo pipefail

# Get free space in bytes using df
# macOS: df -k outputs 1K blocks; Linux: df -B1 outputs bytes
if [[ "$(uname)" == "Darwin" ]]; then
    # macOS: df -k gives 1024-byte blocks
    # -P forces POSIX one-line-per-filesystem output, safer for awk parsing
    # when the filesystem name is long enough to wrap to a second line.
    AVAIL_KB=$(df -kP "$HOME" 2>/dev/null | awk 'NR==2 {print $4}' || echo "0")
    AVAIL_BYTES=$((AVAIL_KB * 1024))
else
    # Linux: df --output=avail -B1 gives bytes
    AVAIL_BYTES=$(df -B1 --output=avail "$HOME" 2>/dev/null | tail -1 | tr -d ' ' || echo "0")
fi

if [[ -z "$AVAIL_BYTES" || "$AVAIL_BYTES" -eq 0 ]]; then
    echo "disk-space unknown could not determine disk space"
    exit 0
fi

# Convert to human-readable GB
AVAIL_GB=$(echo "scale=1; $AVAIL_BYTES / 1073741824" | bc 2>/dev/null || echo "?")

GB_1=1073741824   # 1 GB in bytes
GB_5=5368709120   # 5 GB in bytes

if [[ $AVAIL_BYTES -lt $GB_1 ]]; then
    echo "disk-space fail only ${AVAIL_GB}GB free in \$HOME - free space before running target"
elif [[ $AVAIL_BYTES -lt $GB_5 ]]; then
    echo "disk-space warn only ${AVAIL_GB}GB free in \$HOME (recommend >=5GB)"
else
    echo "disk-space pass ${AVAIL_GB}GB free in \$HOME"
fi
exit 0
