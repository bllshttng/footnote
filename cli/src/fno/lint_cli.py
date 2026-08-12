"""Repository lint commands exposed through ``fno lint``."""
from __future__ import annotations

import ast
import re
import subprocess
from pathlib import Path
from typing import Optional, cast

import click
import typer


app = typer.Typer(help="Repository lint checks", no_args_is_help=True)


# x-71b6 In-N-Out menu ratchet: the advertised command surface stays small.
# These are the two knobs a maintainer touches to widen the menu, on purpose,
# in a one-line diff that shows up in review - the display-surface counterpart
# of the control-plane LOC ratchet. New verbs default to hidden; promotion is a
# deliberate act that must fit under these caps.
MENU_CAP_TOP_LEVEL = 10
MENU_CAP_SUB_APP = 12


# Single-line argv shapes that are intentionally still owned by a provider or
# a legacy one-shot seam. New session spawns belong behind dispatch_spawn; a
# contributor adding a new shape must either migrate it or add the exact file
# here in the same change. This is deliberately a narrow grep-style guard, not
# an AST claim: multi-line assembly is documented coverage debt.
SPAWN_SHAPE_ALLOWLIST = frozenset(
    {
        "cli/src/fno/agents/dispatch.py",
        "cli/src/fno/agents/harnesses/claude.py",
        "cli/src/fno/agents/harnesses/codex.py",
        # Canonical tool-less one-shot judgment seam; x-81ad consolidated the
        # former inbox/graph/review call-site shapes here.
        "cli/src/fno/llm.py",
        "cli/src/fno/skill_diff/synthesize.py",
    }
)
_SPAWN_SHAPE_RE = re.compile(
    r"\[\s*['\"](?:claude|codex)['\"]"
    r"(?:\s*,\s*[^,\]]+)*"
    r"\s*,\s*['\"](?:--print|--bg|-p|--exec)['\"]"
)
# Shell-form single-line launches (`claude --bg "$prompt"`); .sh files only,
# where the argv-list form above can never appear.
_SHELL_SPAWN_RE = re.compile(r"\bclaude\s+(?:--print|--bg|-p)\b|\bcodex\s+(?:--exec|exec)\b")
_SOURCE_SUFFIXES = frozenset({".py", ".sh"})


def _spawn_shape_files(repo_root: Path) -> list[Path]:
    """Return production source files covered by the narrow spawn-shape scan."""
    roots = [repo_root / "cli" / "src" / "fno", repo_root / "scripts"]
    files: list[Path] = []
    for root in roots:
        if root.is_dir():
            files.extend(
                p
                for p in root.rglob("*")
                if p.is_file()
                and p.suffix in _SOURCE_SUFFIXES
                and "tests" not in p.parts
            )
    return sorted(files)


def _spawn_shape_violations(repo_root: Path) -> list[str]:
    violations: list[str] = []
    for path in _spawn_shape_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        if rel in SPAWN_SHAPE_ALLOWLIST:
            continue
        for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            match = _SPAWN_SHAPE_RE.search(line)
            if match is None and path.suffix == ".sh":
                match = _SHELL_SPAWN_RE.search(line)
            if match is not None:
                violations.append(
                    f"{rel}:{line_no}: hand-assembled session spawn shape "
                    f"{match.group(0)!r}"
                )
    return violations


@app.command("spawn-paths")
def spawn_paths() -> None:
    """Reject new single-line hand-assembled Claude/Codex session argv shapes.

    The allowlist is intentionally explicit and lives next to this lint. The
    scan does not claim to catch multi-line argv assembly; those sites remain
    census-backed migration work until they move behind ``dispatch_spawn``.
    """
    violations = _spawn_shape_violations(_repo_root())
    if violations:
        typer.echo("spawn-paths: violations:", err=True)
        for violation in violations:
            typer.echo(f"  {violation}", err=True)
        typer.echo(
            "\nFix: route session launches through fno agents spawn / "
            "dispatch_spawn, or add the exact source file to "
            "SPAWN_SHAPE_ALLOWLIST in cli/src/fno/lint_cli.py with a census-backed reason.",
            err=True,
        )
        raise typer.Exit(1)
    typer.echo("spawn-paths: ok")


def _visible_command_names(group: click.Group) -> list[str]:
    """Non-hidden subcommand names of a Click group, no module imports.

    Lazy top-level entries resolve to hidden-aware stubs, so this reads the
    curated surface straight from the registry the same way `fno --help` does.
    """
    ctx = click.Context(group)
    names: list[str] = []
    for name in group.list_commands(ctx):
        cmd = group.get_command(ctx, name)
        if cmd is not None and not cmd.hidden:
            names.append(name)
    return names


def _repo_root() -> Path:
    # The canonical cached, FNO_REPO_ROOT-aware resolver. This used to be a
    # third bare git-rev-parse copy (flock-pattern and shellout-drift already
    # call resolve_repo_root directly); the duplicate drifted without the env
    # hook and the worktree fallback, so spawn-paths/provider-stderr-merge got
    # different answers from their siblings. One resolver, same answer.
    from fno.paths import resolve_repo_root

    return resolve_repo_root()


@app.command("flock-pattern")
def flock_pattern(
    dispatch_path: Optional[Path] = typer.Option(
        None,
        "--dispatch-path",
        help="Override dispatch.py path for tests or targeted linting.",
    ),
) -> None:
    """Forbid open-coded agent flock + registry re-read patterns."""
    from fno.paths import resolve_repo_root

    script = resolve_repo_root() / "scripts" / "lint-flock-pattern.sh"
    if not script.is_file():
        typer.echo(
            "fno lint flock-pattern: this verb lints the repo's own source and "
            "needs the footnote checkout's lint scripts, which a bare "
            "`pip install fno` does not ship. Run it from a clone (or install "
            "the plugin).",
            err=True,
        )
        raise typer.Exit(2)
    argv = ["bash", str(script)]
    if dispatch_path is not None:
        argv.append(str(dispatch_path))
    result = subprocess.run(argv)
    raise typer.Exit(result.returncode)


def _is_subprocess_stdout(value: ast.AST) -> bool:
    return (
        isinstance(value, ast.Attribute)
        and value.attr == "STDOUT"
        and isinstance(value.value, ast.Name)
        and value.value.id == "subprocess"
    )


def _stdout_merge_lines(tree: ast.AST) -> list[int]:
    lines: list[int] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for keyword in node.keywords:
            if keyword.arg == "stderr" and _is_subprocess_stdout(keyword.value):
                lines.append(keyword.value.lineno)
    return lines


def _has_stdout_merge_justification(source_lines: list[str], line_no: int) -> bool:
    window_start = max(0, line_no - 12)
    prior_window = source_lines[window_start:line_no - 1]
    current_comment = source_lines[line_no - 1].partition("#")[2]
    window = "\n".join([*prior_window, current_comment]).lower()
    return (
        "locked decision" in window
        or "stderr=stdout" in window
        or "stderr=subprocess.stdout" in window
        or "stdout-merge" in window
    )


@app.command("provider-stderr-merge")
def provider_stderr_merge(
    providers_dir: Optional[Path] = typer.Option(
        None,
        "--providers-dir",
        help="Override provider directory for tests or targeted linting.",
    ),
) -> None:
    """Require justification for provider stderr/stdout pipe merging."""
    root = (
        providers_dir
        if providers_dir is not None
        else _repo_root() / "cli" / "src" / "fno" / "agents" / "harnesses"
    )
    if not root.is_dir():
        typer.echo(f"provider-stderr-merge: harness dir not found: {root}", err=True)
        raise typer.Exit(2)

    violations: list[str] = []
    for path in sorted(root.glob("*.py")):
        source = path.read_text(encoding="utf-8")
        tree = ast.parse(source, filename=str(path))
        source_lines = source.splitlines()
        for line_no in _stdout_merge_lines(tree):
            if not _has_stdout_merge_justification(source_lines, line_no):
                violations.append(
                    f"{path.name}:{line_no}: stderr=subprocess.STDOUT "
                    "requires nearby provider-specific justification"
                )

    if violations:
        typer.echo("provider-stderr-merge: violations:", err=True)
        for violation in violations:
            typer.echo(f"  {violation}", err=True)
        typer.echo(
            "\nFix: add a nearby Locked Decision/comment explaining why this "
            "provider may safely merge stderr into stdout, or drain stderr "
            "separately.",
            err=True,
        )
        raise typer.Exit(1)

    typer.echo("provider-stderr-merge: ok")


@app.command("shellout-drift")
def shellout_drift(
    no_degrade: bool = typer.Option(
        False,
        "--no-degrade",
        help="Skip the degrade proof (static scan only). Tests/diagnostics; CI runs the full check.",
    ),
) -> None:
    """Forbid repo-root shell-outs without a proven clone-only degrade path (US4).

    Scans cli/src/fno/ for verbs that bash-exec a resolve_repo_root()/
    resolve_plugin_script()-rooted script; every such script must be on the
    CLONE_ONLY_SCRIPTS allowlist (scripts/lint/.clone-only-scripts.txt) and each
    allowlisted verb must degrade gracefully on a bare install. Fail-closed.
    """
    from fno import lint_shellout_drift

    report = lint_shellout_drift.run(do_degrade=not no_degrade)
    stream_err = report.exit_code != 0
    for line in report.lines:
        typer.echo(line, err=stream_err)
    raise typer.Exit(report.exit_code)


@app.command("menu-caps")
def menu_caps() -> None:
    """Enforce the In-N-Out menu caps (x-71b6): <=10 advertised top-level verbs,
    <=12 advertised verbs per sub-app. New verbs default to hidden; promoting one
    past a cap fails here until it is hidden again or the cap constant is raised
    in a deliberate one-line diff. Introspects the registry - no repo scripts, so
    it runs from a bare install too.
    """
    import importlib

    import typer.main

    from fno.cli import LAZY_SUBCOMMANDS, app as root_app

    root = typer.main.get_command(root_app)
    top_visible = _visible_command_names(cast("click.Group", root))
    failures: list[str] = []

    if len(top_visible) > MENU_CAP_TOP_LEVEL:
        over = ", ".join(top_visible[MENU_CAP_TOP_LEVEL:])
        failures.append(
            f"top-level menu advertises {len(top_visible)} commands "
            f"(cap {MENU_CAP_TOP_LEVEL}); over the cap: {over}.\n"
            f"  Remedy 1: mark it hidden - add {{\"hidden\": True}} to its "
            f"LAZY_SUBCOMMANDS entry (or hidden=True on its @app.command).\n"
            f"  Remedy 2: raise MENU_CAP_TOP_LEVEL (a deliberate one-line diff)."
        )

    # Every group sub-app is capped, INCLUDING hidden top-level ones: opening
    # `fno mail --help` renders mail's own menu even though `mail` is hidden from
    # the top-level surface, so that menu must stay curated too. Iterate the whole
    # registry, not just the advertised entries. Dedupe by import target so an
    # alias (e.g. `graph` -> `backlog`) is checked once.
    seen_targets: set[str] = set()
    for name, entry in LAZY_SUBCOMMANDS.items():
        import_path = entry[0]
        if import_path in seen_targets:
            continue
        seen_targets.add(import_path)
        module_path, _, attr = import_path.rpartition(":")
        try:
            obj = getattr(importlib.import_module(module_path), attr, None)
        except Exception as exc:  # noqa: BLE001 - a lint must degrade, not crash
            typer.echo(f"menu-caps: skipped sub-app {name!r} (import failed: {exc})", err=True)
            continue
        if not isinstance(obj, typer.Typer):
            continue  # single-command entry, not a group
        sub_group = typer.main.get_command(obj)
        # Duck-type, not isinstance(click.Group): Typer bundles a vendored click
        # (typer._click), so a TyperGroup is NOT an instance of the top-level
        # `click.Group` - an isinstance check here silently skips every sub-app.
        if not hasattr(sub_group, "list_commands"):
            continue
        sub_visible = _visible_command_names(cast("click.Group", sub_group))
        if len(sub_visible) > MENU_CAP_SUB_APP:
            over = ", ".join(sub_visible[MENU_CAP_SUB_APP:])
            failures.append(
                f"sub-app `fno {name}` advertises {len(sub_visible)} verbs "
                f"(cap {MENU_CAP_SUB_APP}); over the cap: {over}.\n"
                f"  Remedy 1: mark it hidden - hidden=True on the @command / add_typer.\n"
                f"  Remedy 2: raise MENU_CAP_SUB_APP (a deliberate one-line diff)."
            )

    if failures:
        for f in failures:
            typer.echo(f"menu-caps: FAIL\n{f}", err=True)
        raise typer.Exit(1)
    typer.echo(f"menu-caps: ok (top-level {len(top_visible)}/{MENU_CAP_TOP_LEVEL})")


@app.command("verb-ratchet")
def verb_ratchet(
    update: bool = typer.Option(
        False,
        "--update",
        help="Regenerate scripts/ci/verb-baseline.txt from the live surface.",
    ),
) -> None:
    """Ratchet the REAL verb count.

    ``menu-caps`` caps what ``fno --help`` ADVERTISES; this caps what EXISTS.
    Fails when the live surface and ``scripts/ci/verb-baseline.txt`` disagree,
    naming the added or removed verbs. Covers BOTH binaries (the fno-py
    registry and the Rust front's mux + version surface) and fails closed with
    a named error - writing no baseline - when the Rust front cannot be reached,
    so the ratchet can never pass by reporting only half the surface. ``--update``
    regenerates the baseline after an intentional change.
    """
    from fno import lint_verb_ratchet as vr

    if update:
        try:
            leaves = vr.enumerate_all_leaves()
        except vr.VerbRatchetError as exc:
            typer.echo(str(exc), err=True)
            raise typer.Exit(1)
        path = vr.baseline_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(vr.generate(leaves), encoding="utf-8")
        typer.echo(f"verb-ratchet: regenerated {path.name} ({len(leaves)} leaves)")
        return
    try:
        report = vr.check()
    except vr.VerbRatchetError as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(1)
    typer.echo(report.message, err=not report.ok)
    raise typer.Exit(0 if report.ok else 1)


_STYLE_SURFACES = ("mail", "pr-body", "markdown")


@app.command("style", hidden=True)
def style(
    surface: str = typer.Option(
        "mail",
        "--surface",
        help="Where the text is read: mail, pr-body, or markdown. The five rules apply the same everywhere.",
    ),
    stdin: bool = typer.Option(
        False,
        "--stdin",
        help="Read the body from standard input.",
    ),
    files: Optional[list[Path]] = typer.Option(
        None,
        "--files",
        help="Files to check whole. With --diff-base, scopes the added-lines scan to these paths.",
    ),
    diff_base: Optional[str] = typer.Option(
        None,
        "--diff-base",
        help="For --surface markdown: check ADDED lines only since this ref (e.g. origin/main). "
        "A whole-file gate is unlandable because existing prose already breaks the rules.",
    ),
) -> None:
    """Check text against the five style rules in docs/style-rules.md.

    A list-item sentence is 20 words or fewer, and every other sentence is 25
    or fewer. No semicolon. No "should", "would", "may", "might", or "could".
    No contractions. If a sentence carries "if" or "when", that word starts the
    sentence. Code, paths, flags, and quoted output do not count. Exit 0 clean,
    1 with violations, 2 on bad usage.
    """
    from fno import style as style_mod

    if surface not in _STYLE_SURFACES:
        typer.echo(f"style: unknown surface {surface!r} (mail, pr-body, markdown)", err=True)
        raise typer.Exit(2)
    if diff_base is not None and surface != "markdown":
        typer.echo("style: --diff-base applies to --surface markdown only.", err=True)
        raise typer.Exit(2)

    if diff_base is not None:
        violations, inspected, changed, unexplained = _style_added_lines(
            diff_base, files
        )
        typer.echo(
            f"style: inspected {inspected} added line(s) across {changed} changed file(s)."
        )
        if unexplained:
            typer.echo(
                f"style: {unexplained} changed file(s) contributed no added lines "
                "and are not renames, deletions, or exception-marked. Add a "
                "style-exception marker if the change is deletion-only.",
                err=True,
            )
            raise typer.Exit(1)
    elif stdin:
        import sys

        text = sys.stdin.read()
        if style_mod.has_exception(text):
            raise typer.Exit(0)
        violations = style_mod.check(text, surface=surface)
    elif files:
        violations = []
        for path in files:
            text = path.read_text(encoding="utf-8")
            if style_mod.has_exception(text):
                continue
            violations.extend(style_mod.check(text, surface=surface))
    else:
        typer.echo("style: pass --stdin, --files, or --diff-base.", err=True)
        raise typer.Exit(2)

    if not violations:
        raise typer.Exit(0)
    typer.echo(style_mod.format_violations(violations), err=True)
    raise typer.Exit(1)


# One spell of every rename and quoting knob this module needs, and ONE form of
# rename detection.
#
# Three call sites hand-pinned these in two different spellings, kept in step by
# a comment reading "pinned identically to the pass below". That is the
# snapshot-not-invariant signature: a test can pin that detection is ON at each
# site, which is the outcome, and cannot pin that one thing decides HOW. It also
# misfires, since `-M` is git's own alias for `--find-renames`, so an
# outcome-shaped test flags a correct call as undetected.
#
# `diff.renames=true` because a caller inheriting `false` splits a rename into
# separate D and A entries, so a moved doc bills as authored prose.
# `diff.renameLimit=0` because past the limit git skips the exhaustive pass,
# warns on stderr, and exits 0: measured at limit 1 on this branch, 9 of 21
# renames found with a clean exit code. `core.quotePath=false` because git
# C-quotes any non-ASCII path into a display string that fails is_file(), and
# the file is skipped without a word.
_DIFF_PINS = (
    "-c", "diff.renames=true",
    "-c", "diff.renameLimit=0",
    "-c", "core.quotePath=false",
)


def _pinned_diff_argv(*tail: str) -> "list[str]":
    """The ONLY way this module spells a rename-aware, quote-safe `git diff`.

    Rename detection comes from `_DIFF_PINS` alone. No call site passes
    `--find-renames` or `-M`, so there is one mechanism to reason about rather
    than a per-site choice a reader has to diff by eye.
    """
    return ["git", *_DIFF_PINS, "diff", *tail]


def _run_git(argv: "list[str]", repo: Path, *, label: str):
    """Run git, and refuse to let a failure read as a clean result.

    Every git call in this module answers with stdout alone, and empty stdout is
    also the clean answer, so an unchecked return code turns any git error into
    a green gate. Four sites hand-rolled this same guard and one of the four
    had no test at all, which is the shape that lets a fifth call ship without
    it. Routing every call through here is what makes the guard unforgettable
    rather than remembered.
    """
    proc = subprocess.run(argv, cwd=str(repo), capture_output=True, text=True)
    if proc.returncode != 0:
        typer.echo(f"style: {label} failed: {proc.stderr.strip()}", err=True)
        raise typer.Exit(2)
    return proc


def _repo_scope(paths: list[Path], repo: Path) -> "list[str]":
    """Repo-root-relative POSIX pathspecs for git, from caller-relative paths.

    Callers pass paths relative to THEIR cwd while every git call here runs from
    the repo root, so the two disagree the moment the caller is not standing at
    the root. Measured: `--files ../docs` from `cli/` matched nothing and the
    gate exited 0 over 27 changed files, while the same scope from the root
    inspected 569 added lines. Absence read as success, one directory off.

    A path outside the repository fails loud rather than silently matching
    nothing, for the same reason.
    """
    root = repo.resolve()
    out = []
    for p in paths:
        try:
            out.append(Path(p).resolve().relative_to(root).as_posix())
        except ValueError:
            typer.echo(
                f"style: --files path is outside the repository: {p}", err=True
            )
            raise typer.Exit(2)
    return out


def _style_added_lines(
    diff_base: str, paths: Optional[list[Path]]
) -> "tuple[list, int, int, int]":
    """Return (violations, added-line count, changed-file count, unexplained).

    Per file: a whole-file style-exception marker exempts it; otherwise only the
    ADDED lines since diff_base are checked. A bad diff-base fails loud (exit 2),
    not open: a malformed base that inspects nothing is the absence the pitfalls
    corpus names, indistinguishable from "no violations found".

    ``unexplained`` counts changed files where GIT counted added lines and this
    parser found none. That is an instrument failure and nothing else. Counting
    bare zeros instead swept in every legitimate zero: a pure rename, a
    deletion-only trim, a mode change. Two of those were measured failing real
    PRs, and the rename-resolution fix directly above is what creates the first.
    """
    from fno import style as style_mod

    repo = _repo_root()
    scope = _repo_scope(paths, repo) if paths else ["docs", "skills", "agents"]
    _run_git(
        ["git", "rev-parse", "--verify", diff_base],
        repo,
        label=f"bad diff-base {diff_base!r}",
    )
    diff_files = _run_git(
        _pinned_diff_argv("--name-only", f"{diff_base}...HEAD", "--", *scope),
        repo,
        label=f"listing changed files ({diff_base}...HEAD)",
    )
    # Pre-rename paths, keyed by new path. Resolved from an UNSCOPED name-status
    # pass because rename detection needs both sides visible, which a per-file
    # pathspec denies it.
    renames: dict[str, str] = {}
    name_status = _run_git(
        _pinned_diff_argv("--name-status", f"{diff_base}...HEAD"),
        repo,
        label=f"rename detection ({diff_base}...HEAD)",
    )
    # Backstop for the degradation the pin is meant to prevent, kept because a
    # silent partial answer here mis-bills every moved doc as authored prose.
    if "rename detection was skipped" in name_status.stderr:
        typer.echo(
            "style: git skipped rename detection despite the pinned limit "
            f"({name_status.stderr.strip()}); refusing to bill moved files as "
            "authored prose",
            err=True,
        )
        raise typer.Exit(2)
    for line in name_status.stdout.splitlines():
        parts = line.split("\t")
        if len(parts) == 3 and parts[0].startswith("R"):
            renames[parts[2]] = parts[1]
    # Added-line counts straight from git, which is what the absence guard below
    # actually rests on. Asking "did we inspect zero" cannot tell a broken parser
    # from a file that authored nothing, and a deletion-only edit authors
    # nothing. Measured before this: one trimmed doc beside one normal doc failed
    # the whole gate, demanding a style-exception marker in a file the author had
    # only shortened. A mode-only change did the same.
    numstat = _run_git(
        _pinned_diff_argv("--numstat", f"{diff_base}...HEAD"),
        repo,
        label=f"counting changed lines ({diff_base}...HEAD)",
    )
    added_by_path: dict[str, int] = {}
    for line in numstat.stdout.splitlines():
        parts = line.split("\t")
        if len(parts) == 3 and parts[0].isdigit():
            added_by_path[parts[2]] = int(parts[0])
    # Markdown only: the gate is "changed markdown", so a PR adding a shell or
    # Python file under skills/ is not style-checked as prose.
    changed = [
        line
        for line in diff_files.stdout.splitlines()
        if line.strip() and line.endswith(".md")
    ]
    violations = []
    inspected = 0
    unexplained = 0
    for rel in changed:
        full = repo / rel
        if not full.is_file():
            continue
        whole = full.read_text(encoding="utf-8")
        if style_mod.has_exception(whole):
            continue
        nums = _git_added_line_nums(rel, diff_base, repo, renames.get(rel))
        inspected += len(nums)
        if nums:
            # Mask the WHOLE file and check only the added lines, so an added
            # line inside an existing fenced block is masked as code and skipped.
            violations.extend(style_mod.check_lines(whole, nums))
        elif added_by_path.get(rel, 0) > 0:
            # git counted added lines for this path and the parser found none.
            # That is the only shape here that means the INSTRUMENT failed, and
            # it is the shape the guard was always meant to catch.
            unexplained += 1
    return violations, inspected, len(changed), unexplained


def _git_added_line_nums(
    rel: str, diff_base: str, repo: Path, old_rel: Optional[str] = None
) -> "set[int]":
    """Return 1-based line numbers (in the new file) of added ('+') lines from
    ``git diff -U0 <base>...HEAD -- <rel>``. Position advances on context and
    added lines, not on deleted lines, matching how the new file is laid out.

    ``old_rel`` is the pre-rename path, and passing it is what keeps a MOVED
    file from reading as an authored one. Restricting the pathspec to the new
    path alone hides the rename source from git's detection, so the file comes
    back as ``new file mode`` and every pre-existing line counts as added. A
    pure ``git mv`` of a doc then bills its whole body to whoever moved it.
    """
    pathspec = [rel] if old_rel is None else [rel, old_rel]
    proc = _run_git(
        _pinned_diff_argv("-U0", f"{diff_base}...HEAD", "--", *pathspec),
        repo,
        label=f"reading added lines for {rel}",
    )
    nums: set[int] = set()
    pos = 0
    for line in proc.stdout.splitlines():
        if line.startswith("@@"):
            match = re.search(r"\+(\d+)", line)
            pos = int(match.group(1)) if match else pos
        elif line.startswith("+++") or line.startswith("---"):
            continue
        elif line.startswith("\\"):
            # `\ No newline at end of file`. It is a NOTE about the adjacent
            # line, never a line of the file, so counting it as context shifts
            # every added line after it by one. Measured with the marker
            # mid-hunk: added lines reported as [2, 3] where they are [1, 2],
            # so line 1 was never checked and line 3 does not exist. The
            # escaped line carried a semicolon, meaning a real violation ships
            # while a nonexistent line gets style-checked in its place.
            continue
        elif line.startswith("+"):
            nums.add(pos)
            pos += 1
        elif line.startswith("-"):
            continue
        else:
            pos += 1
    return nums


@app.command("stale-skill-refs")
def stale_skill_refs() -> None:
    """Audit for stale references to cut, demoted, or merged skills.

    Re-homed from the retired `fno consolidation audit` (x-71b6): a lint gate
    wearing a command costume belongs under `fno lint`. Thin wrapper over the
    source-of-truth bash gate scripts/ci/check-no-stale-skill-refs.sh; exit code
    matches it (0 clean, 1 stale references, 2 script error).
    """
    from fno._subprocess_util import propagate_returncode
    from fno.paths import resolve_repo_root

    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "ci" / "check-no-stale-skill-refs.sh"
    if not script.exists():
        typer.echo(f"audit script not found at {script}", err=True)
        raise typer.Exit(code=2)
    try:
        result = subprocess.run(["bash", str(script)], cwd=repo_root)
    except FileNotFoundError as exc:
        typer.echo(f"failed to run audit script: {exc}", err=True)
        raise typer.Exit(code=2)
    raise typer.Exit(code=propagate_returncode(result.returncode))
