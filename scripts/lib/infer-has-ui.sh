#!/usr/bin/env bash
# infer-has-ui.sh - derive has_ui from a changeset (file list on stdin).
#
# Echoes "true" if any path matches the locked frontend surface inference
# list, else "false". Delegates to the in-package module fno.executor._surface
# (the SINGLE source of truth, ported from the retired infer-task-executor.sh)
# via its --has-ui mode so has_ui inference and executor routing share the SAME
# locked patterns and can never drift (the bug this fixes: target init
# defaulted has_ui:false on the M profile even for obvious UI surfaces, so the
# browser/frontend-craft gates were silently skipped).
#
# Usage:
#   git diff --name-only main...HEAD | infer-has-ui.sh   # -> true | false
#   printf '%s\n' src/components/Foo.tsx | infer-has-ui.sh   # -> true

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# In a checkout, point PYTHONPATH at cli/src so `python3 -m fno.executor._surface`
# imports pre-install; otherwise rely on the installed `fno` package.
_FNO_PKG_SRC="$(cd "$SCRIPT_DIR/../.." 2>/dev/null && pwd)/cli/src"
if [[ -f "${_FNO_PKG_SRC}/fno/executor/_surface.py" ]]; then
    export PYTHONPATH="${_FNO_PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
fi

# --has-ui mode reads the same newline file list on stdin and echoes
# true/false, keeping this a stdin -> true/false filter with identical output.
python3 -m fno.executor._surface --has-ui
