"""Shared filesystem resolution helpers.

Kept deliberately import-free (no other ``fno`` imports) so this
module can be loaded from any subcommand or test without pulling in
the Typer app. Avoids the circular-import hazard that previously
forced state/cli.py to duplicate cli.py's repo-root resolver.

Typed path resolver - single source of truth for every user-configurable
path.

Cache-once-per-process: settings are loaded on first access and reused
for the lifetime of the process. Mid-process edits to settings.yaml do
not take effect; the next subprocess sees the new value.
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from functools import cache
from pathlib import Path
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from fno.config import SettingsModel


def _warn_if_foreign_abi_repo_root(resolved: Path) -> None:
    """One-line heads-up when ``FNO_REPO_ROOT`` pins the fno plugin root
    while the cwd is a *different* git repo.

    ``FNO_REPO_ROOT`` is overloaded historically: operators reached for it to
    fix an events-schema-resolution miss, but it ALSO repoints ``fno config
    get`` / project lookup (this very resolver). Setting it to the fno
    checkout from inside another repo silently makes config reads hit the wrong
    project - a failure with no error. The schema resolver now self-locates its
    bundled schema (scripts/lib/events-validate.sh), removing the reason to set
    ``FNO_REPO_ROOT`` that way; this warning catches the lingering footgun.

    Best-effort and non-fatal: any failure is swallowed so path resolution
    never breaks. Fires at most once per process (``resolve_repo_root`` is
    cached), and only when the pinned root is the fno PLUGIN root (by its
    marker file, not its directory basename - a clone/worktree can be named
    anything) AND the cwd resolves to a different git repo. (ab-fe825805 change 4)
    """
    try:
        # Identify the fno plugin by its marker file rather than the
        # directory basename (a clone/worktree may be named anything). Only the
        # plugin root is the footgun that silently repoints `fno config get`.
        if not _is_plugin_root(resolved):
            return
        # Subprocess-free short-circuit: if cwd is inside the pinned root it is
        # the same checkout (no footgun). This keeps the git probe out of the
        # hot path when running inside the fno repo, and - importantly -
        # out of unit tests that globally stub subprocess.run while resolving
        # paths (resolve_repo_root is on the CLI-wrapper hot path).
        try:
            Path.cwd().resolve().relative_to(resolved)
            return  # cwd within resolved -> same repo
        except (ValueError, OSError):
            pass
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
            timeout=2,  # diagnostic-only: never block a CLI invocation on a slow FS
        )
        # Defensive reads: in the stubbed-subprocess test contexts above the
        # result may not be a real CompletedProcess. Treat a missing/garbled
        # result as "cwd unknown" -> no warning, never a crash.
        if getattr(result, "returncode", 1) != 0:
            return
        stdout = getattr(result, "stdout", "") or ""
        if not stdout.strip():
            return
        cwd_root = Path(stdout.strip()).resolve()
        if cwd_root == resolved:
            return
        print(
            f"fno: warning: FNO_REPO_ROOT pins project/config resolution to "
            f"{resolved}, but cwd is a different repo ({cwd_root}); "
            f"`fno config get` will read the fno project, not this repo. "
            f"Unset FNO_REPO_ROOT unless that is intended.",
            file=sys.stderr,
        )
    except (FileNotFoundError, OSError, subprocess.SubprocessError):
        return


@cache
def resolve_repo_root() -> Path:
    """Resolve the repo root for state + artifact path resolution.

    Cached once per process. The ``FNO_REPO_ROOT`` env var is read at
    first call and frozen; changing it mid-process (e.g. in tests) requires
    ``fno.paths.resolve_repo_root.cache_clear()`` before the next call.

    Order: ``FNO_REPO_ROOT`` env var, ``git rev-parse --show-toplevel``,
    then cwd. The env var is the test hook; the git fallback handles
    users running ``fno`` from a subdirectory; cwd is the last resort.
    """
    env_root = os.environ.get("FNO_REPO_ROOT")
    if env_root:
        resolved = Path(env_root).resolve()
        _warn_if_foreign_abi_repo_root(resolved)
        return resolved
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            return Path(result.stdout.strip()).resolve()
    except (FileNotFoundError, OSError):
        pass
    return Path.cwd()


def resolve_canonical_worktree(
    cwd: Optional[Path] = None, *, timeout: Optional[float] = None
) -> Optional[Path]:
    """Resolve the canonical (main) WORKING TREE from ``git worktree list``.

    Returns the first porcelain record whose path is a real working tree (has a
    ``.git`` child), as the *unresolved* ``Path`` git emitted - callers apply
    their own normalization (``.resolve()`` / ``os.path.normpath``) and their
    own final fallback. Returns ``None`` when no usable working tree is found,
    so each caller falls through to its existing resolver.

    Shared by :func:`resolve_canonical_repo_root`,
    ``graph._intake._git_repo_root``, and ``inbox.drain._resolve_memory_dir`` so
    the bare / separate-git-dir handling cannot drift between three copies.

    - **Bare repos** (``git clone --bare``) are listed first with a ``bare``
      marker and have no working tree -> skipped. A bare repo with a linked
      worktree returns that worktree; a bare-only repo returns ``None``.
    - **separate-git-dir** (``git init --separate-git-dir``): git reports the
      EXTERNAL git dir as the first ``worktree`` path. A git dir has no ``.git``
      child, so the entry is skipped and ``None`` is returned (the caller's
      ``--show-toplevel`` fallback yields the real checkout) - the git dir is
      never returned. ``core.worktree`` recovery was rejected (empirically empty
      for ``--separate-git-dir``). git's porcelain paths are already
      symlink-resolved, so the ``.git`` probe is reliable.
    """
    try:
        result = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            cwd=str(cwd) if cwd is not None else None,
            capture_output=True,
            text=True,
            # Force UTF-8: git emits porcelain paths as UTF-8, but ``text=True``
            # alone decodes with the locale codec, so a non-ASCII repo path under
            # ``LC_ALL=C`` would raise UnicodeDecodeError before the fallback
            # (codex P2 on PR #406; matches the prior _intake forcing).
            encoding="utf-8",
            check=False,
            timeout=timeout,
        )
    except (FileNotFoundError, OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0 or not result.stdout.strip():
        return None
    # Porcelain records are blank-line separated; each starts with
    # ``worktree <path>`` followed by attribute lines (``bare``, ``HEAD`` ...).
    # The FIRST non-bare record decides: git lists the main worktree first. A
    # real working tree has a ``.git`` child; a ``--separate-git-dir`` mis-report
    # (the external git dir listed as the main worktree) does NOT. Return None
    # for the git dir rather than continuing to a later linked worktree, so the
    # caller falls back to ``--show-toplevel`` (the CURRENT checkout) instead of
    # rooting config / backlog / memory under an arbitrary sibling worktree
    # (codex P2 on PR #406).
    for record in result.stdout.strip().split("\n\n"):
        lines = record.splitlines()
        if not lines or not lines[0].startswith("worktree "):
            continue
        if any(line.strip() == "bare" for line in lines[1:]):
            continue
        candidate = Path(lines[0][len("worktree ") :])
        return candidate if (candidate / ".git").exists() else None
    return None


def resolve_canonical_repo_root() -> Path:
    """Resolve the CANONICAL (main worktree) repo root, for project-level
    *config* lookup only.

    From inside a linked worktree this returns the *main* checkout, so a
    worktree can read the project-local ``settings.yaml`` that lives in
    canonical's ``.fno/`` with zero per-worktree setup instead of falling
    straight through to global config.

    Deliberately NOT used for state / artifact / session paths: those keep
    :func:`resolve_repo_root` (``--show-toplevel``) so per-session state
    (``target-state.md``, ``artifacts/``) stays bound to its own worktree. Only
    config and cross-worktree coordination state climb to canonical. Claims
    (``fno.claims``) are in the coordination category: a node claim must
    be visible from every worktree, so ``claims_dir()`` also resolves here.
    Mirrors ``graph._intake._git_repo_root``, whose docstring draws the same
    durable-vs-ephemeral line.

    Order: ``FNO_REPO_ROOT`` env var (test hook, same as
    :func:`resolve_repo_root`), the main working tree from
    :func:`resolve_canonical_worktree`, then :func:`resolve_repo_root` as the
    fallback (also covers bare / separate-git-dir layouts the helper returns
    ``None`` for).

    Uncached on purpose: it is called once per process from inside the
    ``lru_cache``-d :func:`fno.config.load_settings`, so caching buys
    nothing, and staying uncached keeps test isolation simple (no extra
    ``cache_clear`` plumbing) and honors mid-process ``FNO_REPO_ROOT`` changes.
    """
    env_root = os.environ.get("FNO_REPO_ROOT")
    if env_root:
        return Path(env_root).resolve()
    canonical = resolve_canonical_worktree()
    if canonical is not None:
        return canonical.resolve()
    return resolve_repo_root()


# ---------------------------------------------------------------------------
# Settings cache (lazy import to avoid circular-import hazard at module load)
# ---------------------------------------------------------------------------


@cache
def _settings() -> "SettingsModel":
    """Load and cache settings for the lifetime of this process."""
    from fno.config import load_settings
    return load_settings()


# ---------------------------------------------------------------------------
# Template substitution
# ---------------------------------------------------------------------------

# Matches {word} but NOT {{ or }} (escape sequences)
_TEMPLATE_VAR = re.compile(r"(?<!\{)\{([^{}]+)\}(?!\})")


def _resolve(raw: str, project_root: Optional[Path] = None) -> Path:
    """Expand ~, $VAR, {vault}, {project}; absolutize via .resolve().

    Template rules:
      - {vault} -> obsidian.vault value (requires obsidian.enabled)
      - {project} -> project.id or git repo basename (requires git or project.id)
      - {{ -> { and }} -> } (escape sequences, processed last)
      - Unknown {foo} -> ValueError

    Processing order:
      1. os.path.expandvars  (POSIX $VAR substitution)
      2. Template variable substitution ({vault}, {project}, unknown -> error)
      3. Unescape {{ -> { and }} -> }
      4. os.path.expanduser (~)
      5. Path(...).resolve()
    """
    settings = _settings()

    # 1. expandvars
    expanded = os.path.expandvars(raw)

    # 2. Substitute known template variables; reject unknown
    def _substitute(match: re.Match[str]) -> str:
        var = match.group(1)
        if var == "vault":
            if not settings.config.obsidian.enabled:
                raise ValueError(
                    "Path template uses {vault} but obsidian.enabled is false. "
                    "Either set obsidian.enabled: true or rewrite the path."
                )
            vault = settings.config.obsidian.vault
            if not vault:
                raise ValueError(
                    "Path template uses {vault} but obsidian.vault is not set."
                )
            # Resolve via vault_root() so there is a single implementation
            # of the vault semantics (bare name like 'myvault' maps to
            # ~/<name>; absolute and ~-prefixed values honored as-is).
            # Returning the raw relative value would leave the assembled
            # path relative, anchoring it at project_root/CWD - in a
            # worktree that lands handoffs at a junk worktree-local path
            # (ab-347f6482).
            vroot = vault_root()
            if vroot is None:  # unreachable: enabled + vault guarded above
                raise ValueError(
                    "Path template uses {vault} but the vault could not be resolved."
                )
            return str(vroot)
        elif var == "project":
            # config.project.id is canonical (the loader aliases a legacy
            # top-level project.id into it, so this covers old files too).
            pid = settings.config.project.id
            if pid:
                return pid
            # Use the repo root's directory basename as the project name.
            # resolve_repo_root() already ran git rev-parse so root IS the toplevel;
            # a redundant git call here is unnecessary.
            root = project_root or resolve_repo_root()
            return root.name
        else:
            raise ValueError(
                f"Unknown template variable {{{var}}} in path {raw!r}. "
                "Recognized variables: {vault}, {project}."
            )

    substituted = _TEMPLATE_VAR.sub(_substitute, expanded)

    # 3. Unescape {{ -> { and }} -> }
    unescaped = substituted.replace("{{", "{").replace("}}", "}")

    # 4. expanduser
    expanded_user = os.path.expanduser(unescaped)

    # 5. Anchor relative paths to project_root (if supplied) rather than CWD
    path = Path(expanded_user)
    if not path.is_absolute() and project_root:
        path = project_root / path
    return path.resolve()


# ---------------------------------------------------------------------------
# Typed path accessors
# ---------------------------------------------------------------------------


def state_dir() -> Path:
    """Return the state directory (default: ~/.fno/)."""
    settings = _settings()
    return _resolve(settings.config.state_dir)


def graph_json() -> Path:
    """Return the path to graph.json."""
    settings = _settings()
    override = settings.config.paths.graph_json
    if override is not None:
        return _resolve(override)
    return state_dir() / "graph.json"


def ledger_json() -> Path:
    """Return the path to ledger.json.

    Pinned global: the ledger is cross-project by definition (one row per
    terminal session across every repo), so it must never fork into a
    per-repo stray. An explicit ``config.paths.ledger_json`` override wins
    (tests/sandboxes set it). Otherwise it follows ``config.state_dir`` only
    when that is an absolute anchor - the ``~/.fno`` default and test
    sandboxes both are; a *relative* (project-/CWD-anchored) ``state_dir``
    would land the ledger inside a repo checkout, so it falls back to the
    user-global ``~/.fno`` instead (epic x-f063 Wave 1, x-bb53).
    """
    settings = _settings()
    override = settings.config.paths.ledger_json
    if override is not None:
        return _resolve(override)
    raw = os.path.expanduser(os.path.expandvars(settings.config.state_dir))
    if os.path.isabs(raw):
        return state_dir() / "ledger.json"
    return _resolve("~/.fno/") / "ledger.json"


def bus_dir() -> Path:
    """Return the cross-agent bus directory (default: ~/.fno/bus/).

    Holds the canonical provider-neutral message log (``messages.jsonl``,
    plus rotated ``messages.jsonl.N`` segments) and per-agent read cursors
    under ``cursors/``. Override via ``config.paths.bus_dir`` in settings.yaml.

    Env overrides take precedence so the bus co-isolates with the inbox store
    under tmp dirs and CI/tests never write the real ~/.fno/bus:
      1. ``FNO_BUS_DIR``    - explicit override (tests / operators).
      2. ``FNO_INBOX_ROOT`` - the inbox store's test root; the bus lands at
                              ``<FNO_INBOX_ROOT>/.bus`` so every existing inbox
                              test/smoke that isolates the store also isolates
                              the bus, with no per-test fixture churn.
    """
    explicit = os.environ.get("FNO_BUS_DIR")
    if explicit:
        return Path(explicit)
    inbox_root = os.environ.get("FNO_INBOX_ROOT")
    if inbox_root:
        return Path(inbox_root) / ".bus"
    settings = _settings()
    override = settings.config.paths.bus_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "bus"


def plans_dir(project_root: Optional[Path] = None) -> Path:
    """Return the plans directory.

    Default is project-relative: <project_root>/.fno/plans/.
    When project_root is None, falls back to resolve_repo_root().
    """
    settings = _settings()
    raw = settings.config.plans_dir
    root = project_root or resolve_repo_root()

    # Detect whether the raw value is a "plain relative" path:
    # - does not start with /, ~, $
    # - does not contain { } template variables (no {vault}, {project}, etc.)
    # For such paths, bypass _resolve() entirely and anchor directly to root.
    # _resolve() internally calls Path(...).resolve() which uses CWD, ignoring project_root.
    leading = raw.lstrip()
    has_templates = "{" in raw
    is_plain_relative = leading and not (
        leading.startswith("/")
        or leading.startswith("~")
        or "$" in raw  # env vars anywhere, not just at start
        or has_templates
    )

    if is_plain_relative:
        return (root / raw).resolve()

    # For template-containing or absolute paths, use _resolve (which handles {vault}, {project})
    return _resolve(raw, project_root=root)


def plans_content_dir(project_root: Optional[Path] = None) -> Path:
    """Resolve where plan DOCS actually live (x-ff83).

    Same lookup ``/blueprint`` and interactive ``/think`` use (mirrors the
    ``scripts/lib/config.sh`` resolution order):
      1. ``.claude/settings.local.json`` -> ``plansDirectory`` (per-machine).
      2. ``.claude/settings.json`` -> ``plansDirectory`` (project-level, committed).
      3. ``config.plans_dir`` (settings.yaml) via :func:`plans_dir`.

    Distinct from :func:`plans_dir`, which returns only the settings.yaml
    default and does not read the ``.claude`` override the docs vault uses.
    """
    import json

    root = project_root or resolve_repo_root()
    for name in ("settings.local.json", "settings.json"):
        try:
            data = json.loads((root / ".claude" / name).read_text())
            raw = data.get("plansDirectory")
            if raw:
                p = Path(raw)
                return p if p.is_absolute() else (root / p).resolve()
        except (OSError, ValueError):
            continue  # missing/unreadable -> try the next tier
    return plans_dir(root)


def handoffs_dir(project_root: Optional[Path] = None) -> Path:
    """Return the directory where target writes session handoff artifacts.

    Resolution order:
      1. ``config.paths.handoffs_dir`` explicit override (template-expanded).
      2. ``{vault}/fno/{project}/handoffs/`` when ``obsidian.enabled`` and
         ``obsidian.vault`` are set (survives worktree archive; lives in
         the user's Obsidian vault).
      3. ``state_dir()/handoffs/<project>/`` fallback when no vault
         (survives worktree archive; lives in the global state dir).
    """
    settings = _settings()
    override = settings.config.paths.handoffs_dir
    if override is not None:
        return _resolve(override, project_root=project_root)
    if settings.config.obsidian.enabled and settings.config.obsidian.vault:
        return _resolve("{vault}/fno/{project}/handoffs/", project_root=project_root)
    pid = settings.config.project.id
    if pid:
        project_name = pid
    else:
        root = project_root or resolve_repo_root()
        project_name = root.name
    return state_dir() / "handoffs" / project_name


def briefs_dir() -> Path:
    """Return the briefs directory."""
    settings = _settings()
    override = settings.config.paths.briefs_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "briefs"


def fleet_dir() -> Path:
    """Return the fleet directory."""
    settings = _settings()
    override = settings.config.paths.fleet_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "fleet"


def postmortems_dir() -> Path:
    """Return the postmortems directory."""
    settings = _settings()
    override = settings.config.paths.postmortems_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "postmortems"


def worktrees_base() -> Path:
    """Return the worktrees base directory."""
    settings = _settings()
    override = settings.config.paths.worktrees_base
    if override is not None:
        return _resolve(override)
    return state_dir() / "worktrees"


def memory_dir() -> Path:
    """Return the memory directory."""
    settings = _settings()
    override = settings.config.paths.memory_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "memory"


def retro_pending_dir() -> Path:
    """Return the retro-pending sentinel directory.

    ``fno backlog reconcile`` drops a per-node sentinel here naming a node
    it closed mechanically, so a later session's inbox/triage pass can pick up
    the judgment half (follow-up capture) without reconcile automating it.
    """
    settings = _settings()
    override = settings.config.paths.retro_pending_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "retro-pending"


def hook_logs_dir() -> Path:
    """Return the hook logs directory."""
    settings = _settings()
    override = settings.config.paths.hook_logs_dir
    if override is not None:
        return _resolve(override)
    return state_dir() / "hook-logs"


def agents_registry_path() -> Path:
    """Return the path to the agents registry JSON file."""
    settings = _settings()
    override = settings.config.paths.agents_registry_path
    if override is not None:
        return _resolve(override)
    return state_dir() / "agents" / "registry.json"


def inbox_dir(project_root: Optional[Path] = None) -> Path:
    """Return the inbox directory.

    Default is project-relative: <project_root>/.fno/inbox/.
    """
    settings = _settings()
    override = settings.config.paths.inbox_dir
    if override is not None:
        return _resolve(override, project_root=project_root)
    root = project_root or resolve_repo_root()
    return (root / ".fno" / "inbox").resolve()


_INBOX_PROJECT_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def vault_root() -> Optional[Path]:
    """Filesystem path of the configured Obsidian vault, or None.

    Returns None when ``obsidian.enabled`` is false or ``obsidian.vault`` is
    unset. ``obsidian.vault`` is conventionally a bare vault name (e.g.
    ``myvault``), which maps to ``~/<name>`` - matching the historical inbox
    default that lived under ``~/myvault``. A value that is already absolute or
    ``~``-prefixed is honored as-is.
    """
    settings = _settings()
    obs = settings.config.obsidian
    if not (obs.enabled and obs.vault):
        return None
    expanded = Path(os.path.expanduser(obs.vault))
    return expanded if expanded.is_absolute() else Path.home() / expanded


def inbox_agents_root() -> Path:
    """Base directory holding per-project cross-project inbox folders.

    Each project's inbox lives at ``<base>/<project>/inbox``. The base is:

      1. Vault-derived ``<vault>/internal/agents`` when ``obsidian.enabled``
         and ``obsidian.vault`` are set. For a ``myvault`` vault this resolves
         to ``~/myvault/internal/agents`` - the path live cross-project
         messaging has always used - so this default is behavior-preserving
         for vault-based setups.
      2. A neutral ``state_dir()/inbox/agents`` fallback otherwise, so a
         fresh install with no Obsidian vault never points at a stranger's
         ``~/myvault``.

    ``config.paths.inbox_dir`` still overrides everything; callers that
    resolve a concrete per-project path consult it first.
    """
    root = vault_root()
    if root is not None:
        return root / "internal" / "agents"
    return state_dir() / "inbox" / "agents"


def inbox_root_for(project_name: str) -> Path:
    """Return the inbox directory for a SPECIFIC project (not the current one).

    Unlike inbox_dir(), this explicitly substitutes the `{project}` template
    with the given project_name rather than the sender's config.project.id.
    Used by cross-project messaging: when sender fno writes to
    recipient acme-web, the recipient's inbox path must use
    "acme-web" as the project name regardless of which project the
    sender's git repo lives in.

    Strict validation: project_name must match ``[A-Za-z0-9][A-Za-z0-9._-]*``
    and contain no path separators or ``..`` segments. paths.py is a shared
    utility module - the validation belongs here at the entry point, not
    only at one call site up the stack (e.g. inbox.store.inbox_dir_for).
    This closes the path-traversal class first surfaced in PR #225
    (feedback_project_name_path_traversal).

    Resolution order:
      1. config.paths.inbox_dir from settings.yaml (substitute {project},
         then run the standard {vault}/$VAR/~ resolution).
      2. Otherwise ``inbox_agents_root()/{project_name}/inbox`` - vault-derived
         when Obsidian is enabled (``<vault>/internal/agents/...``), else a
         neutral ``state_dir()/inbox/agents/...`` default.
    """
    if "/" in project_name or "\\" in project_name:
        raise ValueError(f"project name must not contain path separators: {project_name!r}")
    if ".." in project_name:
        raise ValueError(f"project name must not contain '..': {project_name!r}")
    if not _INBOX_PROJECT_NAME_RE.match(project_name):
        raise ValueError(
            f"project name must match {_INBOX_PROJECT_NAME_RE.pattern}: {project_name!r}"
        )
    settings = _settings()
    override = settings.config.paths.inbox_dir
    if override is not None:
        # Substitute {project} explicitly with project_name BEFORE handing
        # off to _resolve, so the standard {project}-from-config-or-cwd
        # behavior in _resolve doesn't pick up the sender's name.
        substituted = override.replace("{project}", project_name)
        # Pass project_root=repo_root so any relative `paths.inbox_dir`
        # (e.g. `.fno/inbox/{project}`) anchors at the git root
        # instead of the caller's cwd. Without this, inbox commands run
        # from subdirectories read/write distinct inbox locations and
        # cross-project messages are lost (Codex review P2 on PR #267).
        return _resolve(substituted, project_root=resolve_repo_root())
    return (inbox_agents_root() / project_name / "inbox").resolve()


def inbox_path(project_root: Optional[Path] = None) -> Path:
    """Return the path to the backlog *capture-tier* inbox markdown file.

    Distinct from inbox_dir() above: that resolver owns the cross-project
    messaging inbox; this one owns the markdown holding-pen of fu-* items
    that sits below idea nodes. Do not conflate them.

    Resolution order:
      1. ``config.paths.inbox_path`` explicit override (template-expanded).
      2. ``config.post_merge.parking_lot_path`` when set - the per-project queue
         the producer (``/fno:pr merged``) writes its narrative to. The
         design unifies the two: the capture-tier inbox and the post-merge prose
         file are ONE file, so honoring this keeps ``fno backlog capture
         add/list/tidy/promote/dismiss`` and the producer pointed at the same
         file in EVERY repo - not the fno-area default, which is the wrong
         file outside this repo (and would make producer-written items invisible
         to the read commands).
      3. With Obsidian enabled: ``<project_root>/internal/fno/backlog/parking-lot.md``
         (canonical default), unless a legacy
         ``internal/fno/backlog/inbox.md`` already exists, in which case that
         file keeps being used (back-compat, so old captures still resolve).
      4. Without a vault, but a legacy ``internal/fno/backlog/inbox.md``
         already exists under the repo: keep using it (back-compat, so an
         upgrade never strands previously captured fu-* items).
      5. Without a vault, but a legacy ``.fno/backlog/inbox.md`` exists: keep it.
      6. Without a vault and no legacy file: ``<project_root>/.fno/backlog/parking-lot.md``
         so a non-vault repo never has a stray ``internal/`` directory materialized.

    The Obsidian default is plain-relative and anchors to the repo root
    (mirrors plans_dir). ``.resolve()`` follows the ``internal/`` symlink to
    the canonical vault target, so sibling worktrees resolve to one shared
    file and a lock on that target coordinates concurrent writers. The
    no-vault default mirrors the optional-vault seam in ``handoffs_dir()``.
    """
    settings = _settings()
    override = settings.config.paths.inbox_path
    post_merge_parking_lot = settings.config.post_merge.parking_lot_path
    root = project_root or resolve_repo_root()
    if override is not None:
        raw = override
    elif post_merge_parking_lot is not None:
        raw = post_merge_parking_lot
    elif settings.config.obsidian.enabled:
        # Canonical default is parking-lot.md; a legacy inbox.md that
        # already exists wins so old captures still resolve (back-compat).
        if (root / "internal/fno/backlog/inbox.md").exists():
            raw = "internal/fno/backlog/inbox.md"
        else:
            raw = "internal/fno/backlog/parking-lot.md"
    elif (root / "internal/fno/backlog/inbox.md").exists():
        # Back-compat: a non-vault repo that already captured deferrals to the
        # old internal/ default keeps using that file, so upgrading never
        # strands previously captured fu-* items (codex review, PR #424).
        raw = "internal/fno/backlog/inbox.md"
    elif (root / ".fno/backlog/inbox.md").exists():
        # Same back-compat for the non-vault .fno default.
        raw = ".fno/backlog/inbox.md"
    else:
        raw = ".fno/backlog/parking-lot.md"

    leading = raw.lstrip()
    has_templates = "{" in raw
    is_plain_relative = leading and not (
        leading.startswith("/")
        or leading.startswith("~")
        or "$" in raw
        or has_templates
    )
    if is_plain_relative:
        return (root / raw).resolve()
    return _resolve(raw, project_root=root)


def config_file() -> Path:
    """Return the path to settings.yaml.

    Prefers the path that load_settings() actually used to load the file
    (Finding 3: loader and paths must agree on the settings file location).
    Falls back to state_dir()/settings.yaml for backward compatibility when
    settings were loaded from built-in defaults (no file on disk).

    Triggers a settings load if not already cached, so _loaded_from is set.
    """
    from fno.config import loaded_from
    # Trigger a load (no-op if cached) so _loaded_from is populated.
    _settings()
    actual = loaded_from()
    if actual is not None:
        return actual
    return state_dir() / "settings.yaml"


# ---------------------------------------------------------------------------
# Plugin-script resolution with a self-healing persisted pointer (#2)
# ---------------------------------------------------------------------------
# `fno target init` / `fno gate set` need scripts that ship with the PLUGIN
# (hooks/, scripts/lib/), not the active project. From a foreign project with
# no env hint those were unreachable (the uv-tool wheel carries no hooks/, and
# CLAUDE_PLUGIN_ROOT is not propagated to `fno` subprocesses), forcing a
# hand-set FNO_REPO_ROOT. resolve_plugin_script() adds a persisted
# ~/.fno/plugin-root pointer, primed by the session-start hook and
# self-healed on any env/pkg resolve, as the env-less fallback.

_PLUGIN_ROOT_POINTER_NAME = "plugin-root"
_PLUGIN_MARKER_RELPATH = "hooks/helpers/init-target-state.sh"


def _plugin_root_pointer() -> Path:
    # ~/.fno (or $FNO_HOME). Computed inline - paths.py has no
    # abilities_home() helper, and reading the env fresh each call (no cache)
    # matches the session-start hook's ${FNO_HOME:-$HOME/.fno}
    # exactly, so the hook-written pointer and this reader always agree.
    home = os.environ.get("FNO_HOME")
    base = Path(home).expanduser() if home else Path.home() / ".fno"
    return base / _PLUGIN_ROOT_POINTER_NAME


def _is_plugin_root(root: Path) -> bool:
    return (root / _PLUGIN_MARKER_RELPATH).is_file()


def _read_persisted_plugin_root() -> "Path | None":
    """Read ~/.fno/plugin-root, returning it only if it still looks like
    the plugin (marker present). A stale pointer (plugin moved/removed) returns
    None so resolution falls through rather than handing back a dead path."""
    try:
        pointer = _plugin_root_pointer()
        if not pointer.is_file():
            return None
        cand = Path(pointer.read_text().strip()).expanduser()
    except OSError:
        return None
    return cand if _is_plugin_root(cand) else None


def _persist_plugin_root(root: Path) -> None:
    """Best-effort cache of *root* to ~/.fno/plugin-root. Only writes a
    root carrying the plugin manifest (.claude-plugin/plugin.json), so an
    env/test fake with just a stub hook can never poison the pointer. Never
    raises - priming is an optimization, not a contract."""
    try:
        if not (root / ".claude-plugin" / "plugin.json").is_file():
            return
        pointer = _plugin_root_pointer()
        new = str(root)
        if pointer.is_file() and pointer.read_text().strip() == new:
            return
        pointer.parent.mkdir(parents=True, exist_ok=True)
        tmp = pointer.with_name(pointer.name + ".tmp")
        tmp.write_text(new + "\n")
        tmp.replace(pointer)
    except OSError:
        pass


def resolve_plugin_script(relpath: str) -> Path:
    """Resolve a script that ships with the fno PLUGIN (not the active
    project), e.g. ``hooks/helpers/init-target-state.sh`` or
    ``scripts/lib/set-gate.sh``.

    Order: env hint (CLAUDE_PLUGIN_ROOT / FNO_REPO_ROOT, authoritative) ->
    package-relative -> persisted ~/.fno/plugin-root pointer -> repo.
    Self-heals the pointer on any env/pkg resolve (manifest-gated)."""
    for env_name in ("CLAUDE_PLUGIN_ROOT", "FNO_REPO_ROOT"):
        root = os.environ.get(env_name)
        if root:
            base = Path(root).expanduser()
            _persist_plugin_root(base)
            return base / relpath
    pkg_root = Path(__file__).resolve().parents[3]
    if _is_plugin_root(pkg_root):
        _persist_plugin_root(pkg_root)
        return pkg_root / relpath
    persisted = _read_persisted_plugin_root()
    if persisted is not None:
        return persisted / relpath
    return resolve_repo_root() / relpath
