#!/usr/bin/env bash
# Verify that production mux process creation stays behind the admission seam.
# Embedded cfg(test) modules are excluded after their marker, so test fixtures
# cannot make the production inventory look covered.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --root)
            [[ $# -ge 2 ]] || { echo "--root needs a path" >&2; exit 2; }
            ROOT="$2"
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

SOURCE_ROOT="$ROOT/crates/fno/src"
[[ -d "$SOURCE_ROOT" ]] || {
    echo "process-admission coverage: production source inventory unavailable: $SOURCE_ROOT" >&2
    exit 1
}

exec python3 - "$ROOT" <<'PY'
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
source_root = root / "crates" / "fno" / "src"
files = sorted(source_root.rglob("*.rs"))
if not files:
    print("process-admission coverage: production source inventory empty", file=sys.stderr)
    raise SystemExit(1)

launch = re.compile(r"(?:Command::new\s*\(|tokio::process::Command::new\s*\(|\.exec\s*\()")
pty_spawn = re.compile(r"spawn_command\s*\(")

def brace_delta(line: str) -> int:
    # Test-only code is excluded by item boundaries. Rust strings in those
    # boundaries are not production inventory, so a conservative brace count
    # is sufficient and keeps this gate dependency-free.
    return line.count("{") - line.count("}")

inspected = 0
bypasses = []
for path in files:
    lines = path.read_text(encoding="utf-8").splitlines()
    pending_test_item = False
    skip_depth = 0
    for number, line in enumerate(lines, 1):
        stripped = line.strip()
        if skip_depth:
            skip_depth += brace_delta(line)
            if skip_depth <= 0:
                skip_depth = 0
            continue
        if pending_test_item:
            if not stripped or stripped.startswith("//"):
                continue
            pending_test_item = False
            depth = brace_delta(line)
            if depth > 0:
                skip_depth = depth
            continue
        if re.match(r"^#\[cfg\(test\)\]$", stripped):
            pending_test_item = True
            continue
        if not stripped:
            continue
        inspected += 1
        violation = bool(launch.search(line))
        if pty_spawn.search(line) and not path.name == "pty.rs":
            violation = True
        if path.name == "process_admission.rs":
            violation = False
        if violation:
            bypasses.append(f"[bypass] {path.relative_to(root)}:{number}: {line}")

if inspected <= 0:
    print("process-admission coverage: production inventory inspected no lines", file=sys.stderr)
    raise SystemExit(1)
if bypasses:
    print(f"process-admission coverage: inspected={inspected} bypasses={len(bypasses)}", file=sys.stderr)
    print("\n".join(bypasses), file=sys.stderr)
    raise SystemExit(1)
print(f"process-admission coverage: inspected={inspected} bypasses=0")
PY
