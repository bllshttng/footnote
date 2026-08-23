#!/usr/bin/env bash
# check-skill-limitations.sh - every shipped skill discloses a real boundary.
#
# The section is deliberately a disclosure contract, not a checkbox: a missing
# section, an empty section, or a placeholder such as "None known" is a failure
# that names the file and the text that made it unverifiable.
#
# Run: bash scripts/ci/check-skill-limitations.sh
# Self-test: bash scripts/ci/check-skill-limitations.sh --selftest DIR

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SCAN_ROOT="${REPO_ROOT}/skills"
SELFTEST_DIR=""

usage() {
    sed -n '2,12p' "${BASH_SOURCE[0]}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --root)
            [[ $# -ge 2 ]] || { echo "check-skill-limitations: --root needs a path" >&2; exit 2; }
            SCAN_ROOT="$2"
            shift 2
            ;;
        --selftest)
            [[ $# -ge 2 ]] || { echo "check-skill-limitations: --selftest needs a fixture directory" >&2; exit 2; }
            SELFTEST_DIR="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "check-skill-limitations: unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

check_tree() {
    python3 - "$1" <<'PY'
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
heading = "## Known Limitations and Deferred Work"
placeholders = {
    "none",
    "none known",
    "nothing known",
    "n/a",
    "no known limitation",
    "no known limitations",
    "no known issues",
    "nothing to disclose",
    "todo",
    "tbd",
}


def normalize_placeholder(text):
    normalized = re.sub(r"\s+", " ", text.strip().casefold())
    return re.sub(r"[.!?]+$", "", normalized)

if not root.is_dir():
    print(f"check-skill-limitations: skill directory is missing: {root}", file=sys.stderr)
    raise SystemExit(1)

direct_file = root / "SKILL.md"
skill_dirs = [root] if direct_file.is_file() else sorted(
    path for path in root.iterdir() if path.is_dir() and (path / "SKILL.md").is_file()
)
if not skill_dirs:
    print(f"check-skill-limitations: no skill files found under {root}", file=sys.stderr)
    raise SystemExit(1)

failures = []
def validate(path, require_pointer):
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        failures.append((path, f"could not read file: {exc}"))
        return

    matches = [index for index, line in enumerate(lines) if line.rstrip() == heading]
    if not matches:
        failures.append((path, f"missing verbatim heading {heading!r}"))
        return
    if len(matches) > 1:
        failures.append((path, f"heading appears {len(matches)} times; keep one disclosure section"))
        return

    start = matches[0] + 1
    end = next((index for index in range(start, len(lines)) if re.match(r"^##\s+", lines[index])), len(lines))
    body = lines[start:end]
    bullets = []
    for line in body:
        match = re.match(r"^\s*[-*+]\s+(.+?)\s*$", line)
        if match:
            bullets.append(match.group(1))

    placeholder_hits = [text for text in bullets if normalize_placeholder(text) in placeholders]
    if placeholder_hits:
        failures.append((path, f"placeholder text is not a disclosure: {placeholder_hits[0]!r}"))
    real_bullets = [text for text in bullets if text.strip() and normalize_placeholder(text) not in placeholders]
    if not real_bullets:
        failures.append((path, "section has no real bullet"))
    if require_pointer and not any("LIMITATIONS.md" in text for text in real_bullets):
        failures.append((path, "section must point to sibling LIMITATIONS.md"))

for skill_dir in skill_dirs:
    skill_path = skill_dir / "SKILL.md"
    limitations_path = skill_dir / "LIMITATIONS.md"
    validate(skill_path, require_pointer=True)
    if limitations_path.is_file():
        validate(limitations_path, require_pointer=False)
    else:
        failures.append((limitations_path, "sibling limitations file is missing"))

if failures:
    print(f"check-skill-limitations: {len(failures)} failure(s)", file=sys.stderr)
    for path, reason in failures:
        print(f"  {path}: {reason}", file=sys.stderr)
    raise SystemExit(1)

print(f"check-skill-limitations: {len(skill_dirs)} skill file(s) passed")
PY
}

run_selftest() {
    local fixture_dir="$1"
    local failures=0 output rc

    [[ -d "$fixture_dir" ]] || {
        echo "check-skill-limitations selftest: fixture directory missing: $fixture_dir" >&2
        return 1
    }

    run_case() {
        local name="$1" expected_rc="$2" marker="$3"
        if output=$(bash "${BASH_SOURCE[0]}" --root "${fixture_dir}/${name}" 2>&1); then
            rc=0
        else
            rc=$?
        fi
        if [[ "$rc" != "$expected_rc" ]]; then
            echo "  FAIL: ${name}: expected exit ${expected_rc}, got ${rc}" >&2
            printf '%s\n' "$output" >&2
            failures=$((failures + 1))
        elif [[ "$output" != *"$marker"* ]]; then
            echo "  FAIL: ${name}: output did not name ${marker}" >&2
            printf '%s\n' "$output" >&2
            failures=$((failures + 1))
        else
            echo "  ok: ${name} (exit ${rc}, marker ${marker})"
        fi
    }

    echo "check-skill-limitations selftest:"
    run_case missing 1 "missing/SKILL.md"
    run_case empty 1 "no real bullet"
    run_case placeholder 1 "None known"
    run_case no-known 1 "No known limitations."
    run_case todo 1 "TODO"
    run_case valid 0 "1 skill file(s) passed"

    if [[ "$failures" -ne 0 ]]; then
        echo "selftest FAILED (${failures} case(s))" >&2
        return 1
    fi
    echo "selftest OK"
}

if [[ -n "$SELFTEST_DIR" ]]; then
    run_selftest "$SELFTEST_DIR"
else
    check_tree "$SCAN_ROOT"
fi
