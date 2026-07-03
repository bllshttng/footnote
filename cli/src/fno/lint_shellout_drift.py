"""US4 CI drift guard - no repo-root shell-out without a degrade path.

`fno lint shellout-drift` is the regression backstop for the packaging epic
(ab-8bdb4642 / ab-acbde274). Once the shell-outs were eliminated (Group 1:
folded into Rust or internalized into Python), the remaining risk is that a
future `fno` verb shells out to a NEW repo-root script and silently reintroduces
the clean-install gap a bare `pip install fno` cannot satisfy.

This guard scans ``cli/src/fno/`` for verbs that bash/sh-exec a script whose path
is rooted at the SHARED resolvers ``resolve_repo_root()`` / ``resolve_plugin_script()``
(the idiom 11 of 12 plugin-script call sites use). Every such script must be on the
``CLONE_ONLY_SCRIPTS`` allowlist (``scripts/lint/.clone-only-scripts.txt``), and the
guard PROVES each allowlisted verb degrades on a bare install by invoking it in a
no-script environment.

Two remedies for a flagged shell-out (AC4-ERR): eliminate it (fold to Rust /
internalize to Python), or add it to the allowlist with a proven degrade path.

Scope boundaries (deliberate, documented so they are not silent gaps):

* SIGNAL = a module that (a) bash/sh-execs via ``subprocess`` AND (b) calls the
  shared ``resolve_repo_root()`` / ``resolve_plugin_script()`` (exact names). This
  excludes ``cost/_register.py`` (roots ``events.sh`` at ``PLUGIN_ROOT`` /
  package-relative via its own ``_resolve_repo_root`` helper, best-effort and
  non-fatal - not the shared resolver), ``paths_cli.py`` (constructs
  ``scripts/lib/paths.sh`` but writes/reads it - never bash-execs it), and
  ``worktree.py``'s ``_run_setup_worktree_hook`` (roots ``setup-worktree.sh`` at an
  injected ``repo_root`` parameter, and already tolerates the script's absence).
* A verb that roots a shell-out via a PRIVATE ``git rev-parse`` helper instead of
  the shared resolver is invisible to this guard (the ``(b)`` SIGNAL above fails).
  That is a hiding place, not a sanctioned exemption: the fix is to re-root the
  verb on the shared resolver and add a degrade branch + an allowlist entry, which
  is what ``fno lint flock-pattern`` (-> ``scripts/lint-flock-pattern.sh``) now does
  (ab-fd017698) - so it is in scope, listed, and degrade-proven.
* SCAN_EXCLUDE: ``evals/runner.py`` is the in-repo-only efficacy-eval harness; it
  shells out to ``run-target-loop.sh`` + fixture asserts that exist only inside a
  clone by construction. It is never reachable on a bare install.
* Detection matches the dominant idiom: a literal ``"bash"``/``"sh"`` as ``argv[0]``
  of a ``subprocess`` call (``subprocess.run`` or a bare ``run``; inline list or an
  annotated/plain local bound to one), and the resolver / ``Path`` ctor called by name
  in either direct (``resolve_repo_root()``) or attribute (``paths.resolve_repo_root()``,
  ``pathlib.Path(...)``) form. By design it does NOT catch ``shell=True`` string commands,
  ``/bin/bash`` absolute ``argv[0]``, a script embedded inside a ``bash -c`` string,
  ``os.system(...)``, or a resolver reached through an import ALIAS
  (``from fno.paths import resolve_repo_root as rr``). These are accepted blind spots for
  a regression lint (not an adversarial sandbox): the realistic regression vector is a
  contributor reaching for the same shared helper the other 11 call sites use, which this
  does catch. ``collect_relpaths`` is module-level (it flags every ``scripts/``/``hooks/``
  ``.sh`` literal in an in-scope module, not only those in a subprocess-argument position),
  so it errs toward flagging - the safe direction for a drift guard.

Fail-closed (AC4-FR): any parse failure or unexpected error makes the check red,
never a silent green.
"""
from __future__ import annotations

import ast
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple


# --------------------------------------------------------------------------- #
# Defaults (repo-relative).
# --------------------------------------------------------------------------- #
SCAN_REL = "cli/src/fno"
ALLOWLIST_REL = "scripts/lint/.clone-only-scripts.txt"

# In-repo-only modules excluded from the scan (see module docstring). Repo-relative.
SCAN_EXCLUDE: Set[str] = {"cli/src/fno/evals/runner.py"}

_SUBPROCESS_FUNCS = {"run", "Popen", "call", "check_call", "check_output"}
_SHELLS = {"bash", "sh"}
_RESOLVER_NAMES = {"resolve_repo_root", "resolve_plugin_script"}
_SCRIPT_PREFIXES = ("scripts", "hooks")
_DEGRADE_TIMEOUT = 30


class GuardError(Exception):
    """Raised on a condition that must make the guard fail closed (red)."""


@dataclass(frozen=True)
class Violation:
    file: str  # repo-relative
    line: int
    relpath: str


@dataclass(frozen=True)
class DegradeFailure:
    relpath: str
    verb: List[str]
    reason: str


@dataclass
class Report:
    exit_code: int
    lines: List[str]


# --------------------------------------------------------------------------- #
# Allowlist parsing.
# --------------------------------------------------------------------------- #
def parse_allowlist(path: Path) -> Dict[str, List[str]]:
    """Parse ``relpath :: verb args`` lines into {relpath: [verb, args...]}.

    Raises GuardError on a malformed line (fail-closed: a broken allowlist must
    not silently pass).
    """
    if not path.is_file():
        raise GuardError(f"allowlist not found: {path}")
    entries: Dict[str, List[str]] = {}
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "::" not in line:
            raise GuardError(
                f"{path}:{lineno}: malformed allowlist line (missing ' :: ' "
                f"separator between script path and degrade verb): {raw!r}"
            )
        relpath_part, _, verb_part = line.partition("::")
        relpath = relpath_part.strip()
        verb = verb_part.split()
        if not relpath or not verb:
            raise GuardError(
                f"{path}:{lineno}: malformed allowlist line (empty script path "
                f"or empty degrade verb): {raw!r}"
            )
        entries[relpath] = verb
    return entries


# --------------------------------------------------------------------------- #
# AST detection.
# --------------------------------------------------------------------------- #
def _leftmost_list(node: ast.AST) -> Optional[ast.List]:
    """Peel a leading ``+`` chain to reach a list literal (``["bash",..] + args``)."""
    while isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        node = node.left
    return node if isinstance(node, ast.List) else None


def _first_elt_is_shell(node: ast.AST) -> bool:
    lst = _leftmost_list(node)
    if lst is not None and lst.elts:
        first = lst.elts[0]
        return isinstance(first, ast.Constant) and first.value in _SHELLS
    return False


def _called_name(node: ast.AST) -> Optional[str]:
    """The simple name of a Call's target for both ``foo()`` (Name) and
    ``mod.foo()`` (Attribute) forms - so ``subprocess.run`` vs ``run``,
    ``paths.resolve_repo_root`` vs ``resolve_repo_root``, and ``pathlib.Path``
    vs ``Path`` are all recognized. Returns None for anything else."""
    if not isinstance(node, ast.Call):
        return None
    f = node.func
    if isinstance(f, ast.Name):
        return f.id
    if isinstance(f, ast.Attribute):
        return f.attr
    return None


def _module_bash_execs(tree: ast.AST) -> bool:
    """True if the module runs a subprocess with ``['bash'/'sh', ...]`` as argv,
    inline or via a Name bound to such a list. Handles both ``subprocess.run``
    and ``from subprocess import run`` call forms, and both plain and annotated
    (``cmd: list[str] = [...]``) list assignments."""
    shell_names: Set[str] = set()
    for n in ast.walk(tree):
        if isinstance(n, ast.Assign) and _first_elt_is_shell(n.value):
            for t in n.targets:
                if isinstance(t, ast.Name):
                    shell_names.add(t.id)
        elif isinstance(n, ast.AnnAssign) and n.value is not None and _first_elt_is_shell(n.value):
            if isinstance(n.target, ast.Name):
                shell_names.add(n.target.id)
    for n in ast.walk(tree):
        if not isinstance(n, ast.Call) or not n.args:
            continue
        if _called_name(n) not in _SUBPROCESS_FUNCS:
            continue
        a0 = n.args[0]
        if _first_elt_is_shell(a0):
            return True
        if isinstance(a0, ast.Name) and a0.id in shell_names:
            return True
    return False


def _module_has_resolver_call(tree: ast.AST) -> bool:
    """True if the module calls the SHARED ``resolve_repo_root()`` /
    ``resolve_plugin_script()`` by name, direct (``resolve_repo_root()``) or
    attribute (``paths.resolve_repo_root()``). Exact names, so ``_resolve_repo_root``
    is excluded."""
    for n in ast.walk(tree):
        if _called_name(n) in _RESOLVER_NAMES:
            return True
    return False


def _docstring_nodes(tree: ast.AST) -> Set[int]:
    """id()s of bare-string-statement Constants (module/func/class docstrings),
    so script-name examples in prose do not count as shell-out targets."""
    ids: Set[int] = set()
    for n in ast.walk(tree):
        if isinstance(n, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            body = getattr(n, "body", None)
            if body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) \
                    and isinstance(body[0].value.value, str):
                ids.add(id(body[0].value))
    return ids


def _flatten_div(node: ast.AST) -> List[Optional[str]]:
    """Flatten ``a / b / c`` (BinOp Div) into string segments.
    ``Path("x")`` -> "x"; ``"lit"`` -> "lit"; anything else -> None."""
    segs: List[Optional[str]] = []

    def rec(x: ast.AST) -> None:
        if isinstance(x, ast.BinOp) and isinstance(x.op, ast.Div):
            rec(x.left)
            rec(x.right)
        elif isinstance(x, ast.Constant) and isinstance(x.value, str):
            segs.append(x.value)
        elif (
            _called_name(x) == "Path"
            and isinstance(x, ast.Call)
            and x.args
            and isinstance(x.args[0], ast.Constant)
            and isinstance(x.args[0].value, str)
        ):
            segs.append(x.args[0].value)
        else:
            segs.append(None)

    rec(node)
    return segs


def _const_str(tree: ast.AST, name: str) -> Optional[str]:
    for n in ast.walk(tree):
        if isinstance(n, ast.Assign) and isinstance(n.value, ast.Constant) and isinstance(n.value.value, str):
            for t in n.targets:
                if isinstance(t, ast.Name) and t.id == name:
                    return n.value.value
    return None


def _is_script_relpath(value: str) -> bool:
    return (
        any(value.startswith(p + "/") for p in _SCRIPT_PREFIXES)
        and value.endswith(".sh")
    )


def collect_relpaths(tree: ast.AST) -> List[Tuple[str, int]]:
    """Collect (relpath, lineno) for every repo-root ``scripts/``/``hooks/`` ``.sh``
    path the module names: full-string constants, segmented ``Path`` joins, and
    ``resolve_plugin_script("relpath")`` args. Docstring strings are skipped."""
    skip = _docstring_nodes(tree)
    found: List[Tuple[str, int]] = []

    for n in ast.walk(tree):
        # resolve_plugin_script("relpath") / resolve_plugin_script(NAME), direct
        # or attribute form (paths.resolve_plugin_script(...)).
        if (
            isinstance(n, ast.Call)
            and _called_name(n) == "resolve_plugin_script"
            and n.args
        ):
            arg = n.args[0]
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                if _is_script_relpath(arg.value):
                    found.append((arg.value, getattr(n, "lineno", 0)))
            elif isinstance(arg, ast.Name):
                v = _const_str(tree, arg.id)
                if v and _is_script_relpath(v):
                    found.append((v, getattr(n, "lineno", 0)))

        # full-string constants
        elif isinstance(n, ast.Constant) and isinstance(n.value, str) and id(n) not in skip:
            if _is_script_relpath(n.value):
                found.append((n.value, getattr(n, "lineno", 0)))

        # segmented Path-join chains
        elif isinstance(n, ast.BinOp) and isinstance(n.op, ast.Div):
            segs = _flatten_div(n)
            for i, s in enumerate(segs):
                if s in _SCRIPT_PREFIXES:
                    tail = segs[i:]
                    if None not in tail:
                        parts = [t for t in tail if t is not None]
                        if parts and parts[-1].endswith(".sh"):
                            found.append(("/".join(parts), getattr(n, "lineno", 0)))
                    break
    return found


# --------------------------------------------------------------------------- #
# Scan.
# --------------------------------------------------------------------------- #
def scan_tree(scan_root: Path, allowed: Set[str], repo_root: Path,
              exclude: Optional[Set[str]] = None) -> List[Violation]:
    """Scan every ``*.py`` under ``scan_root`` for un-allowlisted repo-root
    shell-outs. Raises GuardError on a SyntaxError (fail-closed)."""
    exclude = exclude if exclude is not None else SCAN_EXCLUDE
    if not scan_root.is_dir():
        raise GuardError(f"scan root not found: {scan_root}")
    violations: List[Violation] = []
    seen: Set[Tuple[str, str]] = set()  # (file, relpath) dedupe
    for py in sorted(scan_root.rglob("*.py")):
        try:
            rel = str(py.relative_to(repo_root))
        except ValueError:
            rel = str(py)
        if rel in exclude:
            continue
        src = py.read_text(encoding="utf-8", errors="replace")
        try:
            tree = ast.parse(src, filename=str(py))
        except SyntaxError as exc:
            raise GuardError(f"could not parse {rel}: {exc}") from exc
        if not _module_bash_execs(tree):
            continue
        if not _module_has_resolver_call(tree):
            continue
        for relpath, lineno in collect_relpaths(tree):
            if relpath in allowed:
                continue
            key = (rel, relpath)
            if key in seen:
                continue
            seen.add(key)
            violations.append(Violation(file=rel, line=lineno, relpath=relpath))
    return violations


# --------------------------------------------------------------------------- #
# Degrade proof.
# --------------------------------------------------------------------------- #
def _resolve_fno_cmd() -> List[str]:
    # The console script is `fno-py` (the Rust mux binary owns `fno`); resolve
    # it directly so the proof works without the front-door binary installed.
    found = shutil.which("fno-py")
    if found:
        return [found]
    sibling = Path(sys.executable).parent / "fno-py"
    if sibling.exists():
        return [str(sibling)]
    raise GuardError(
        "could not locate the `fno-py` executable for the degrade proof "
        "(not on PATH and not beside the interpreter); run via `uv run`"
    )


_TRACEBACK = "Traceback (most recent call last)"


def degrade_proof(entries: Dict[str, List[str]],
                  fno_cmd: Optional[List[str]] = None) -> List[DegradeFailure]:
    """Invoke each allowlisted verb in a no-script env and require a graceful
    degrade: exit != 0 and != 127, non-empty stderr, no traceback (AC4-EDGE)."""
    cmd_base = fno_cmd if fno_cmd is not None else _resolve_fno_cmd()
    failures: List[DegradeFailure] = []
    empty = Path(tempfile.mkdtemp(prefix="fno-shellout-drift-"))
    try:
        env = dict(os.environ)
        env.pop("CLAUDE_PLUGIN_ROOT", None)
        env["FNO_REPO_ROOT"] = str(empty)
        # Isolate the home dir too, so probing the verbs cannot read the user's
        # ~/.fno settings or write the persisted plugin-root pointer / history.
        # HOME + USERPROFILE cover Path.home() on POSIX and Windows; FNO_HOME is
        # fno's own override.
        env["FNO_HOME"] = str(empty)
        env["HOME"] = str(empty)
        env["USERPROFILE"] = str(empty)
        for relpath, verb in entries.items():
            try:
                proc = subprocess.run(
                    cmd_base + verb,
                    cwd=str(empty),
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=_DEGRADE_TIMEOUT,
                )
            except subprocess.TimeoutExpired:
                failures.append(DegradeFailure(relpath, verb, "timed out (hung instead of degrading)"))
                continue
            except OSError as exc:
                failures.append(DegradeFailure(relpath, verb, f"could not run verb: {exc}"))
                continue
            stderr = (proc.stderr or "").strip()
            combined = (proc.stderr or "") + (proc.stdout or "")
            rc = proc.returncode
            if rc == 0:
                failures.append(DegradeFailure(relpath, verb, "verb exited 0 (did not fail when its script was absent)"))
            elif rc == 127:
                failures.append(DegradeFailure(relpath, verb, "verb exited 127 (shell command-not-found, not a graceful degrade)"))
            elif not stderr:
                failures.append(DegradeFailure(relpath, verb, f"verb exited {rc} but printed nothing to stderr (silent)"))
            elif _TRACEBACK in combined:
                failures.append(DegradeFailure(relpath, verb, f"verb exited {rc} with a Python traceback (panic, not a degrade)"))
    finally:
        shutil.rmtree(empty, ignore_errors=True)
    return failures


# --------------------------------------------------------------------------- #
# Orchestration + reporting.
# --------------------------------------------------------------------------- #
def _repo_root() -> Path:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True
    )
    if result.returncode != 0 or not result.stdout.strip():
        raise GuardError("git rev-parse --show-toplevel failed; run from inside the repo")
    return Path(result.stdout.strip())


def run(
    *,
    repo_root: Optional[Path] = None,
    scan_root: Optional[Path] = None,
    allowlist_path: Optional[Path] = None,
    do_degrade: bool = True,
    fno_cmd: Optional[List[str]] = None,
    exclude: Optional[Set[str]] = None,
) -> Report:
    """Run the guard. Returns a Report(exit_code, lines). exit_code 0 = clean,
    1 = violations / degrade failures, 3 = guard error (fail-closed)."""
    try:
        rr = repo_root if repo_root is not None else _repo_root()
        sr = scan_root if scan_root is not None else (rr / SCAN_REL)
        al = allowlist_path if allowlist_path is not None else (rr / ALLOWLIST_REL)
        entries = parse_allowlist(al)
        allowed = set(entries)
        violations = scan_tree(sr, allowed, rr, exclude=exclude)
        degrade_failures = degrade_proof(entries, fno_cmd=fno_cmd) if do_degrade else []
    except GuardError as exc:
        return Report(3, [f"shellout-drift: GUARD ERROR (fail-closed, check is red): {exc}"])
    except Exception as exc:  # pragma: no cover - defensive fail-closed
        return Report(3, [f"shellout-drift: unexpected error (fail-closed, check is red): {exc!r}"])

    lines: List[str] = []
    for v in violations:
        lines.append(
            f"{v.file}:{v.line}: repo-root shell-out to '{v.relpath}' is not on the "
            f"CLONE_ONLY_SCRIPTS allowlist."
        )
        lines.append(
            "  Remedy 1 (preferred): eliminate the shell-out - fold the logic into "
            "fno-agents (Rust) or internalize it into the package (Python), so the verb "
            "no longer needs a repo-root script at runtime."
        )
        lines.append(
            f"  Remedy 2: if the verb is intentionally clone-only, add '{v.relpath}' to "
            f"{ALLOWLIST_REL} with a degrade-proof verb, and make the verb exit non-zero "
            "with an actionable message on a bare install."
        )
    for df in degrade_failures:
        lines.append(
            f"shellout-drift: allowlist entry '{df.relpath}' (verb: fno {' '.join(df.verb)}) "
            f"failed the degrade proof in a no-script env: {df.reason}."
        )
        lines.append(
            "  An allowlisted clone-only verb MUST degrade with a non-empty actionable "
            "message (exit != 0 and != 127, no traceback). Add an is_file()/exists() guard "
            "before the subprocess that prints the install-the-plugin message and returns "
            "a non-zero code."
        )

    if violations or degrade_failures:
        lines.insert(
            0,
            f"shellout-drift: FAIL - {len(violations)} un-allowlisted repo-root shell-out(s), "
            f"{len(degrade_failures)} degrade-proof failure(s).",
        )
        return Report(1, lines)

    lines.insert(
        0,
        "shellout-drift: ok - all repo-root shell-outs are on the CLONE_ONLY_SCRIPTS "
        "allowlist and every allowlisted verb degrades gracefully.",
    )
    return Report(0, lines)
