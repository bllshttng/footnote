#!/usr/bin/env bash
# Anti-pattern scanner for target
# Usage: scan-antipatterns.sh [directory]
# Exit: 0 = clean, 1 = issues found

set -euo pipefail

TARGET="${1:-.}"
ISSUES=0

scan() {
    local severity="$1"
    local label="$2"
    local pattern="$3"

    local results
    results=$(grep -rnE "$pattern" "$TARGET" \
        --include="*.ts" --include="*.tsx" \
        --include="*.js" --include="*.jsx" \
        --include="*.py" --include="*.sh" \
        --exclude-dir=node_modules --exclude-dir=.git \
        --exclude-dir=dist --exclude-dir=__pycache__ \
        --exclude-dir=.next --exclude-dir=coverage \
        --exclude="*.test.*" --exclude="*.spec.*" \
        2>/dev/null || true)

    if [[ -n "$results" ]]; then
        echo "[$severity] $label:"
        echo "$results" | head -20
        echo ""
        local count
        count=$(echo "$results" | wc -l | tr -d ' ')
        ((ISSUES += count)) || true
    fi
}

echo "Anti-Pattern Scan: $TARGET"
echo "=========================="
echo ""

# AC1: TODO/FIXME/HACK/XXX comments
scan "WARN"  "TODO/FIXME/HACK/XXX comments"   "(TODO|FIXME|HACK|XXX)[: ]"

# AC2: Stub return patterns
scan "ERROR" "Stub null/undefined returns"    "return (null|undefined)\s*;?\s*$"
scan "ERROR" "Stub empty object/array returns" "return (\{\}|\[\])\s*;?\s*$"
scan "ERROR" "Not implemented throws"          "throw.*[Nn]ot.?[Ii]mplemented"
scan "ERROR" "Empty function bodies"           "^\s*(async\s+)?function\s+\w+[^{]*\{\s*\}"

# AC3: Hardcoded URLs and potential secrets
scan "ERROR" "Hardcoded localhost URLs"        "https?://localhost:[0-9]+"
scan "ERROR" "Potential hardcoded secrets"     "(api[_-]?key|secret|password)\s*[:=]\s*['\"][^'\"]{4,}['\"]"

# Bonus: console.log left in production code (WARN only)
scan "WARN"  "Console.log in production code"  "console\.(log|debug|info)\("

echo "=========================="
echo "Issues found: $ISSUES"

if [[ $ISSUES -gt 0 ]]; then
    exit 1
fi

echo "Clean!"
exit 0
