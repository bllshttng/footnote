"""fno worktree - thin lifecycle wrapper.

The actual git-worktree create/remove operations happen via Claude Code's
native EnterWorktree/ExitWorktree tools (or `git worktree add` directly).
This CLI exposes the bookkeeping subset of the old git-worktrees skill:
listing active worktrees with target status, cleaning up stale ones, and
archiving (remove directory, keep branch).

`ensure` is the mechanical dispatch-time primitive (node x-73ca): the
deterministic-isolation behaviour PR #29 gave the bash spawn path, exposed as
a CLI verb so the two Rust-intercepted code-dispatch callers (`dispatch-node.sh`
and `/do`'s foreign-wave prose) can shell it. It lives here, NOT under
`fno agents` (Rust-intercepted runtime), so the default install can reach it.
"""
import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.paths import resolve_repo_root

app = typer.Typer(
    name="worktree",
    help="Worktree lifecycle: status, cleanup, archive.",
    no_args_is_help=True,
)


def _run_lifecycle(*args: str) -> int:
    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "lib" / "worktree-lifecycle.sh"
    if not script.exists():
        typer.echo(f"worktree-lifecycle script not found at {script}", err=True)
        return 2
    try:
        # cwd=repo_root so the lifecycle script's relative paths (notably
        # .claude/worktrees/<name>) resolve against the git root even when
        # the user invokes `fno worktree archive` from a subdirectory.
        # Without this, valid worktrees were reported as missing (Codex P2).
        result = subprocess.run(["bash", str(script), *args], cwd=str(repo_root))
    except FileNotFoundError as exc:
        typer.echo(f"failed to run worktree-lifecycle: {exc}", err=True)
        return 2
    return propagate_returncode(result.returncode)


@app.command()
def status() -> None:
    """List active worktrees with branch, last-commit age, and target status."""
    raise typer.Exit(code=_run_lifecycle("status"))


@app.command()
def cleanup(
    older_than: Optional[str] = typer.Option(
        None,
        "--older-than",
        help="Remove worktrees with no commits in N days (e.g. '7d' or '7').",
    ),
    dry_run: bool = typer.Option(False, "--dry-run", "-N", help="Show what would be removed."),
    prefix: Optional[str] = typer.Option(
        None, "--prefix", help="Restrict to worktrees whose branch starts with this prefix."
    ),
    merged: bool = typer.Option(
        False,
        "--merged",
        help="Reap worktrees whose branch tip already landed in origin/main "
        "(clean + pushed + no live session). Dry-run by default; pass --apply to execute.",
    ),
    apply: bool = typer.Option(
        False, "--apply", help="With --merged, actually archive (default is dry-run)."
    ),
    kill_orphans: bool = typer.Option(
        False,
        "--kill-orphans",
        help="With --merged, SIGTERM ppid-1 orphan processes squatting in a "
        "candidate worktree instead of skipping it. Live process trees are never killed.",
    ),
) -> None:
    """Remove stale worktrees with no active target session.

    Two selection modes (mutually exclusive): --older-than (commit age) or
    --merged (branch already merged into origin/main).
    """
    if merged and older_than:
        typer.echo("worktree cleanup: --merged and --older-than are mutually exclusive", err=True)
        raise typer.Exit(code=1)
    args = ["cleanup"]
    if older_than:
        args.extend(["--older-than", older_than])
    if dry_run:
        args.append("--dry-run")
    if prefix:
        args.extend(["--prefix", prefix])
    if merged:
        args.append("--merged")
    if apply:
        args.append("--apply")
    if kill_orphans:
        args.append("--kill-orphans")
    raise typer.Exit(code=_run_lifecycle(*args))


@app.command()
def archive(name: str = typer.Argument(..., help="Worktree branch or path to archive.")) -> None:
    """Remove the worktree directory but keep the branch."""
    raise typer.Exit(code=_run_lifecycle("archive", name))


# --- ensure: mechanical dispatch-time isolation primitive (x-73ca) ----------


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True
    )


def _abs_git_path(repo: Path, which: str) -> Optional[Path]:
    """Absolute path of a repo's git-dir or git-common-dir (symlink-resolved)."""
    out = _git(repo, "rev-parse", f"--{which}")
    if out.returncode != 0 or not out.stdout.strip():
        return None
    p = Path(out.stdout.strip())
    if not p.is_absolute():
        p = repo / p
    return p.resolve()


def _base_ref(repo: Path) -> Optional[str]:
    """The ref a fresh dispatch branch is based on. Prefer origin/main so the
    worker never inherits the dispatcher's stale-ahead local HEAD (the
    phantom-deletion bug this verb retires, Locked Decision 5). Falls back to
    HEAD (None) only when no remote-tracking main exists (e.g. a local repo)."""
    for ref in ("origin/main", "origin/master"):
        if _git(repo, "rev-parse", "--verify", "--quiet", ref).returncode == 0:
            return ref
    return None


def _worktree_ensure(
    repo: str, name: str, branch: Optional[str], harness: Optional[str] = None
) -> int:
    """Idempotently ensure `<worktrees_base>/<repo>/<name>` exists.

    On success prints the worktree path on stdout (exit 0). On ANY failure
    prints a reason on stderr and NOTHING on stdout (non-zero), so a caller's
    `wt=$(fno worktree ensure ...)` reads empty and falls back to its prior
    cwd -- the dispatch is never blocked. Mechanism only: the caller owns the
    "is this a code payload / a main checkout" policy.

    Step 0 is the per-project policy gate: `never` launches in place (repo root
    on stdout, exit 0), and a parse error / out-of-enum value REFUSES creation
    (empty stdout, exit 1) so a misconfig fails closed, never auto-isolating.
    """
    repo_path = Path(repo)
    top_out = _git(repo_path, "rev-parse", "--show-toplevel")
    if top_out.returncode != 0 or not top_out.stdout.strip():
        typer.echo(f"worktree ensure: {repo} is not a git repository", err=True)
        return 1
    top = Path(top_out.stdout.strip()).resolve()

    # Step 0: policy gate, before any worktree operation. A `never` project
    # (e.g. an Obsidian vault whose working tree IS the product) launches in
    # place; a parse error / out-of-enum value refuses (fail closed).
    from fno.worktree_paths import WorktreePolicyError, resolve_worktree_policy

    try:
        pol = resolve_worktree_policy(top, harness)
    except WorktreePolicyError as exc:
        typer.echo(f"worktree ensure: {exc}", err=True)
        return 1
    if pol.policy == "never":
        typer.echo(str(top))  # repo main-checkout path -> caller launches here
        typer.echo(
            f"worktree ensure: policy=never ({pol.project}); launching in place",
            err=True,
        )
        return 0

    # Only a MAIN checkout may spawn a worktree (git-dir == git-common-dir);
    # a linked worktree has no business nesting another.
    gdir = _abs_git_path(top, "git-dir")
    common = _abs_git_path(top, "git-common-dir")
    if gdir is None or common is None or gdir != common:
        typer.echo(
            f"worktree ensure: {top} is a linked worktree, not a main checkout; refusing to nest",
            err=True,
        )
        return 1

    wt = pol.base / top.name / name

    # Idempotent reuse: an existing registered worktree rooted at wt -> reuse.
    if wt.exists():
        inside = _git(wt, "rev-parse", "--is-inside-work-tree")
        wt_top = _git(wt, "rev-parse", "--show-toplevel")
        if (
            inside.returncode == 0
            and inside.stdout.strip() == "true"
            and wt_top.returncode == 0
            and Path(wt_top.stdout.strip()).resolve() == wt.resolve()
        ):
            typer.echo(str(wt))
            return 0
        # Exists but is NOT our worktree: never clobber a stray dir.
        typer.echo(
            f"worktree ensure: {wt} exists but is not a worktree; not clobbering",
            err=True,
        )
        return 1

    wt.parent.mkdir(parents=True, exist_ok=True)
    br = branch or f"feature/{name}"
    if _git(top, "show-ref", "--verify", "--quiet", f"refs/heads/{br}").returncode == 0:
        # Branch already exists (e.g. a re-dispatch after archive) -> check it out.
        add = _git(top, "worktree", "add", str(wt), br)
    else:
        base = _base_ref(top)
        add_args = ["worktree", "add", str(wt), "-b", br]
        if base:
            add_args.append(base)
        add = _git(top, *add_args)
    if add.returncode != 0:
        typer.echo(
            f"worktree ensure: git worktree add failed: {add.stderr.strip() or add.stdout.strip()}",
            err=True,
        )
        return 1

    # Portable git mechanism only. Linking footnote-ecosystem shared state
    # (.fno / internal / .claude) via scripts/setup/setup-worktree.sh is the
    # CALLER's job, not this package verb's: a `pip install fno` ships no
    # repo-root scripts, so a shell-out here trips the shellout-drift gate
    # (and forcing it to fail-on-bare-install would break the "best-effort,
    # never fail the ensure" contract). The in-repo skill callers
    # (dispatch-node.sh, spawn.sh, /do) run setup-worktree.sh after ensure.
    typer.echo(str(wt))  # the ONLY stdout line -> the caller's $wt
    return 0


@app.command()
def ensure(
    repo: str = typer.Option(..., "--repo", help="Repo MAIN checkout to spawn a worktree from."),
    name: str = typer.Option(..., "--name", help="Worktree name (dir + default branch suffix)."),
    branch: Optional[str] = typer.Option(
        None, "--branch", help="Branch to create/checkout (default: feature/<name>)."
    ),
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Resolved harness (claude/codex/...); drives the policy gate."
    ),
) -> None:
    """Ensure a worktree for <name>; print its path (mechanism-only).

    Used by dispatch callers to deterministically isolate a code worker instead
    of relying on the worker to self-isolate in-session. Failure is non-fatal:
    exits non-zero with empty stdout so the caller falls back to its prior cwd.
    A `never`-policy project exits 0 with the repo root on stdout (launch here).
    """
    raise typer.Exit(code=_worktree_ensure(repo, name, branch, harness))


@app.command()
def policy(
    repo: str = typer.Option(..., "--repo", help="Repo MAIN checkout to resolve policy for."),
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Resolved harness (claude/codex/...); drives harness-native degradation."
    ),
) -> None:
    """Print the resolved worktree policy for <repo>. Read-only.

    Line 1 is the policy (never|harness-native|external); for a non-never result
    line 2 is `base=<worktrees-base>`. Shares the SAME resolver `ensure` uses, so
    bash callers get the identical verdict with no second precedence impl. A
    parse error / out-of-enum value exits 1 with the reason on stderr.
    """
    from fno.worktree_paths import WorktreePolicyError, resolve_worktree_policy

    try:
        pol = resolve_worktree_policy(Path(repo).resolve(), harness)
    except WorktreePolicyError as exc:
        typer.echo(f"worktree policy: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(pol.policy)
    if pol.policy != "never":
        typer.echo(f"base={pol.base}")
    raise typer.Exit(0)
