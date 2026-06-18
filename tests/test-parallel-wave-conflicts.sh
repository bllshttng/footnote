#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

mkdir -p "$TMP_DIR/conflict-plan" "$TMP_DIR/safe-plan"

cat > "$TMP_DIR/conflict-plan/00-INDEX.md" <<'EOF'
## Execution Strategy
```yaml
execution_mode: mixed

waves:
  - wave: 1
    mode: parallel
    tasks: [1.1, 1.2]
    reason: "Generated agent artifacts collide under .codex/agents"
```
EOF

cat > "$TMP_DIR/conflict-plan/01-phase.md" <<'EOF'
### Task 1.1: Generate one agent file
**Files:**
- Create: `.codex/agents/target.toml`

### Task 1.2: Generate a second agent file
**Files:**
- Create: `.codex/agents/reviewer.toml`
EOF

cat > "$TMP_DIR/safe-plan/00-INDEX.md" <<'EOF'
## Execution Strategy
```yaml
execution_mode: mixed

waves:
  - wave: 1
    mode: parallel
    tasks: [1.1, 1.2]
    reason: "Independent files stay parallel"
```
EOF

cat > "$TMP_DIR/safe-plan/01-phase.md" <<'EOF'
### Task 1.1: Update one provider doc
**Files:**
- Modify: `providers/codex/skills/codex-do/SKILL.md`

### Task 1.2: Update another provider doc
**Files:**
- Modify: `docs/providers/codex.md`
EOF

python3 - <<'PY' "$TMP_DIR"
from pathlib import Path
import importlib.util
import sys

tmp_dir = Path(sys.argv[1])
root = Path(".")
orchestrator_path = root / "skills/do/orchestrator.py"
spec = importlib.util.spec_from_file_location("abilities_orchestrator", orchestrator_path)
module = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(module)

conflict_strategy = module.parse_execution_strategy(str(tmp_dir / "conflict-plan" / "00-INDEX.md"))
safe_strategy = module.parse_execution_strategy(str(tmp_dir / "safe-plan" / "00-INDEX.md"))

conflict_decision = module.resolve_wave_execution_mode(conflict_strategy.waves[0], str(tmp_dir / "conflict-plan"), "codex")
safe_decision = module.resolve_wave_execution_mode(safe_strategy.waves[0], str(tmp_dir / "safe-plan"), "codex")

if conflict_decision["effective_mode"] != "sequential":
    raise SystemExit(f"expected hidden output conflict to downgrade wave, got {conflict_decision}")
if ".codex/agents" not in conflict_decision["conflicts"]["shared_output_conflicts"]:
    raise SystemExit(f"expected .codex/agents shared output conflict, got {conflict_decision}")
if safe_decision["effective_mode"] != "parallel":
    raise SystemExit(f"expected safe wave to stay parallel, got {safe_decision}")

# Gemini cannot spawn concurrent Task-tool subagents, so a conflict-free
# parallel wave still downgrades to sequential main-thread (codex review, PR #426).
gemini_decision = module.resolve_wave_execution_mode(safe_strategy.waves[0], str(tmp_dir / "safe-plan"), "gemini")
if gemini_decision["effective_mode"] != "sequential":
    raise SystemExit(f"expected gemini to downgrade conflict-free parallel wave to sequential, got {gemini_decision}")
if gemini_decision["dispatch"] != "main-thread":
    raise SystemExit(f"expected gemini dispatch to be main-thread, got {gemini_decision}")

print("Parallel wave conflict validation passed")
PY
