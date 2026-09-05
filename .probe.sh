#!/usr/bin/env bash
set -u
cd /Users/bb16/.fno/worktrees/footnote/x-9223-port/cli
git -C .. add -A
git -C .. commit --quiet -m "fix(claims): widen the probe annotation to the kwarg contract"
uv run pytest tests/unit/test_claim_reap.py tests/unit/test_claim_closure_release.py tests/unit/test_claim_verdict.py tests/unit/test_advance.py tests/unit/test_lane_dispatch.py tests/unit/test_reconcile_dispatch.py tests/unit/test_decide.py tests/unit/test_spawn_guard.py -x -q 2>&1 | tail -5
