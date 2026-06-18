#!/usr/bin/env bash
# Warn when a gate artifact body exceeds the substituted-executor budget.
# Soft warning only - stop hook does NOT block on this.
#
# Usage: bash check-artifact-body.sh <artifact-path>
# Exits 0 always. Emits a single-line stderr WARN when body > 500 bytes.
#
# See skills/target/references/gate-artifacts.md section
# "Body content when an LLM writes the artifact" for the convention.

set -euo pipefail

ARTIFACT="${1:-}"
if [[ -z "$ARTIFACT" || ! -r "$ARTIFACT" ]]; then
    exit 0
fi

# Strip frontmatter (everything between the first two `---` lines plus
# the lines themselves), then count both body bytes and paragraphs
# (blank-line-separated chunks of non-blank lines). The convention pins
# both: <=500 bytes AND <=3 single-line paragraphs.
read -r BODY_BYTES BODY_PARAGRAPHS < <(awk '
    BEGIN { in_fm = 0; seen_fm = 0; in_para = 0; paras = 0; bytes = 0 }
    /^---$/ {
        if (!seen_fm) { in_fm = 1; seen_fm = 1; next }
        if (in_fm) { in_fm = 0; next }
    }
    in_fm { next }
    {
        bytes += length($0) + 1  # +1 for the newline awk strips
        if ($0 ~ /^[[:space:]]*$/) {
            in_para = 0
        } else if (!in_para) {
            paras++
            in_para = 1
        }
    }
    END { print bytes, paras }
' "$ARTIFACT")

BYTE_LIMIT=500
PARA_LIMIT=3
REF="skills/target/references/gate-artifacts.md 'Body content when an LLM writes the artifact'"

if [[ "${BODY_BYTES:-0}" -gt "$BYTE_LIMIT" ]]; then
    echo "target: WARN: artifact body $BODY_BYTES bytes > $BYTE_LIMIT byte budget at $ARTIFACT (see $REF)" >&2
fi

if [[ "${BODY_PARAGRAPHS:-0}" -gt "$PARA_LIMIT" ]]; then
    echo "target: WARN: artifact body $BODY_PARAGRAPHS paragraphs > $PARA_LIMIT paragraph budget at $ARTIFACT (see $REF)" >&2
fi
