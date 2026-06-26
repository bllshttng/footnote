#!/bin/bash
# Exec shim over `fno-agents loop run --driver target` (step 5, ab-781b6d17).
# The legacy 466-line bash loop moved into the Rust loop runtime
# (crates/fno-agents/src/loop_target.rs). This shim maps the documented
# legacy flags onto the new verb and execs the binary - no loop logic here.
# Resume after a crash is world-state driven: the Rust verb re-reads the
# events journal and refuses to double-dispatch a terminated session.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<EOF
Usage: run-target-loop.sh [OPTIONS]

Exec shim over: fno-agents loop run --driver target [options]

Options (mapped onto the Rust verb):
  --driver <name>          Dispatcher: claude-code (default) | hermes | openclaw | opencode
                           (maps to --dispatcher; --driver target is pinned)
  --cli <claude|opencode>  Legacy CLI alias (passed through)
  --max-iterations N       Max loop iterations (alias: --max-iter)
  --max-turns N            Max turns per session (default: 15)
  --budget N               Cost cap per session in USD (default: 25)
  --model NAME             Force a specific model (if driver supports it)
  --prompt-file PATH       First-iteration prompt (non-CC drivers)
  -h, --help               Show this help

Binary resolution order: \$FNO_AGENTS_BIN, repo crates/fno-agents/target/
{release,debug}, then PATH. Exit codes come from fno-agents loop run:
0 done, 1 budget/no-progress, 2 misuse, 77 driver binary missing, 130 SIGINT.
EOF
}

DISPATCHER="${FNO_DRIVER:-claude-code}"
PASS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --driver)
      case "${2:-}" in
        claude-code|hermes|openclaw|opencode) DISPATCHER="$2" ;;
        *) echo "run-target-loop.sh: unknown --driver '${2:-}' (expected claude-code | hermes | openclaw | opencode)" >&2
           exit 2 ;;
      esac
      shift 2 ;;
    --max-iterations|--max-iter) PASS+=(--max-iterations "${2:?$1 needs a value}"); shift 2 ;;
    --cli|--max-turns|--budget|--model|--prompt-file) PASS+=("$1" "${2:?$1 needs a value}"); shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "run-target-loop.sh: unknown flag '$1'" >&2
      echo "The bash loop moved to 'fno-agents loop run' (step 5, ab-781b6d17);" >&2
      echo "this shim maps only the documented legacy flags. See --help." >&2
      exit 2 ;;
  esac
done

# Binary resolution per grilled 8 (same order as hooks/target-stop-hook.sh).
BIN=""
for c in "${FNO_AGENTS_BIN:-}" \
         "$SCRIPT_DIR/../crates/fno-agents/target/release/fno-agents" \
         "$SCRIPT_DIR/../crates/fno-agents/target/debug/fno-agents"; do
  [[ -n "$c" && -x "$c" ]] && { BIN="$c"; break; }
done
[[ -z "$BIN" ]] && BIN="$(command -v fno-agents 2>/dev/null || true)"
if [[ -z "$BIN" ]]; then
  echo "run-target-loop.sh: fno-agents binary not found." >&2
  echo "Checked: \$FNO_AGENTS_BIN, crates/fno-agents/target/{release,debug}, PATH." >&2
  echo "Build it: (cd crates/fno-agents && cargo build --release) or set FNO_AGENTS_BIN." >&2
  exit 2
fi

exec "$BIN" loop run --driver target --dispatcher "$DISPATCHER" \
  --driver-lib-dir "$SCRIPT_DIR/lib" --cwd "$PWD" \
  ${PASS[@]+"${PASS[@]}"}
