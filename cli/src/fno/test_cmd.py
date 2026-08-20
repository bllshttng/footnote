"""`fno test [pytest-args...]` / `fno test rust [cargo-args...]` - run a suite
honestly AND tersely.

Footguns this verb encodes (tribal knowledge made runtime behavior):

1. A worktree's bare `pytest` imports the *canonical* `fno` (installed editable
   from the main checkout), not the worktree's own source. We pin
   `PYTHONPATH=<repo>/cli/src` so a worktree tests what it changed.
2. rtk rewrites a bare `pytest`/`cargo` into a wrapped run that has stalled for
   12+ minutes with zero output. `fno test` spawns the runner directly and sets
   `RTK_DISABLED=1` in the child env so nothing re-wraps it.
3. `... | tail && echo OK` masks the real exit code (false green). We propagate
   the child's *actual* return code.
4. Full test output in an agent transcript is re-read by every later request in
   the session. Default mode therefore captures ALL output to
   `<repo>/.fno/last-test.log` and prints only a summary: on failure, the TAIL
   of the log (errors live at the end - read from the end, expand upward via
   the log path). `--stream` restores inherited stdio for interactive runs.

The interpreter is resolved worktree-venv -> canonical-venv -> the running
interpreter, so a fresh worktree with no local `.venv` still runs.

Exposed as a `click.Command` (not a plain Typer function) so the lazy group
uses it verbatim, giving `UNPROCESSED` passthrough: every pytest flag (`-x`,
`-k expr`, `::nodeid`) flows through untouched.
"""
from __future__ import annotations

import fnmatch
import atexit
import json
import os
import posixpath
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections import deque
from pathlib import Path
from typing import Mapping, Optional, Sequence

import click

from fno.hermetic import neutralise, poison

_TAIL_LINES = 40

# Which ambient shape the child processes run under. `clean` is today's
# behaviour and the merge gate; `dirty` feeds a poisoned PARENT through
# neutralise() so a channel the surface missed survives into the child and
# reddens the test that reads it; `both` runs the two in sequence and names the
# lane that differed. See docs/architecture/test-hermeticity.md.
_AMBIENT_MODES = ("clean", "dirty", "both")
_AMBIENT_MODE = "clean"

_SANDBOX: Optional[Path] = None


def _sandbox() -> Path:
    """One throwaway state root per `fno test` process, reaped at exit."""
    global _SANDBOX
    if _SANDBOX is None:
        _SANDBOX = Path(tempfile.mkdtemp(prefix="fno-test-sandbox-"))
        atexit.register(shutil.rmtree, str(_SANDBOX), True)
    return _SANDBOX


def _poison_fixtures(root: Path) -> Path:
    return root / "cli" / "tests" / "fixtures" / "ambient-poison"


def _repo_root(start: Path) -> Optional[Path]:
    """Walk up from `start` to the checkout root (the dir with cli/src/fno)."""
    for d in (start, *start.parents):
        if (d / "cli" / "src" / "fno" / "__init__.py").exists():
            return d
    return None


def _canonical_root() -> Optional[Path]:
    """The main checkout root: parent of git's common .git dir.

    In a worktree, `git rev-parse --git-common-dir` points at the MAIN repo's
    `.git`, so its parent is the canonical checkout (whose `cli/.venv` a
    worktree shares deps with). In the main checkout it is just `.git`.
    """
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"],
            capture_output=True,
            text=True,
            check=False,
        )
    except FileNotFoundError:
        return None
    if out.returncode != 0 or not out.stdout.strip():
        return None
    common = Path(out.stdout.strip())
    if not common.is_absolute():
        common = (Path.cwd() / common).resolve()
    return common.parent


def _resolve_interpreter(root: Path) -> str:
    """worktree venv -> canonical venv -> the interpreter running fno."""
    local = root / "cli" / ".venv" / "bin" / "python"
    if local.exists():
        return str(local)
    canon = _canonical_root()
    if canon is not None:
        cand = canon / "cli" / ".venv" / "bin" / "python"
        if cand.exists():
            return str(cand)
    return sys.executable


def _log_path(root: Path) -> Path:
    d = root / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    # ponytail: one shared file per checkout, last-writer-wins; per-run files
    # if parallel same-worktree runs ever matter.
    return d / "last-test.log"


def _tail(path: Path, n: int) -> list[str]:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            return list(deque(fh, maxlen=n))
    except OSError:
        return []


def _child_env(root: Path) -> dict:
    """The environment every test child gets.

    This is the ONE process boundary all four test trees cross - pytest over
    `cli/tests`, pytest over `cli/src`, every `bash tests/*.sh` smoke step, and
    cargo - so neutralising here needs no shell mirror and no Rust mirror to
    drift out of sync with it.

    Under `--ambient dirty` the parent is poisoned FIRST and then neutralised.
    If the ambient surface is complete the result is identical to the clean
    lane; a name the surface missed survives into this dict and the test that
    reads it fails naming itself.
    """
    parent: Mapping[str, str] = os.environ
    if _AMBIENT_MODE == "dirty":
        parent = poison(os.environ, _poison_fixtures(root))
    env = neutralise(parent, _sandbox())
    src = str((root / "cli" / "src").resolve())
    existing = env.get("PYTHONPATH")
    env["PYTHONPATH"] = src + (os.pathsep + existing if existing else "")
    env["RTK_DISABLED"] = "1"  # never let rtk re-wrap the child run
    return env


def _run_captured(cmds: Sequence[Sequence[str]], env: dict, log: Path) -> int:
    """Run each command with output captured to `log`; print the terse verdict.

    The header (command + log path) prints BEFORE the run so a long suite never
    looks stalled - a watcher can `tail -f` the log. Returns the first non-zero
    child exit code, else 0.
    """
    rc = 0
    with open(log, "w", encoding="utf-8") as fh:
        for cmd in cmds:
            print(f"running: {' '.join(map(str, cmd))} | log: {log}", flush=True)
            fh.write(f"$ {' '.join(map(str, cmd))}\n")
            fh.flush()
            try:
                proc = subprocess.run(cmd, env=env, stdout=fh, stderr=subprocess.STDOUT)
            except OSError as exc:
                sys.stderr.write(f"fno test: failed to run {cmd[0]}: {exc}\n")
                return 127
            if proc.returncode != 0:
                rc = proc.returncode
                break  # first failure wins; its output is the log tail
    if rc == 0:
        lines = [ln.rstrip() for ln in _tail(log, 5) if ln.strip()]
        summary = lines[-1] if lines else "(no output)"
        print(f"PASS | {summary}")
    else:
        print(
            f"FAIL (rc={rc}) - last {_TAIL_LINES} lines below; read from the end, "
            f"expand upward if needed: tail -100 {log}"
        )
        sys.stdout.write("".join(_tail(log, _TAIL_LINES)))
    return rc


def _run(args: Sequence[str], stream: bool = False) -> int:
    """Resolve interpreter + env, run pytest, return its real exit code."""
    root = _repo_root(Path.cwd()) or Path.cwd()
    interp = _resolve_interpreter(root)

    # Default to THE Python suite (cli/tests) when no collection target was
    # given. A bare `pytest` collects from cwd, which from the repo root pulls
    # in script-style tests/ files that `raise SystemExit` at import (pytest
    # INTERNALERROR, not a real run). A collection target is a non-flag arg
    # that names a path or nodeid; a flag value like `-k expr` does not count.
    pytest_args = list(args)
    has_target = any(
        (not a.startswith("-")) and ("::" in a or "/" in a or Path(a).exists())
        for a in pytest_args
    )
    if not has_target:
        pytest_args = [
            "-n",
            "auto",
            "--maxprocesses=4",
            "--dist=loadgroup",
            *pytest_args,
        ]
        pytest_args.append(str((root / "cli" / "tests").resolve()))

    env = _child_env(root)
    cmd = [interp, "-m", "pytest", *pytest_args]
    if stream:
        try:
            proc = subprocess.run(cmd, env=env)  # inherit stdio; no pipe, no mask
        except OSError as exc:
            # FileNotFoundError (missing) AND PermissionError (present but not
            # executable) are both OSError; either means we could not run it.
            sys.stderr.write(f"fno test: failed to run interpreter {interp}: {exc}\n")
            return 127
        return proc.returncode
    return _run_captured([cmd], env, _log_path(root))


def _run_rust(args: Sequence[str], stream: bool = False) -> int:
    """Run the Rust suites: nextest when installed, else `cargo test -q`.

    No workspace root exists, so without an explicit `--manifest-path` we sweep
    every `crates/*/Cargo.toml` (the two-test-trees lesson: a green subset is
    not proof).
    """
    root = _repo_root(Path.cwd()) or Path.cwd()
    cargo_args = list(args)
    if shutil.which("cargo-nextest"):
        base = ["cargo", "nextest", "run"]
    else:
        base = ["cargo", "test", "-q"]

    if "--manifest-path" in cargo_args:
        cmds = [[*base, *cargo_args]]
    else:
        manifests = sorted((root / "crates").glob("*/Cargo.toml"))
        if not manifests:
            sys.stderr.write(f"fno test rust: no crates/*/Cargo.toml under {root}\n")
            return 2
        cmds = [[*base, "--manifest-path", str(m), *cargo_args] for m in manifests]

    env = _child_env(root)
    if stream:
        rc = 0
        for cmd in cmds:
            try:
                proc = subprocess.run(cmd, env=env)
            except OSError as exc:
                sys.stderr.write(f"fno test: failed to run {cmd[0]}: {exc}\n")
                return 127
            if proc.returncode != 0:
                return proc.returncode
        return rc
    return _run_captured(cmds, env, _log_path(root))


# ---------------------------------------------------------------------------
# fno test smoke - the one authoritative Python + shell smoke entry.
#
# Replaces the hand-edited scripts/ci/smoke.sh registry: the structural steps
# below are the code-owned translation of that file's ~45 step() lines, and
# discover_shell_harnesses() auto-discovers the owned shell-harness trees so a
# new tests/hooks/test_x.sh runs with zero registry edits (the two-test-trees
# lesson: a green subset is not proof that everything ran).
#
# Modes mirror the retired smoke.sh: --keep-going / --only / --retry-failed /
# --list, the FULL vs RETRY-SUBSET vs ONLY-SUBSET header, and the failure
# record. A partial green never satisfies preflight's mode=FULL verdict=green
# attestation (preflight gates that on its own RETRY_FAILED flag, independent
# of this runner's honest exit code).
# ---------------------------------------------------------------------------

SMOKE_FAILURE_RECORD_DEFAULT = ".fno/preflight-last-failures.txt"

# (name, cwd, cwd, cmd). cwd is repo-relative ("." = root). A multi-line cmd
# runs under `bash -eo pipefail -c`, matching GitHub's default shell.
_STRUCTURAL_STEPS: tuple[tuple[str, str, str], ...] = (
    ("Sync + build", "cli", "uv sync\nuv build"),
    (
        "Pytest (unit + integration)",
        "cli",
        # The canary is EXPECTED to fail under --ambient dirty; that failure is
        # the positive control, not a defect. Running it as part of the ordinary
        # suite would make this job red on every commit, and a job that is
        # always red teaches people to ignore it exactly as fast as one that is
        # always green. It is exercised deliberately, in BOTH lanes, by
        # tests/ci/test_hermetic_lanes.sh, which is where the assertion belongs.
        "uv run pytest --tb=short -q -n auto --maxprocesses=4 --dist=loadgroup "
        "--ignore=tests/unit/test_ambient_canary.py",
    ),
    ("paths.sh hash gate", "cli", "uv run fno-py paths verify ../scripts/lib/paths.sh"),
    ("Bash events-validate harness", ".", "bash tests/events/test-bash-validator.sh"),
    ("frontend-craft gate harness", ".",
     "bash tests/lib/test_frontend_surface.sh\n"
     "bash tests/lib/test_infer_has_ui.sh\n"
     "bash tests/lib/test_resolve_plan_executor.sh"),
    ("config global-precedence harness", ".", "bash tests/lib/test_config_global_precedence.sh"),
    ("cost-accuracy harness", ".",
     "uv run --project cli python tests/lib/test_cost_tracker_pricing.py\n"
     "uv run --project cli python tests/metrics/test_session_cost_dedup.py\n"
     "uv run --project cli python tests/metrics/test_backfill_cost_recompute.py\n"
     "bash tests/lib/test_cost_tracker_sh_parity.sh"),
    ("loop-check shim + immutable manifest harness", ".",
     "bash tests/hooks/test_loop_check_shim.sh\n"
     "bash tests/hooks/test_manifest_immutable.sh\n"
     "bash tests/hooks/test_graph_write_protect.sh\n"
     "bash tests/hooks/test_worktree_write_protect.sh\n"
     "bash tests/hooks/test_plan_location_guard.sh\n"
     "bash tests/hooks/test_worktree_live_peers.sh\n"
     "bash tests/hooks/test_setup_nudge_session_start.sh\n"
     "bash tests/hooks/test_init_target_session_id.sh\n"
     "bash tests/hooks/test_agy_stop_hook.sh\n"
     "bash tests/hooks/test_check_impl_location.sh"),
    # The Python hook harnesses run NOWHERE otherwise. Discovery is .sh-only
    # (shebang-gated), and the pytest step above collects `cli/tests` alone, so
    # a test_*.py under tests/hooks/ is in neither tree - three files and 116
    # cases sat unrun, including the ones covering this very gate. Named
    # explicitly rather than globbed so adding a fourth is a visible edit.
    #
    # This closes tests/hooks/ ONLY, and the gap is wider: tests/operator/,
    # tests/spec/, tests/integration/*_bdd_invariants.py and about a dozen
    # loose tests/test_*.py files are in neither tree either. They are left out
    # deliberately, not overlooked - running them today gives 3 failed, 159
    # passed, 1 error, so wiring them in would red the build on pre-existing
    # rot that has nothing to do with the guard this PR adds. Whoever repairs
    # those files should add them here in the same change, and the honest
    # reading until then is that this step fixes one directory, not the class.
    ("Python hook harnesses (outside both test trees)", ".",
     "uv run --project cli python -m pytest -q "
     "tests/hooks/test_git_protection_push.py "
     "tests/hooks/test_merge_guard_worktree.py "
     "tests/hooks/test_merge_guard_stacked_base.py"),
    ("worktree lifecycle: remove-hook contract + cwd-anchored liveness + ttyless archive + job reap",
     ".", "bash tests/hooks/test_worktree_remove_lifecycle.sh"),
    ("in_review dispatch guard", ".", "bash tests/hooks/test_init_in_review_gate.sh"),
    ("born-with-why offer inject harness", ".", "bash tests/hooks/test_born_with_why_offer_inject.sh"),
    ("eval-sweep hygiene harness", ".", "bash tests/hooks/test_eval_sweep_session_start.sh"),
    ("ship-phase PR->node link verify guard", ".", "bash tests/target/test_ship_phase_link_verify.sh"),
    ("docs-before-ship phase-ordering guard", ".", "bash tests/test-docs-before-ship.sh"),
    (".fno/ dir-hygiene harness", ".",
     "python3 tests/metrics/test_completion_summary_path.py\n"
     "bash tests/lib/test_rotate_append_log.sh"),
    ("state-dir path gate self-test", ".",
     "bash scripts/tests/check-no-hardcoded-paths-selftest.sh"),
    ("corrections.log placement migration", ".", "bash scripts/tests/test_corrections_migrate.sh"),
    ("placement-rule lint self-test", ".", "bash scripts/tests/test_check_placement_rule.sh"),
    ("Build fno-agents debug binary (for journey tests)", "crates/fno-agents", "cargo build"),
    # The debug binary is present here, so the @requires_rust parity suites run
    # instead of skipping. Stub the provider CLIs on PATH (test_rust_verb_parity
    # presence-checks them without faking); per-test fakes still win where a
    # provider is actually executed. Reuses the build above; no second compile.
    # opencode belongs in this list for the same reason codex and gemini do:
    # the OpenCode canonical-tail case resumes through it, and a runner without
    # it fails on `opencode CLI not on PATH` rather than on any parity claim.
    ("Scoped @requires_rust parity suites (binary present; stubbed providers)", "cli",
     'FAKE_BIN="$(mktemp -d)"\n'
     'for p in codex gemini opencode; do printf "%s\\n%s\\n" "#!/bin/sh" "exit 0" > "$FAKE_BIN/$p"; chmod +x "$FAKE_BIN/$p"; done\n'
     'PATH="$FAKE_BIN:$PATH" uv run pytest --tb=short -q '
     "tests/agents/test_rust_verb_parity.py tests/agents/test_ask_e2e_dispatch.py"),
    ("registry-miss heal across the Rust/Python seam", ".", "bash tests/test-agents-heal-token.sh"),
    ("Cross-impl claims compat matrix (merge gate; fails loudly, never skips here)", "cli",
     "FNO_CLAIMS_COMPAT_REQUIRED=1 uv run pytest --tb=short -q tests/integration/test_claims_cross_impl.py"),
    ("loop-check journey tests (e2e + emission-schema + backstop-subprocess)", ".",
     "bash tests/hooks/test_loop_check_e2e.sh\n"
     "bash tests/events/test-loop-check-emission-schema.sh\n"
     "bash tests/hooks/test_loop_check_backstop_subprocess.sh"),
    ("Target self-handoff harness", ".",
     "bash tests/test-handoff.sh\n"
     "bash tests/target/test_handoff_ledger_record.sh"),
    ("Plan Mode front door harness", ".",
     "bash tests/hooks/test_capture_plan_mode.sh\n"
     "bash tests/hooks/test_pending_plan_wipe.sh\n"
     "bash tests/target/test_backfill_plan.sh\n"
     "bash tests/target/test_detect_pending_plan.sh\n"
     "bash tests/target/test_plan_mode_e2e.sh"),
    ("bg-dispatch + ready-gated auto-launch harness", ".", "bash tests/test-bg-dispatch.sh"),
    ("agent skill harness", ".",
     "bash tests/skills/test_agent_normalize.sh\n"
     "bash tests/skills/test_agent_receipt.sh\n"
     "bash skills/agent/tests/test_normalize.sh\n"
     "bash skills/agent/tests/test_confirm.sh\n"
     "bash skills/agent/tests/test_auto_worktree.sh\n"
     "bash skills/agent/tests/test_spawn_guard.sh"),
    ("mail skill harness", ".", "bash skills/mail/tests/test_normalize.sh"),
    ("events-discipline lint", ".", "bash scripts/lint/events-discipline.sh"),
    ("events-discipline lint self-test", ".", "bash tests/lint/test-events-discipline.sh"),
    ("No quarantined events.invalid.jsonl rows", ".", "bash scripts/lint/no-invalid-events.sh"),
    ("ruff + mypy (both repo-wide)", "cli",
     "uv run ruff check --no-respect-gitignore src/\n"
     "uv run mypy src/"),
    ("Smoke tests", ".", "bash cli/tests/smoke/run-all.sh"),
    ("no hardcoded paths", ".", "bash scripts/ci/check-no-hardcoded-paths.sh"),
    ("placement rule", ".", "bash scripts/ci/check-placement-rule.sh"),
    ("No residual old-name patterns (rename guard)", ".", "bash scripts/rename/residual-check.sh"),
    ("No stale skill refs (consolidation audit)", ".", "bash scripts/ci/check-no-stale-skill-refs.sh"),
    ("Company function-agnosticism check", ".", "bash scripts/ci/check-company-function-agnostic.sh"),
    ("Plugin function-agnosticism check", ".", "bash scripts/ci/check-plugins-function-agnostic.sh"),
    ("Company module boundary self-test", ".", "bash tests/ci/test_check_company_boundaries.sh"),
    ("Company module boundary check", ".", "bash scripts/ci/check-company-boundaries.sh --baseline"),
    ("Skill snippet hazard lint", ".", "bash scripts/ci/check-skill-snippets.sh"),
    ("Skill snippet lint self-test", ".", "bash tests/ci/test_check_skill_snippets.sh"),
    # Both gates measure the always-loaded SessionStart preamble. Neither could
    # FAIL anywhere but CI before this entry: `fno doctor` prints the number,
    # but advisorily, and never changes its own exit code. So the only local
    # signal was a line nobody had to act on, and an AGENTS.md edit went green
    # locally and red on push - which is how the preamble reached 55 bytes of
    # headroom with nobody watching the slope.
    ("SessionStart preamble byte budget", ".", "bash scripts/ci/check-preamble-budget.sh"),
    ("Pitfalls corpus cap", ".", "bash scripts/ci/check-pitfalls.sh"),
    # One registration, two lanes: cli-ci runs `fno-py test smoke` and so does
    # preflight, so this line is both the CI gate and the local one. A second
    # copy in guards.yml would be a duplicate implementation of one operation,
    # not extra coverage.
    ("CI gate lanes self-test", ".", "bash scripts/ci/check-ci-gate-lanes.sh --self-test"),
    ("CI gate lanes (every gate has a real invoker)", ".", "bash scripts/ci/check-ci-gate-lanes.sh"),
    ("GraphQL PR reads are routed, not raw", ".", "bash scripts/ci/check-no-direct-graphql-pr-read.sh"),
    ("No stale /spec refs (blueprint rename audit)", ".", "bash scripts/ci/check-no-stale-spec-refs.sh"),
    ("Config schema docs freshness", ".", "bash scripts/ci/check-config-schema-drift.sh"),
    ("Skill bundles freshness check", ".", "bash scripts/lint/check-skill-bundles-fresh.sh"),
    ("No ${REPO_ROOT}/scripts in skills", ".", "bash scripts/lint/no-repo-root-scripts-in-skills.sh"),
    ("Marketplace-readiness lint (no Skill calls, no path escapes, fno declared)", ".",
     "bash scripts/lint/no-cross-skill-runtime-calls.sh"),
    ("No unwrapped lib scripts", ".", "bash scripts/lint/no-unwrapped-lib-scripts.sh"),
    ("pin-skill generator self-test", ".", "bash tests/test-pin-skill.sh"),
    ("Agent flock-pattern lint", "cli", "uv run fno-py lint flock-pattern"),
    ("Provider stderr-merge lint", "cli", "uv run fno-py lint provider-stderr-merge"),
    ("Repo-root shell-out drift guard (clone-only allowlist)", "cli", "uv run fno-py lint shellout-drift"),
    ("Spawn-shape lint (single-line argv guard)", "cli", "uv run fno-py lint spawn-paths"),
    ("In-N-Out menu-cap ratchet", "cli", "uv run fno-py lint menu-caps"),
    ("Verb-surface ratchet (real count, both binaries)", "cli", "uv run fno-py lint verb-ratchet"),
    ("Schema parity self-test", ".", "bash scripts/tests/check-event-schema-parity-selftest.sh"),
    ("Schema parity check (Python side)", ".", "bash scripts/check-event-schema-parity.sh"),
    ("Registry schema parity selftest", ".", "bash scripts/ci/check-registry-schema-parity.sh --selftest"),
    ("Registry schema parity check", ".", "bash scripts/ci/check-registry-schema-parity.sh"),
    ("Reviewer descriptor parity selftest", ".",
     "bash scripts/ci/check-reviewer-descriptor-parity.sh --selftest"),
    ("Reviewer descriptor parity check", ".",
     "bash scripts/ci/check-reviewer-descriptor-parity.sh"),
    ("smoke mode machinery self-test", ".", "bash tests/ci/test_smoke_modes.sh"),
    ("hermetic lanes positive control (the dirty lane must be able to go red)", ".",
     "bash tests/ci/test_hermetic_lanes.sh"),
    ("preflight orchestration self-test", ".", "bash tests/ci/test_preflight.sh"),
    ("changed/full CI job-boundary guard", ".", "bash tests/ci/test_changed_smoke_workflow.sh"),
)

# Owned shell-harness trees: a new file here runs with zero registry edits.
_SMOKE_HARNESS_GLOBS = (
    "tests/*.sh",
    "tests/hooks/*.sh", "tests/lib/*.sh", "tests/target/*.sh",
    "tests/skills/*.sh", "tests/events/*.sh", "tests/lint/*.sh",
    "scripts/tests/*.sh",
    "skills/*/tests/*.sh",
)

_SH_PATH_RE = re.compile(r"[\w./@{}$+-]*\.sh\b")
# A source target may contain a command substitution with its own spaces
# (`source "$(dirname "$0")/x.sh"`), so the token cannot be a bare \S+ - that
# stops at the space inside $( ) and the ref is missed entirely. The alternation
# swallows a whole $(...) group but still refuses to cross unrelated whitespace,
# so `source "$X"  # see helper.sh` does not read as a ref to helper.sh.
_SOURCE_RE = re.compile(
    r"""(?:^|[\s;|&(])(?:source|\.)\s+["']?((?:\$\([^)]*\)|[\w./@{}$+-])*\.sh)"""
)


def _has_shell_shebang(path: Path) -> bool:
    try:
        with open(path, "rb") as fh:
            first = fh.readline()
    except OSError:
        return False
    return first.startswith(b"#!") and b"sh" in first


def _resolve_source_ref(sourcer_rel: str, target: str,
                        root: Optional[Path] = None) -> Optional[str]:
    """Resolve a `source`/`.` target to a repo-relative path.

    Handles `./x.sh`, a bare `x.sh` beside the sourcer, and the variable forms
    (`$SCRIPT_DIR/x.sh`, `$(dirname "$0")/x.sh`, `$SCRIPT_DIR/../lib/x.sh`,
    `$REPO_ROOT/scripts/lib/x.sh`).

    A leading variable is ambiguous: `$SCRIPT_DIR/..` is sourcer-relative while
    `$REPO_ROOT/...` is repo-relative, and the name alone cannot tell them
    apart. So both readings are built, `..` is normalized away, and the one
    that exists on disk wins - collapsing to the trailing basename instead
    (the previous behavior) silently missed every `../lib/x.sh` helper, which
    is how most shared helpers here are actually sourced.
    """
    t = target.strip().strip("'\"")
    if not t.endswith(".sh"):
        return None
    base = sourcer_rel.rsplit("/", 1)[0] if "/" in sourcer_rel else ""

    def _norm(candidate: str) -> Optional[str]:
        n = posixpath.normpath(candidate)
        # Escaping the checkout is not a repo-relative ref.
        return None if n.startswith("..") or n.startswith("/") else n

    if "$" in t:
        # Drop the leading variable / command-substitution component; what
        # follows is the path, read either way.
        rest = t.split("/", 1)[1] if "/" in t else os.path.basename(t)
        cands = [c for c in (_norm(f"{base}/{rest}" if base else rest), _norm(rest)) if c]
        if not cands:
            return None
        if root is not None:
            for c in cands:
                if (root / c).exists():
                    return c
        return cands[0]

    if t.startswith("./"):
        t = t[2:]
    if t.startswith("/"):
        return None
    return _norm(f"{base}/{t}" if base else t)


def _source_edges(root: Path) -> dict[str, set[str]]:
    """Reverse source-reference map: helper path -> the files that source it.

    Read forward by discovery (a sourced file is a helper, not a standalone
    harness; running it directly produces a spurious failure) and backward by
    changed-selection (a touched helper must select its owning harnesses).
    """
    scan_dirs = [root / "tests", root / "scripts" / "tests", root / "skills"]
    edges: dict[str, set[str]] = {}
    for d in scan_dirs:
        if not d.exists():
            continue
        for p in d.rglob("*.sh"):
            try:
                text = p.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            sourcer_rel = p.relative_to(root).as_posix()
            for line in text.splitlines():
                if line.lstrip().startswith("#"):
                    continue
                for m in _SOURCE_RE.finditer(line):
                    resolved = _resolve_source_ref(sourcer_rel, m.group(1), root)
                    if resolved is not None and resolved != sourcer_rel:
                        edges.setdefault(resolved, set()).add(sourcer_rel)
    return edges


def _sourced_helpers(root: Path, candidates: set[str]) -> set[str]:
    if not candidates:
        return set()
    return {h for h in _source_edges(root) if h in candidates}


def discover_shell_harnesses(root: Path) -> list[str]:
    """Auto-discover owned shell harnesses; return repo-relative paths.

    A file is a harness iff it has a shell shebang, is not a library
    (scripts/lib/) or an orchestrated subtree (cli/tests/smoke/), and is not a
    source-only helper. This is what makes a new tests/hooks/test_x.sh run with
    no registry edit.
    """
    candidates: set[str] = set()
    for pat in _SMOKE_HARNESS_GLOBS:
        for p in root.glob(pat):
            rel = p.relative_to(root).as_posix()
            if rel.startswith("scripts/lib/") or rel.startswith("cli/tests/smoke/"):
                continue
            if not _has_shell_shebang(p):
                continue
            candidates.add(rel)
    sourced = _sourced_helpers(root, candidates)
    return sorted(candidates - sourced)


def _referenced_sh_files(steps: Sequence[tuple[str, str, str]]) -> set[str]:
    """Repo-relative .sh paths a structural step already invokes (for dedup)."""
    referenced: set[str] = set()
    for _, _, cmd in steps:
        for token in _SH_PATH_RE.findall(cmd):
            referenced.add(token.strip("./"))
    return referenced


def _smoke_discovered_steps(root: Path, referenced: set[str]) -> list[tuple[str, str, str]]:
    """Auto-discovered shell steps the full gate runs: a harness
    discover_shell_harnesses finds that no structural step already references
    and that is not quarantined. Extracted from _run_smoke so the
    _DISCOVERY_DEFERRED subtraction on the full-gate side is unit-tested too
    (select_changed's copy is covered by its own test; both must agree).
    """
    return [
        (rel, ".", f"bash {rel}")
        for rel in discover_shell_harnesses(root)
        if rel not in referenced and rel not in _DISCOVERY_DEFERRED
    ]


# Shell harnesses discover_shell_harnesses finds but smoke must not run yet.
# 16 entries held. 10 are RED on both platforms (pre-existing rot; each its own
# debugging session, out of scope here). The other 6 are macOS-green and
# Linux-red, so a developer census calls them drainable and CI does not:
#
#   tests/hooks/test_hook_events.sh - a non-portable perm check; GNU stat does
#     not fail on -f, so the Linux fallback branch never runs.
#
#   THE `fno`-ON-PATH FAMILY - five live harnesses, one cause, TWO SHAPES.
#   A fresh runner ships only the venv `fno-py`; a dev machine has a global
#   `fno`. Every member passes locally for that reason alone.
#
#   Shape 1, declared - opens with `command -v fno || skip`, exits 77, and
#   smoke counts a SKIP as a FAILURE, so it goes red at 0s having asserted
#   nothing. Find these with `rg -l 'command -v fno' tests/`:
#     tests/hooks/test_init_claim_stderr_and_modern_claim.sh
#     tests/hooks/test_init_contested_steal_guard.sh
#     tests/hooks/test_init_node_guard_tokenize.sh
#     tests/test_provider_substrate_e2e.sh
#
#   Shape 2, undeclared - the test never mentions `fno`; the CODE UNDER TEST
#   needs it, and that code FAILS OPEN when it is missing. So the harness does
#   not skip, it runs and asserts against behavior that silently did not happen:
#     tests/test-worktree-inside-checkout-redirect.sh - hooks/worktree-setup.sh:47
#       gates the `policy = never` refusal on `command -v fno`. With no `fno`
#       the guard never runs, the hook creates the worktree it should have
#       refused, and 6 of 11 assertions fail. The fail-open is deliberate (a
#       stale `fno` must not break interactive `claude --worktree`), so the test
#       is right and the environment is wrong.
#   A previous specimen, tests/test-target-state-recovery.sh, was repaired on
#   main (its fixture now seeds config.toml directly instead of relying on a
#   migration that fired as an init side effect) and is drained.
#   Shape 2 is invisible to the rg above: grep the harness and you find nothing,
#   because the dependency is one layer down in the code it exercises. There is
#   no static detector for it. CI is the detector.
#
#   The graduating fix is a shared `fno`-then-`fno-py` resolver, not a PATH
#   symlink. Eight files under scripts/lib/ and hooks/helpers/ open with
#   `command -v fno` and only scripts/lib/reconcile-throttle.sh:31 has an
#   ad-hoc fallback (and it points at ~/.local/bin, not the venv). That is one
#   convention decision across eight call sites, and it deserves its own PR:
#   scripts/ci/check-config-schema-drift.sh:24 branches on `command -v fno`
#   too, so this changes steps unrelated to this list.
#
# A LOCAL GREEN IS NOT GROUNDS TO DRAIN. The census runs on the developer's OS
# with the developer's PATH, and CI disagrees in BOTH directions: three of the
# family were drained on a macOS green and reverted when CI went red, while two
# other harnesses were macOS-red and Linux-green and were drained on CI's word.
#
# Simulating CI locally by stripping the global `fno` off PATH does NOT work
# either - tests/hooks/test_inject_fno_agent_whoami.sh fails that way and
# passes on CI, so the simulation manufactures failures of its own. Push and
# read the smoke log. It is slower and it is the only source that has been
# right every time.
#
# A green-fast harness here is silent coverage loss no file edit surfaces.
# `fno test --census-deferred` runs every entry bounded and exits non-zero the
# moment one passes inside the 60s tranche, so the set cannot re-accumulate green.
#
# Check an entry with the census, NOT with a bare `bash tests/foo.sh`. The census
# runs under _smoke_env (cli/.venv/bin on PATH), which is the environment smoke
# itself uses; a bare run resolves python3 to whatever the shell has and fails on
# a missing `typer` that CI would never hit. Both directions of that mismatch are
# a wrong conclusion: a spurious red reads as "correctly quarantined" and leaves
# working coverage off.
# Drain by deleting a line (discovery then covers it by shebang); _run_smoke and
# select_changed both subtract this set, so dropping a line widens both gates.
# The census only bites if something runs it: .github/workflows/deferred-census.yml
# runs it weekly, which is the cadence this list actually drifts at.
_DISCOVERY_DEFERRED: frozenset[str] = frozenset("""
scripts/tests/test_graph_resolve.sh
tests/events/test-check-pr-emits-polling.sh
tests/hooks/test_hook_events.sh
tests/hooks/test_init_claim_stderr_and_modern_claim.sh
tests/hooks/test_init_contested_steal_guard.sh
tests/hooks/test_init_node_guard_tokenize.sh
tests/smoke-megatron-e2e.sh
tests/smoke-target-shim.sh
tests/test-backlog-triage.sh
tests/test-graph-resolve.sh
tests/test-worktree-inside-checkout-redirect.sh
tests/test_emit_gate_transition.sh
tests/test_provider_substrate_e2e.sh
""".split())


def _smoke_env(root: Path) -> dict:
    """_child_env + the smoke additions: NO_COLOR, venv on PATH, no FORCE_COLOR.

    NO_COLOR=1 + unset FORCE_COLOR kill the SGR escapes that re-inflate an agent
    log ~7x; cli/.venv/bin on PATH pins every step's python3/fno/pytest to the
    project env (inert until the first `uv sync` creates it, exactly as the
    retired smoke.sh did).
    """
    env = _child_env(root)
    env["NO_COLOR"] = "1"
    env.pop("FORCE_COLOR", None)
    venv_bin = str((root / "cli" / ".venv" / "bin"))
    env["PATH"] = venv_bin + os.pathsep + env.get("PATH", "")
    return env


def _read_failure_record(path: str, known: set[str]) -> set[str]:
    """Recorded step names that still exist in the registry (corrupt -> drop)."""
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            lines = [ln.strip() for ln in fh if ln.strip()]
    except OSError:
        return set()
    return {ln for ln in lines if ln in known}


def _write_failure_record(path: str, failed_names: Sequence[str]) -> None:
    # The parent (.fno/) may not exist in a fresh checkout / preflight worktree;
    # the retired smoke.sh did `mkdir -p "$(dirname "$FAILURE_RECORD")"`. Without
    # this the open() raises OSError, is swallowed, and no record is written, so
    # the next --retry-failed falls back to the full suite instead of the failures.
    parent = os.path.dirname(path)
    try:
        if parent:
            os.makedirs(parent, exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            for n in failed_names:
                fh.write(n + "\n")
    except OSError:
        pass  # last-writer-wins per checkout; unwritable is non-fatal


def _smoke_prereqs(selected_cmds: Sequence[str]) -> Optional[str]:
    """Assert only what the selected steps need (uv / python3+yaml / cargo)."""
    blob = "\n".join(selected_cmds)
    if "uv " in blob or blob.endswith("uv") or "\nuv" in blob:
        if not shutil.which("uv"):
            return "uv (install from https://docs.astral.sh/uv)"
    if "python3" in blob:
        if not shutil.which("python3"):
            return "python3 (install Python 3)"
    if "cargo" in blob:
        if not shutil.which("cargo"):
            return "cargo (install the Rust toolchain)"
    return None


def _parse_smoke_args(args: Sequence[str]) -> dict:
    """Parse smoke flags into an options dict.

    Raises ValueError on a bad/unknown/contradictory argument; the caller
    prints it and exits 2. `--help`/`-h` is handled by the caller first.
    """
    opts: dict = {
        "list": False, "keep_going": False, "retry_failed": False,
        "verbose": False, "only": "", "changed": False, "base": "", "head": "",
        "ambient": "clean",
    }
    valued = {
        "--only": "only", "--base": "base", "--head": "head",
        "--ambient": "ambient",
    }
    pending = ""
    for a in args:
        if pending:
            # An empty value is a caller bug (an unset shell variable), never an
            # instruction. Accepting it silently turned `--only=` into a FULL
            # run and `--base=` into local mode, both looking like the subset
            # the caller asked for.
            # A following option is a forgotten value, not the value. Taking it
            # made `--only --retry-failed` an only-glob of "--retry-failed",
            # which matches no step and silently skips the mutual-exclusion
            # check that should have caught the combination.
            if not a or a.startswith("--"):
                raise ValueError(
                    f"smoke: --{pending.replace('_', '-')} needs a value"
                    + (f" (got the flag {a})" if a else ""))
            opts[pending] = a
            pending = ""
        elif a in valued:
            pending = valued[a]
        elif "=" in a and a.split("=", 1)[0] in valued:
            flag, val = a.split("=", 1)
            if not val:
                raise ValueError(f"smoke: {flag} needs a value")
            opts[valued[flag]] = val
        elif a == "--list":
            opts["list"] = True
        elif a == "--verbose":
            opts["verbose"] = True
        elif a == "--keep-going":
            opts["keep_going"] = True
        elif a == "--retry-failed":
            opts["retry_failed"] = True
        elif a == "--changed":
            opts["changed"] = True
        else:
            raise ValueError(f"smoke: unknown arg {a!r}")
    if pending:
        raise ValueError(f"smoke: --{pending.replace('_', '-')} needs a value")
    # Three subset modes with no defined precedence: refuse rather than let one
    # silently win and mislabel the evidence.
    subsets = [n for n, on in
               (("--changed", opts["changed"]), ("--only", bool(opts["only"])),
                ("--retry-failed", opts["retry_failed"])) if on]
    if len(subsets) > 1:
        raise ValueError(f"smoke: {' and '.join(subsets)} are separate subset modes - pick one")
    if (opts["base"] or opts["head"]) and not opts["changed"]:
        raise ValueError("smoke: --base/--head only apply to --changed")
    if opts["ambient"] not in _AMBIENT_MODES:
        raise ValueError(
            f"smoke: --ambient {opts['ambient']!r} is not one of "
            f"{'/'.join(_AMBIENT_MODES)}"
        )
    return opts


# ---------------------------------------------------------------------------
# Changed-surface packet (`fno test smoke --changed`).
#
# Partial evidence, never full. The selector maps changed paths to a subset of
# the same work the full runner owns; execution, step naming and exit semantics
# stay with the runner. Three separate mechanisms stop a partial green from
# reading as a full one: the CHANGED SUBSET label (never FULL), a failure
# record and receipt in their own namespace (so a partial run cannot clear the
# full record or mint a mode=FULL attestation), and typed exit codes that let a
# caller distinguish "passed" from "nothing mapped" from "no trustworthy diff".
# ---------------------------------------------------------------------------

CHANGED_RC_UNSELECTED = 20   # nothing mapped: not a failure, and not a green verdict
CHANGED_RC_UNEVALUATED = 21  # no trustworthy diff: fail closed, never partial-green
CHANGED_RC_PREREQ = 22       # the packet could not run at all (missing tool)
CHANGED_RECEIPT_DEFAULT = ".fno/changed-last-receipt.json"

# Broad infrastructure (rule 6): the runner, the selector, shared test config,
# and the workflows. A change here invalidates the mapping itself, so it selects
# the selector's own contract tests rather than trusting an inferred subset.
_INFRA_EXACT = ("cli/src/fno/test_cmd.py", "scripts/ci/preflight.sh", "cli/pyproject.toml")
_INFRA_PREFIX = (".github/workflows/",)
_INFRA_BASENAME = ("conftest.py",)
_INFRA_SELECTS = (
    "cli/tests/unit/test_test_cmd.py",
    "tests/ci/test_smoke_modes.sh",
    "tests/ci/test_preflight.sh",
    "tests/ci/test_changed_smoke_workflow.sh",
)


# (path prefix, the registry step whose runner orchestrates that whole tree).
_ORCHESTRATED_SUBTREES = (("cli/tests/smoke/", "Smoke tests"),)


def _lines(text: str) -> list[str]:
    return [ln.strip() for ln in text.splitlines() if ln.strip()]


def _git(root: Path, *args: str) -> tuple[int, str]:
    try:
        p = subprocess.run(["git", "-C", str(root), *args],
                           capture_output=True, text=True)
    except OSError as exc:
        return 127, str(exc)
    return p.returncode, (p.stdout if p.returncode == 0 else p.stderr).strip()


def changed_snapshot(root: Path, base: str = "", head: str = "") -> tuple[list[str], str, str]:
    """One deterministic changed-path snapshot -> (paths, unevaluated reason, resolved base).

    Explicit base+head (CI) diffs those exact revisions, so behavior never
    depends on a mutable remote-tracking ref. Local mode diffs the merge-base
    with origin/main (origin/master when main is absent) and adds untracked
    files, which a commit-to-commit diff
    cannot see but which are part of local changed intent. A non-empty reason
    means the result is UNEVALUATED, never an empty changeset. The resolved
    base names what actually sized the diff (`origin/master@<sha12>` in local
    mode, the given base when explicit, "" when unevaluated) so a receipt can
    never claim a base it did not use.
    """
    if bool(base) != bool(head):
        return [], "--changed takes both --base and --head, or neither (local mode)", ""
    if base:
        shas = []
        for rev in (base, head):
            rc, out = _git(root, "rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}")
            if rc != 0 or not out:
                return [], f"revision {rev!r} does not resolve here (shallow checkout or missing ref)", ""
            shas.append(out)
        rc, out = _git(root, "diff", "--name-only", f"{shas[0]}...{shas[1]}")
        if rc != 0:
            return [], f"git diff {base}...{head} failed: {out}", ""
        return sorted(set(_lines(out))), "", base

    # Only an ABSENT ref advances the fallback: a resolvable ref with no
    # merge-base (unrelated histories) means this branch cannot be sized
    # against that ref, not that a possibly stale sibling ref should answer
    # instead - a stale pre-migration origin/master would inflate the packet
    # with a whole era of main's history. Only a successful probe may set mb:
    # _git returns stderr on failure, so a leftover error string would slip
    # past the `not mb` gate below.
    mb = ""
    won = ""
    for candidate in ("origin/main", "origin/master"):
        rc, out = _git(root, "rev-parse", "--verify", "--quiet", f"{candidate}^{{commit}}")
        if rc != 0 or not out:
            continue
        rc, out = _git(root, "merge-base", candidate, "HEAD")
        if rc == 0 and out:
            mb = out
            won = candidate
        break
    if not mb:
        return [], "cannot resolve merge-base with origin/main or origin/master (fetch it, or pass --base/--head)", ""
    paths: set[str] = set()
    for args in (("diff", "--name-only", mb),
                 ("ls-files", "--others", "--exclude-standard")):
        rc, out = _git(root, *args)
        if rc != 0:
            return [], f"git {' '.join(args)} failed: {out}", ""
        paths.update(_lines(out))
    return sorted(paths), "", f"{won}@{mb[:12]}"


def _conventional_tests(root: Path, stem: str) -> list[str]:
    """Same-stem tests: `test_<stem>.py` plus the `test_<stem>_*.py` family.

    The suffixed family is not decoration - it is how a third of this repo's
    covered modules are actually named (claims.py -> test_claims_core.py,
    test_claims_io.py, ...), so exact-match alone leaves them unmapped. The
    widest family here is 19 files, still seconds against the full pytest step.
    """
    tests = root / "cli" / "tests"
    hits = {p.relative_to(root).as_posix() for p in tests.rglob(f"test_{stem}.py")}
    hits |= {p.relative_to(root).as_posix() for p in tests.rglob(f"test_{stem}_*.py")}
    return sorted(hits)


def _rust_family(rel: str) -> list[str]:
    """Registry step names that own the crate `rel` lives in."""
    parts = rel.split("/")
    if len(parts) < 2:
        return []
    crate = f"{parts[0]}/{parts[1]}"
    return [name for name, cwd, cmd in _STRUCTURAL_STEPS
            if cwd == crate or crate in cmd]


def select_changed(root: Path, paths: Sequence[str]) -> tuple[list[dict], list[str]]:
    """Map changed paths to work, in rule order -> (selections, unmapped).

    Each selection names the rule that produced it, so a bad mapping is
    diagnosable from the receipt. Selection is deterministic and intentionally
    incomplete: an unmapped path stays visible instead of becoming "green".
    """
    # Subtract the quarantine, exactly as _run_smoke does. These harnesses are
    # pre-existing rot the full gate refuses to run; selecting one turns an
    # ordinary edit into a red packet that aborts preflight BEFORE the real
    # gate, on a test CI would never have run.
    harnesses = set(discover_shell_harnesses(root)) - _DISCOVERY_DEFERRED
    edges = _source_edges(root)
    selections: list[dict] = []
    unmapped: list[str] = []
    seen: set[tuple[str, str]] = set()

    def add(rule: str, path: str, kind: str, target: str) -> None:
        if (kind, target) in seen:
            return
        seen.add((kind, target))
        selections.append({"rule": rule, "path": path, "kind": kind, "target": target})

    def fallback(rel: str) -> None:
        """Last resort before unmapped: a registry step that names this path.

        Applies to every path shape, not just shell: root-level `tests/*.py`
        run by a step's python3 invocation are owned exactly this way. An
        orchestrated subtree names only its runner in the registry, so the
        substring match cannot see the individual file - that prefix map is the
        mirror of discover_shell_harnesses() excluding the same tree.
        """
        owners = [name for name, _, cmd in _STRUCTURAL_STEPS if rel in cmd]
        owners += [o for prefix, o in _ORCHESTRATED_SUBTREES if rel.startswith(prefix)]
        if not owners:
            unmapped.append(rel)
            return
        for owner in dict.fromkeys(owners):
            add("registry-step", rel, "step", owner)

    for rel in paths:
        base = os.path.basename(rel)
        if (rel in _INFRA_EXACT or base in _INFRA_BASENAME
                or rel.startswith(_INFRA_PREFIX)):
            for target in _INFRA_SELECTS:
                if (root / target).exists():
                    add("infra-broad", rel, "pytest" if target.endswith(".py") else "shell", target)
            continue
        if rel.endswith(".py") and base.startswith("test_") and "/tests/" in rel:
            # A deleted test cannot be run. The path stays visible rather than
            # being handed to pytest, which would fail on the missing file.
            if (root / rel).exists():
                add("test-file-self", rel, "pytest", rel)
            else:
                fallback(rel)
            continue
        if rel.startswith("cli/src/") and rel.endswith(".py"):
            # rglob only returns files that exist, so a deleted source still
            # correctly selects its surviving test family.
            hits = _conventional_tests(root, base[:-3])
            if hits:
                for hit in hits:
                    add("python-source-stem", rel, "pytest", hit)
                continue
            fallback(rel)
            continue
        if rel.endswith(".sh"):
            if rel in harnesses:
                add("shell-harness-self", rel, "shell", rel)
                continue
            owners = sorted(s for s in edges.get(rel, ()) if s in harnesses)
            if owners:
                for o in owners:
                    add("shell-helper-reverse", rel, "shell", o)
                continue
            fallback(rel)
            continue
        if rel.startswith("crates/"):
            fam = _rust_family(rel)
            if fam:
                for name in fam:
                    add("rust-family", rel, "step", name)
                continue
            fallback(rel)
            continue
        fallback(rel)
    return selections, unmapped


# A journey harness that cannot find the debug binary exits 77 (a SKIP), which
# the packet counts as a failure - so selecting one without its build step
# produces a false red instead of feedback. The registry already owns the
# build; selection has to carry it along.
_RUST_BIN_MARKER = "target/debug/fno-agents"
_RUST_BUILD_STEP = "Build fno-agents debug binary (for journey tests)"


def _needs_rust_binary(root: Path, rel: str) -> bool:
    try:
        return _RUST_BIN_MARKER in (root / rel).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return False


def _changed_steps(root: Path, selections: Sequence[dict]) -> list[tuple[str, str, str]]:
    """Turn selections into runner steps, fastest-signal first."""
    pytest_targets = [s["target"] for s in selections if s["kind"] == "pytest"]
    steps: list[tuple[str, str, str]] = []
    if pytest_targets:
        targets = sorted(set(pytest_targets))
        # xdist costs more than it saves on a handful of files.
        par = " -n auto --maxprocesses=4 --dist=loadgroup" if len(targets) > 3 else ""
        steps.append((f"Pytest (changed subset, {len(targets)} file(s))", ".",
                      f"uv run --project cli pytest --tb=short -q{par} "
                      + " ".join(shlex.quote(t) for t in targets)))
    shell_rels = sorted({s["target"] for s in selections if s["kind"] == "shell"})
    by_name = {name: (name, cwd, cmd) for name, cwd, cmd in _STRUCTURAL_STEPS}
    if any(_needs_rust_binary(root, rel) for rel in shell_rels) and _RUST_BUILD_STEP in by_name:
        steps.append(by_name[_RUST_BUILD_STEP])
    for rel in shell_rels:
        steps.append((rel, ".", f"bash {shlex.quote(rel)}"))
    shell_targets = set(shell_rels)
    for name in sorted({s["target"] for s in selections if s["kind"] == "step"}):
        step = by_name[name]
        refs = {t.strip("./") for t in _SH_PATH_RE.findall(step[2])}
        if refs and refs <= shell_targets:
            continue  # every harness it wraps is already selected directly
        steps.append(step)
    return steps


def _write_changed_receipt(path: str, payload: dict) -> None:
    """Receipt in the changed-mode namespace; never the full record's path."""
    try:
        parent = os.path.dirname(path)
        if parent:
            os.makedirs(parent, exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2, sort_keys=True)
            fh.write("\n")
    except OSError:
        pass  # a receipt we cannot write is not a reason to fail the packet


def _run_changed(root: Path, opts: dict, env: dict) -> int:
    t0 = time.monotonic()
    paths, reason, resolved_base = changed_snapshot(root, opts["base"], opts["head"])
    rc_head, head_sha = _git(root, "rev-parse", "HEAD")
    candidate = head_sha if rc_head == 0 else "unknown"
    print("smoke: mode=CHANGED SUBSET - partial evidence; the full "
          "'fno test smoke' gate still decides merge readiness", flush=True)
    if reason:
        # Overwrite any earlier receipt: leaving the previous run's green in
        # place would let a stale verdict outlive the candidate it described.
        _write_changed_receipt(
            os.environ.get("SMOKE_CHANGED_RECEIPT") or str(root / CHANGED_RECEIPT_DEFAULT),
            {"mode": "CHANGED SUBSET", "candidate": candidate, "verdict": "unevaluated",
             "reason": reason, "base": opts["base"], "head": opts["head"]},
        )
        sys.stderr.write(f"smoke: CHANGED SUBSET UNEVALUATED - {reason}\n")
        sys.stderr.write("smoke: no changed verdict was earned (this is not a green result)\n")
        return CHANGED_RC_UNEVALUATED

    selections, unmapped = select_changed(root, paths)
    steps = _changed_steps(root, selections)
    select_s = time.monotonic() - t0

    print(f"smoke: base={opts['base'] or resolved_base} "
          f"head={opts['head'] or candidate[:12]} changed={len(paths)} "
          f"selected={len(steps)} unmapped={len(unmapped)}", flush=True)
    for s in selections:
        print(f"  select  {s['rule']:20} {s['path']} -> {s['target']}", flush=True)
    for u in unmapped:
        print(f"  unmapped {u}  (no rule; covered only by the full gate)", flush=True)

    receipt = {
        "mode": "CHANGED SUBSET", "candidate": candidate,
        "base": opts["base"] or resolved_base, "head": opts["head"] or candidate,
        "changed_paths": paths, "unmapped_paths": unmapped,
        "selections": selections, "selected_count": len(steps),
        "unmapped_count": len(unmapped),
        "selection_seconds": round(select_s, 3),
    }
    receipt_path = os.environ.get("SMOKE_CHANGED_RECEIPT") or str(root / CHANGED_RECEIPT_DEFAULT)

    if opts["list"]:
        return 0
    if not steps:
        receipt.update(verdict="unselected", execution_seconds=0.0, first_signal_seconds=None)
        _write_changed_receipt(receipt_path, receipt)
        sys.stderr.write("smoke: CHANGED SUBSET selected NOTHING - that is evidence about the "
                         "selector, not that the change is safe\n")
        return CHANGED_RC_UNSELECTED

    miss = _smoke_prereqs([c for _, _, c in steps])
    if miss:
        receipt.update(verdict="unevaluated", reason=f"missing prerequisite: {miss}")
        _write_changed_receipt(receipt_path, receipt)
        sys.stderr.write(f"smoke: missing prerequisite: {miss}\n")
        return CHANGED_RC_PREREQ

    # Same faithful-ordering guard the full run applies: the pytest step must
    # see NO fno-agents binary so the @requires_rust parity tests skip, as they
    # do in CI. preflight deliberately preserves target/ across runs, so without
    # this the packet runs ~15 tests the full gate skips - and a failure there
    # aborts preflight on a discrepancy the gate would never report. Any journey
    # harness that needs the binary is preceded by its build step (above).
    if any(n.startswith("Pytest (changed subset") for n, _, _ in steps):
        for rel in ("crates/fno-agents/target/debug/fno-agents",
                    "crates/fno-agents/target/release/fno-agents"):
            try:
                (root / rel).unlink()
            except OSError:
                pass

    e0 = time.monotonic()
    results, rc = _execute_steps(root, env, steps, keep_going=opts["keep_going"])
    exec_s = time.monotonic() - e0
    if rc in (CHANGED_RC_UNSELECTED, CHANGED_RC_UNEVALUATED, CHANGED_RC_PREREQ):
        # In-band signalling hazard. Propagating a child code that happens to
        # equal a sentinel would tell preflight "nothing selected" / "no
        # trustworthy diff", and it would fall THROUGH to the full gate instead
        # of stopping on a real failure - a red silently downgraded to a
        # non-verdict. The sentinels have to stay unambiguous, so a genuine
        # step failure that collides with one reports as a plain 1.
        print(f"smoke: step exit {rc} collides with a changed-mode sentinel; "
              f"reporting 1 (a failure, not a non-verdict)", flush=True)
        rc = 1
    # Time to first signal: elapsed up to and including the first failing step,
    # or the whole packet when it is green. Summing every step would overstate
    # it under --keep-going, where execution continues past the first failure.
    signal = 0.0
    for _, status, dur in results:
        signal += dur
        if status == "fail":
            break

    print("", flush=True)
    print(f"smoke: summary (changed-subset, {len(results)} steps)", flush=True)
    for step_name, status, dur in results:
        print(f"  {status:6} {dur:4.0f}s  {step_name}", flush=True)
    verdict = "red" if rc != 0 else "green"
    print(f"smoke: CHANGED SUBSET verdict={verdict} selected={len(steps)} "
          f"unmapped={len(unmapped)} select={select_s:.1f}s exec={exec_s:.1f}s "
          f"first_signal={signal:.1f}s", flush=True)
    if verdict == "green":
        print("smoke: CHANGED SUBSET green means the selected packet passed - run "
              "'fno test smoke' full before the settle-green push", flush=True)

    receipt.update(verdict=verdict, execution_seconds=round(exec_s, 3),
                   first_signal_seconds=round(signal, 3),
                   failed=[n for n, s, _ in results if s == "fail"])
    # The receipt is the changed mode's ONLY durable artifact, and it lives in
    # its own namespace - the full runner's failure record is never touched.
    _write_changed_receipt(receipt_path, receipt)
    return rc


def _execute_steps(
    root: Path, env: dict, steps: Sequence[tuple[str, str, str]], keep_going: bool
) -> tuple[list[tuple[str, str, float]], int]:
    """Run steps in order -> (per-step results, first non-zero child rc).

    The child's real return code is propagated, never a pipe's or a `tail`'s.
    """
    in_ci = bool(os.environ.get("GITHUB_ACTIONS"))
    results: list[tuple[str, str, float]] = []
    first_rc = 0
    for name, cwd, cmd in steps:
        if in_ci:
            print(f"::group::{name}", flush=True)
        else:
            print(f"\n=== {name} ===", flush=True)
        t0 = time.monotonic()
        try:
            proc = subprocess.run(
                ["bash", "-eo", "pipefail", "-c", cmd],
                cwd=str(root / cwd) if cwd != "." else str(root),
                env=env,
            )
            rc = proc.returncode
        except OSError as exc:
            sys.stderr.write(f"smoke: failed to run step: {exc}\n")
            rc = 127
        dur = time.monotonic() - t0
        if in_ci:
            print("::endgroup::", flush=True)
        results.append((name, "pass" if rc == 0 else "fail", dur))
        print(f"smoke: {'pass' if rc == 0 else 'fail'}  {dur:4.0f}s  {name}", flush=True)
        if rc != 0:
            if first_rc == 0:
                first_rc = rc
            if not keep_going:
                sys.stderr.write(f"smoke: step failed, stopping (fail-fast): {name}\n")
                break
    return results, first_rc


def _run_smoke(args: Sequence[str], stream: bool = False) -> int:
    """Run the full smoke with mode machinery; return the real child exit code.

    First failure wins in fail-fast; --keep-going runs every step, records the
    failures, and aggregates a non-zero exit. The exit code is the child's
    actual rc (no pipe/tail masking), so preflight's attestation gate reads the
    truth.

    Flags: --list [--verbose], --keep-going, --only <glob>, --retry-failed,
    and --changed [--base REV --head REV] for the changed-surface packet (a
    CHANGED SUBSET; exits 20 when nothing mapped, 21 when the diff is not
    trustworthy - neither is a green verdict, and the full run still gates).
    The three subset modes are mutually exclusive.

    --ambient clean|dirty|both (default clean) picks the ambient shape every
    step's child process runs under. `dirty` poisons this runner's own
    environment with synthetic developer state and lets the hermeticity fixture
    scrub it: a channel the fixture covers produces a child identical to the
    clean lane, and a channel it MISSED survives into the child and reddens the
    test that reads it. A green clean lane beside a red dirty lane is an ambient
    leak, named by the failing test. `both` runs the two and reports which
    diverged.

    NOTE: hermeticity is applied by THIS runner, when it builds each step's
    environment. Running a shell test directly (`bash tests/foo.sh`) skips it,
    so that run reads your real HOME, config chain and carve-out ledger; a pass
    there is not evidence the test is hermetic, and a failure may be your
    machine. `fno test smoke --only '<glob>'` runs the same step hermetically.
    """
    root = _repo_root(Path.cwd()) or Path.cwd()
    if any(a in ("-h", "--help") for a in args):
        print(_run_smoke.__doc__)
        return 0
    try:
        opts = _parse_smoke_args(args)
    except ValueError as exc:
        sys.stderr.write(str(exc) + "\n")
        return 2

    if opts["ambient"] == "both":
        rest: list[str] = []
        skip_next = False
        for a in args:
            if skip_next:  # the detached value of a bare `--ambient`
                skip_next = False
                continue
            if a == "--ambient":
                skip_next = True
                continue
            if a.startswith("--ambient="):
                continue
            rest.append(a)
        print("ambient: clean lane")
        clean_rc = _run_smoke([*rest, "--ambient=clean"], stream=stream)
        print("ambient: dirty lane")
        dirty_rc = _run_smoke([*rest, "--ambient=dirty"], stream=stream)
        if clean_rc == 0 and dirty_rc != 0:
            sys.stderr.write(
                "ambient divergence: the clean lane is green and the dirty lane "
                "is red on the same commit. A test above read ambient state that "
                "fno/hermetic.py does not neutralise. The failing test names "
                "itself; add its channel to _AMBIENT_NAMES.\n"
            )
        elif clean_rc != 0 and dirty_rc == 0:
            sys.stderr.write(
                "ambient divergence (inverted): the clean lane is red and the "
                "dirty lane is green. A test DEPENDS on ambient state being "
                "present.\n"
            )
        return clean_rc or dirty_rc

    global _AMBIENT_MODE
    _AMBIENT_MODE = opts["ambient"]
    list_mode, keep_going = opts["list"], opts["keep_going"]
    retry_failed, verbose, only_glob = opts["retry_failed"], opts["verbose"], opts["only"]

    env = _smoke_env(root)

    if opts["changed"]:
        return _run_changed(root, opts, env)

    reg_file = os.environ.get("SMOKE_REGISTRY_FILE")
    if reg_file:
        # Test seam: a tiny hermetic registry exercises the mode machinery
        # without the real ~57 steps or their uv/cargo prerequisites.
        ns: dict = {}
        with open(reg_file, encoding="utf-8") as fh:
            exec(fh.read(), ns)
        steps = list(ns["STEPS"])
    else:
        structural = list(_STRUCTURAL_STEPS)
        referenced = _referenced_sh_files(structural)
        steps = structural + _smoke_discovered_steps(root, referenced)

    names = [s[0] for s in steps]
    total = len(steps)
    name_set = set(names)

    # Lane-scoped. The two lanes fail for different reasons, so a shared record
    # would let a dirty-lane failure list be replayed into a clean-lane
    # --retry-failed and re-run steps that were never red there.
    failure_record = os.environ.get("SMOKE_FAILURE_RECORD") or str(
        root / SMOKE_FAILURE_RECORD_DEFAULT
    )
    if _AMBIENT_MODE != "clean":
        base, dot, ext = failure_record.rpartition(".")
        failure_record = f"{base}-{_AMBIENT_MODE}{dot}{ext}" if dot else (
            f"{failure_record}-{_AMBIENT_MODE}"
        )

    # Selection: build the selected indices from the mode.
    retry_fell_back = False
    if retry_failed:
        recorded = _read_failure_record(failure_record, name_set)
        if recorded:
            selected = [i for i, n in enumerate(names) if n in recorded]
        else:
            retry_fell_back = True
            selected = list(range(total))
    elif only_glob:
        selected = [i for i, n in enumerate(names) if fnmatch.fnmatch(n, only_glob)]
        if not selected:
            sys.stderr.write(f"smoke: --only {only_glob!r} matched no steps. Available:\n")
            for n in names:
                sys.stderr.write(f"  {n}\n")
            return 1
    else:
        selected = list(range(total))

    if list_mode:
        for i in selected:
            if verbose:
                joined = steps[i][2].replace("\n", ";")
                print(f"{names[i]}\n  cwd: {steps[i][1]}\n  cmd: {joined}")
            else:
                print(names[i])
        return 0

    # Header: a partial run must never read as full.
    label = "FULL"
    if retry_failed:
        label = "RETRY SUBSET"
    elif only_glob:
        label = "ONLY SUBSET"
    kg = " keep-going" if keep_going else ""
    print(f"smoke: mode={label} steps={len(selected)}/{total}{kg}", flush=True)
    if retry_fell_back:
        print("smoke: no usable failure record - falling back to FULL run", flush=True)
    if len(selected) != total:
        print("smoke: SUBSET run - run 'fno test smoke' full before the settle-green push",
              flush=True)

    miss = _smoke_prereqs([steps[i][2] for i in selected])
    if miss:
        sys.stderr.write(f"smoke: missing prerequisite: {miss}\n")
        return 2  # the full run's documented prerequisite code; 22 is changed-only

    # Faithful-ordering guard: the main pytest step runs with the fno-agents
    # binary absent so @requires_rust parity tests skip (they need a provider
    # CLI CI lacks); the dedicated build step recreates it for later rust steps.
    if "Pytest (unit + integration)" in {names[i] for i in selected}:
        for rel in ("crates/fno-agents/target/debug/fno-agents",
                    "crates/fno-agents/target/release/fno-agents"):
            try:
                (root / rel).unlink()
            except OSError:
                pass

    if not selected:
        sys.stderr.write("smoke: zero steps selected - never green\n")
        return 1

    results, first_rc = _execute_steps(root, env, [steps[i] for i in selected], keep_going)
    failed = sum(1 for _, s, _ in results if s == "fail")
    if first_rc != 0 and not keep_going:
        _write_failure_record(failure_record, [n for n, s, _ in results if s == "fail"])
        return 1

    print("", flush=True)
    kind = ("retry-subset" if retry_failed else "only-subset" if only_glob else "full")
    print(f"smoke: summary ({kind}, {len(results)} steps)", flush=True)
    for n, s, d in results:
        print(f"  {s:6} {d:4.0f}s  {n}", flush=True)

    _write_failure_record(failure_record, [n for n, s, _ in results if s == "fail"])
    return 1 if failed else 0


# --census-deferred: stop _DISCOVERY_DEFERRED from silently holding green
# harnesses. A passing-fast entry here is coverage loss no file edit surfaces,
# so this runs every entry bounded and fails the moment one passes inside the
# tranche bound. It reads the set and reports; it never edits it.
CENSUS_TRANCHE_BOUND_S = 60          # GREEN vs SLOW splitter: the drain tranche ceiling
CENSUS_KILL_BOUND_S_DEFAULT = 300    # generous; slow-but-green harnesses finish and
                                     # classify as SLOW, only true hangers get killed


def _census_entries() -> list[str]:
    """Deferred harness paths to census.

    _DISCOVERY_DEFERRED is the source; CENSUS_DEFERRED_FILE (one path per line,
    ``#`` comments allowed) overrides it for a targeted run or a cheap check.
    Missing paths are returned as-is: bash reports them rc 127, recorded as RED.
    """
    override = os.environ.get("CENSUS_DEFERRED_FILE")
    if override:
        try:
            with open(override, encoding="utf-8") as fh:
                raw = [ln.strip() for ln in fh if ln.strip() and not ln.lstrip().startswith("#")]
        except OSError as exc:
            # Name the path + error: a typo'd CENSUS_DEFERRED_FILE otherwise
            # looks identical to a genuine empty set (both exit 2).
            sys.stderr.write(
                f"census: cannot read CENSUS_DEFERRED_FILE={override!r}: {exc}\n")
            return []
        return sorted(set(raw))
    return sorted(_DISCOVERY_DEFERRED)


def _run_bounded(cmd: Sequence[str], env: dict, cwd: Path, kill_bound_s: int) -> tuple[int, float, bool]:
    """Run cmd; on exceeding kill_bound_s, kill the whole process group.

    Returns (returncode, elapsed_seconds, killed). Uses subprocess.wait(timeout)
    + os.killpg, never the `timeout` binary: that is absent on macOS (a recorded
    trap) and exits 127, measuring nothing. start_new_session makes the child a
    group leader so killpg reaches its grandchildren too, since a shell harness
    spawns subprocesses a direct SIGKILL would orphan.
    """
    start = time.monotonic()
    proc = subprocess.Popen(
        cmd, env=env, cwd=str(cwd),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        rc = proc.wait(timeout=kill_bound_s)
        killed = False
    except subprocess.TimeoutExpired:
        # ProcessLookupError is the TOCTOU window where the child exited between
        # the timeout and getpgid; wait() reaps it. PermissionError is NOT caught:
        # start_new_session makes us own the group so it is near-impossible, and
        # if it ever surfaces a loud crash beats a wedged proc.wait() with no kill.
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()
        rc = proc.returncode if proc.returncode is not None else 124
        killed = True
    except KeyboardInterrupt:
        # The harness runs in its own session (start_new_session), so it does not
        # share the terminal's SIGINT and would outlive a Ctrl-C with all its
        # grandchildren. Kill the group before re-raising so nothing is orphaned.
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()
        raise
    return rc, time.monotonic() - start, killed


def _run_census_deferred(args: Sequence[str]) -> int:
    """Run every _DISCOVERY_DEFERRED harness bounded; exit 1 if any is GREEN.

    One tab-separated line per harness on stdout: verdict, rc, seconds, path.
    Verdicts: GREEN (rc 0 and <= tranche bound), SLOW (over the tranche bound,
    or killed by the run bound), RED (non-zero rc). Exit 1 if any GREEN, 0 if
    all RED/SLOW, 2 if the deferred set is empty or unparseable. Costs about 12
    minutes on the full set, so run it on a cadence, not on every PR; its
    finding is never caused by the PR under review.
    """
    root = _repo_root(Path.cwd()) or Path.cwd()
    tranche = CENSUS_TRANCHE_BOUND_S
    raw_bound = os.environ.get("CENSUS_KILL_BOUND_S")
    try:
        kill_bound = int(raw_bound) if raw_bound is not None else CENSUS_KILL_BOUND_S_DEFAULT
    except ValueError:
        # Surface the rejection: a typo'd bound silently runs at the default and
        # the header prints kill=300s with no hint the operator's value was dropped.
        sys.stderr.write(
            f"census: ignoring non-integer CENSUS_KILL_BOUND_S={raw_bound!r}, "
            f"using {CENSUS_KILL_BOUND_S_DEFAULT}\n")
        kill_bound = CENSUS_KILL_BOUND_S_DEFAULT
    if kill_bound < tranche:
        kill_bound = tranche  # below the tranche, a 60-70s-green harness is killed as SLOW

    entries = _census_entries()
    if not entries:
        sys.stderr.write("census: deferred set is empty or unparseable\n")
        return 2

    env = _smoke_env(root)
    green: list[tuple[str, float]] = []
    print(f"census: {len(entries)} deferred entries, tranche={tranche}s kill={kill_bound}s", flush=True)
    for rel in entries:
        rc, secs, killed = _run_bounded(["bash", rel], env, root, kill_bound)
        if killed:
            verdict = "SLOW"
        elif rc == 0:
            verdict = "GREEN" if secs <= tranche else "SLOW"
            if verdict == "GREEN":
                green.append((rel, secs))
        else:
            verdict = "RED"
        print(f"{verdict}\t{rc}\t{secs:.0f}\t{rel}", flush=True)

    if green:
        sys.stderr.write(
            "census: GREEN deferred entries are silent coverage loss; "
            "delete their lines from _DISCOVERY_DEFERRED (discovery then covers them):\n")
        for rel, secs in green:
            sys.stderr.write(f"  {rel}  ({secs:.0f}s)\n")
        return 1
    sys.stderr.write(f"census: no GREEN entries; {len(entries)} held as RED/SLOW\n")
    return 0


@click.command(
    name="test",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Run the Python suite (default), or a sub-suite: `fno test rust ...` "
        "(crates) or `fno test smoke [flags]` (the full CI smoke: structural "
        "steps plus auto-discovered shell harnesses). `fno test "
        "--census-deferred` runs every _DISCOVERY_DEFERRED entry bounded and "
        "exits non-zero if any passes inside the 60s tranche, so the quarantine "
        "cannot silently hold working tests. The real exit code is "
        "propagated, rtk is bypassed (RTK_DISABLED=1), and PYTHONPATH is "
        "pinned to the worktree's cli/src. Use this, never a bare `pytest` in "
        "a worktree: that imports the canonical fno, lets rtk re-wrap the run, "
        "and masks the exit code. Bare `fno test` runs the Python suite in parallel "
        "and captures to .fno/last-test.log (the transcript gets PASS or the failing tail); "
        "--stream restores full inherited-stdio output."
    ),
)
@click.option("--stream", is_flag=True, help="Stream full output (no capture/log).")
@click.argument("runner_args", nargs=-1, type=click.UNPROCESSED)
def test_command(stream: bool, runner_args: tuple[str, ...]) -> None:
    args = list(runner_args)
    if args and args[0] == "rust":
        raise SystemExit(_run_rust(args[1:], stream=stream))
    if args and args[0] == "smoke":
        raise SystemExit(_run_smoke(args[1:], stream=stream))
    if args and args[0] == "--census-deferred":
        raise SystemExit(_run_census_deferred(args[1:]))
    raise SystemExit(_run(args, stream=stream))
