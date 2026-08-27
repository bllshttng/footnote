#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

cat > "$TMP_DIR/conflict-plan.md" <<'EOF'
## Execution Strategy
```yaml
execution_mode: mixed

waves:
  - wave: 1
    mode: parallel
    tasks: [1.1, 1.2]
    reason: "Generated agent artifacts collide under .codex/agents"
```

### Task 1.1: Generate one agent file
**Files:**
- Create: `.codex/agents/target.toml`

### Task 1.2: Generate a second agent file
**Files:**
- Create: `.codex/agents/reviewer.toml`
EOF

cat > "$TMP_DIR/safe-plan.md" <<'EOF'
## Execution Strategy
```yaml
execution_mode: mixed

waves:
  - wave: 1
    mode: parallel
    tasks: [1.1, 1.2]
    reason: "Independent files stay parallel"
```

### Task 1.1: Update one provider doc
**Files:**
- Modify: `providers/codex/skills/codex-do/SKILL.md`

### Task 1.2: Update another provider doc
**Files:**
- Modify: `docs/harnesses/codex.md`
EOF

# The conflict detection now imports fno.graph.collision, so the probe needs
# an interpreter with the cli package's deps. Prefer a venv next to the
# checkout (or the canonical checkout this worktree branches from); fall back
# to python3 for CI, where deps are installed into the environment.
PY=python3
for candidate in "$ROOT_DIR/cli/.venv/bin/python" \
    "$(git -C "$ROOT_DIR" rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's#/\.git$##')/cli/.venv/bin/python"; do
    if [[ -x "$candidate" ]]; then
        PY="$candidate"
        break
    fi
done

"$PY" - <<'PY' "$TMP_DIR"
from pathlib import Path
import importlib.util
import sys

tmp_dir = Path(sys.argv[1])
root = Path(".")
orchestrator_path = root / "skills/execute/orchestrator.py"
spec = importlib.util.spec_from_file_location("fno_orchestrator", orchestrator_path)
module = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(module)

conflict_strategy = module.parse_execution_strategy(str(tmp_dir / "conflict-plan.md"))
safe_strategy = module.parse_execution_strategy(str(tmp_dir / "safe-plan.md"))

conflict_decision = module.resolve_wave_execution_mode(conflict_strategy.waves[0], str(tmp_dir / "conflict-plan.md"), "codex")
safe_decision = module.resolve_wave_execution_mode(safe_strategy.waves[0], str(tmp_dir / "safe-plan.md"), "codex")

# The wave stays parallel on a conflict; partition edges serialize the
# overlapping tasks instead of downgrading the whole wave.
if conflict_decision["effective_mode"] != "parallel":
    raise SystemExit(f"expected conflicted wave to stay parallel, got {conflict_decision}")
if conflict_decision["dispatch"] != "native-subagents":
    raise SystemExit(f"expected conflicted wave to keep subagent dispatch, got {conflict_decision}")
if ["1.1", "1.2"] not in conflict_decision["conflicts"]["groups"]:
    raise SystemExit(f"expected one partition group holding both tasks, got {conflict_decision}")
if ".codex/agents" not in conflict_decision["conflicts"]["shared_output_conflicts"]:
    raise SystemExit(f"expected .codex/agents shared output conflict, got {conflict_decision}")
edges = module.partition_edges(str(tmp_dir / "conflict-plan.md"), conflict_strategy.waves[0])
if edges != {"1.2": ["1.1"]}:
    raise SystemExit(f"expected 1.2 to serialize behind 1.1, got {edges}")
if safe_decision["effective_mode"] != "parallel":
    raise SystemExit(f"expected safe wave to stay parallel, got {safe_decision}")

# Gemini cannot spawn concurrent Task-tool subagents, so a conflict-free
# parallel wave still downgrades to sequential main-thread (codex review, PR #426).
gemini_decision = module.resolve_wave_execution_mode(safe_strategy.waves[0], str(tmp_dir / "safe-plan.md"), "gemini")
if gemini_decision["effective_mode"] != "sequential":
    raise SystemExit(f"expected gemini to downgrade conflict-free parallel wave to sequential, got {gemini_decision}")
if gemini_decision["dispatch"] != "main-thread":
    raise SystemExit(f"expected gemini dispatch to be main-thread, got {gemini_decision}")

print("Parallel wave conflict validation passed")
PY
