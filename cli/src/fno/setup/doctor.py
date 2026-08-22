"""fno config doctor - diagnostic command.

Reports each resolved path, flags suspicious values, and recommends fixes.
Read-only; never modifies state.

Exit 0 means clean. Non-zero means at least one suspicious path was detected
or settings could not be loaded.
"""
from __future__ import annotations

from pathlib import Path

# Patterns that indicate misconfigured paths.
# Each entry is (path_prefix, human_reason).
SUSPICIOUS_PATHS: list[tuple[str, str]] = [
    ("/tmp/", "temp directory; data will not survive reboot"),
    ("/var/tmp/", "temp directory; data will not survive reboot"),
    ("/private/tmp/", "temp directory; data will not survive reboot"),
    ("~/Dropbox/", "Dropbox sync; conflicted copies on multi-machine setups"),
    ("~/iCloud/", "iCloud sync; conflicted copies on multi-machine setups"),
    ("~/Library/Mobile Documents/", "iCloud sync; conflicted copies on multi-machine setups"),
    ("~/OneDrive/", "OneDrive sync; conflicted copies on multi-machine setups"),
    (".git/", "git internal; may be cleaned by git gc"),
]

# Accessors to check. All take no arguments (project-relative ones default
# to resolve_repo_root() which is fine for diagnostic purposes).
_ACCESSOR_NAMES = (
    "state_dir",
    "graph_json",
    "ledger_json",
    "briefs_dir",
    "fleet_dir",
    "postmortems_dir",
    "worktrees_base",
    "memory_dir",
    "hook_logs_dir",
)


def check_wip_caps() -> list[str]:
    """Report malformed ``config.kanban.wip_caps`` entries (ab-554d37ef).

    The board renderer (``render_html._load_wip_caps``) silently drops a
    malformed cap so a config typo never crashes a backlog mutation - a
    deliberate "never raise" contract on the render path. The cost is zero
    feedback: a quoted, negative, or mistyped cap just stops working. This
    surfaces those drops as advisory messages at ``fno config doctor`` time,
    reading the same GLOBAL settings file the renderer reads. Returns a
    (possibly empty) list of human-readable reasons.
    """
    try:
        import yaml

        from fno.config import _global_settings_path
    except Exception:
        return []

    path = _global_settings_path()
    if not path.is_file():
        return []
    try:
        with path.open(encoding="utf-8") as f:
            data = yaml.safe_load(f) or {}
    except Exception:
        return []

    # A YAML doc that parses to a non-mapping (list/scalar) would make the
    # data.get(...) below raise AttributeError and crash `doctor`. Degrade to
    # "nothing to check" instead - matching render_html._load_wip_caps, which
    # wraps the same access in a blanket try/except.
    if not isinstance(data, dict):
        return []

    kanban = (data.get("config") or {}).get("kanban")
    if not isinstance(kanban, dict) or "wip_caps" not in kanban:
        return []
    raw = kanban.get("wip_caps")
    if raw is None:
        return []
    if not isinstance(raw, dict):
        return [
            f"config.kanban.wip_caps is {type(raw).__name__}, not a mapping; "
            "all columns left uncapped"
        ]

    problems: list[str] = []
    for k, v in raw.items():
        if not isinstance(k, str):
            problems.append(f"wip_caps key {k!r} is not a string column name; ignored")
            continue
        # bool subclasses int, so check it before the int branch.
        if isinstance(v, bool):
            problems.append(f"wip_caps[{k!r}] = {v!r} is a boolean, not a cap; column left uncapped")
        elif not isinstance(v, int) or v <= 0:
            problems.append(
                f"wip_caps[{k!r}] = {v!r} is not a positive integer; column left uncapped"
            )
    return problems


_VALID_WORKTREE_POLICIES = ("never", "harness-native", "external")
_KNOWN_PROJECT_KEYS = frozenset(
    {"name", "path", "type", "stack", "package_manager", "worktree"}
)


def _edit_distance_le_1(a: str, b: str) -> bool:
    """True if ``a`` and ``b`` differ by at most one insert/delete/substitute."""
    if a == b:
        return True
    la, lb = len(a), len(b)
    if abs(la - lb) > 1:
        return False
    if la == lb:  # one substitution
        return sum(1 for x, y in zip(a, b) if x != y) == 1
    # one insert/delete: the shorter must be a subsequence missing one char
    short, long = (a, b) if la < lb else (b, a)
    i = j = edits = 0
    while i < len(short) and j < len(long):
        if short[i] == long[j]:
            i += 1
        else:
            edits += 1
            if edits > 1:
                return False
        j += 1
    return True


def _worktree_policy_problems_in(data: object) -> list[str]:
    """Out-of-enum policy + typo'd per-project keys in one flat config dict."""
    if not isinstance(data, dict):
        return []
    problems: list[str] = []
    wt = data.get("worktree")
    policy = wt.get("policy") if isinstance(wt, dict) else None
    if policy is not None and policy not in _VALID_WORKTREE_POLICIES:
        problems.append(
            f"config.worktree.policy = {policy!r} is not one of "
            f"{' | '.join(_VALID_WORKTREE_POLICIES)}; worktree creation will refuse"
        )
    work = data.get("work")
    workspaces = work.get("workspaces") if isinstance(work, dict) else None
    if isinstance(workspaces, dict):
        for ws in workspaces.values():
            projects = ws.get("projects") if isinstance(ws, dict) else None
            if not isinstance(projects, list):
                continue
            for entry in projects:
                if not isinstance(entry, dict):
                    continue
                name = entry.get("name") or entry.get("path") or "?"
                for key in entry:
                    if (
                        key not in _KNOWN_PROJECT_KEYS
                        and _edit_distance_le_1(str(key), "worktree")
                    ):
                        problems.append(
                            f"project {name!r} has key {key!r}, likely a typo for "
                            "'worktree'; it is IGNORED, so the project silently gets "
                            "the default policy"
                        )
    return problems


def check_worktree_policy() -> list[str]:
    """Report a bad ``config.worktree.policy`` or a typo'd per-project key (x-168b).

    Two silent footguns: an out-of-enum policy value refuses worktree creation
    (fail-closed is correct, but the operator gets no doctor-time hint), and a
    per-project key mistyped within one edit of ``worktree`` (e.g. ``worktre``)
    is dropped by ``extra="ignore"`` -- the project silently gets the DEFAULT
    policy when it wanted ``never``. Scans BOTH the global config AND the
    invoking repo's ``.fno/config.toml`` (a per-project override, and its typo,
    can live in either), deduping identical messages. Returns human-readable
    reasons.
    """
    try:
        from fno.config import _global_settings_path
        from fno.config_io import _load_raw, _unwrap_config_dict
    except Exception:
        return []

    yaml_path = _global_settings_path()
    paths: list[Path] = [yaml_path.with_name("config.toml"), yaml_path]
    try:
        from fno.paths import resolve_repo_root

        repo_fno = Path(resolve_repo_root()) / ".fno"
        paths[:0] = [repo_fno / "config.toml", repo_fno / "settings.yaml"]
    except Exception:
        pass

    problems: list[str] = []
    seen_files: set[Path] = set()
    for path in paths:
        if not path.is_file():
            continue
        resolved = path.resolve()
        if resolved in seen_files:
            continue
        seen_files.add(resolved)
        parsed, ok = _load_raw(path)
        if not ok:
            problems.append(f"{path} failed to parse; worktree policy cannot be validated")
            continue
        for msg in _worktree_policy_problems_in(_unwrap_config_dict(parsed)):
            if msg not in problems:
                problems.append(msg)
    return problems


def _detected_harness() -> str:
    """Best-effort name of the harness running this shell, for the remedy line.

    Delegates to the canonical tables in :mod:`fno.harness_identity` rather than
    listing markers here. A second copy drifted immediately: the first version of
    this function checked ``CLAUDE_SESSION_ID``, which is the LEGACY marker, and
    never ``CLAUDE_CODE_SESSION_ID``, which is what a live claude session
    actually sets. A real claude session therefore fell through to the ambient
    tier, where a ``CODEX_HOME`` exported in the shell profile - ordinary on a
    machine that runs both - answered "codex" and pointed the remedy at the wrong
    settings file.

    Session-scoped markers are consulted first, then the legacy spellings, then
    ambient vars that merely survive a fork. Only the ambient tier is local: it
    is a remedy-line nicety, not an identity decision, so it does not belong in
    the resolver's own precedence.
    """
    import os

    from fno.harness_identity import (
        HARNESS_SESSION_MARKERS,
        LEGACY_HARNESS_SESSION_MARKERS,
    )

    ambient = (
        ("CLAUDECODE", "claude"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("CODEX_HOME", "codex"),
    )
    for env, name in (
        *HARNESS_SESSION_MARKERS,
        *LEGACY_HARNESS_SESSION_MARKERS,
        *ambient,
    ):
        if os.environ.get(env):
            return name
    return ""


_REMEDY = {
    "claude": (
        "add the state root to permissions.additionalDirectories in your "
        "~/.claude/settings.json"
    ),
    "codex": (
        "add the state root to sandbox_workspace_write.writable_roots in your "
        "~/.codex/config.toml, or run this session on a bypass posture"
    ),
    "agy": "grant the state root write access in your agy settings",
    "opencode": (
        "opencode's --dir SETS the working directory rather than adding one, so "
        "launch this session from a root that contains the state directory"
    ),
}


def check_state_root_writable() -> list[str]:
    """Probe whether THIS session can write the claim store, by writing to it.

    A per-spawn ``--add-dir`` grant (:mod:`fno.agents.writable_dirs`) cannot reach
    a session the operator started by hand, or one that joined by ``/fno-me``: the
    first session on any machine does not come from ``fno agents spawn``. So the
    grant needs an advisory half, and this is it.

    Do NOT infer the answer by parsing each harness's settings file. Doctor runs
    INSIDE the hand-started session, so it IS the sample: create a real file in
    the claim store and remove it. That is a positive marker rather than an
    absence, and an absence has two explanations - unwritable, and the probe
    never ran. It costs one temp file.

    Advises and never writes to any settings file (operator ruling d-926a2b90).
    """
    import os
    import tempfile

    # A diagnostic must not create the state it reports on. FNO_TEST_MODE runs
    # in sandboxes with no writable $HOME, where this probe would fail doctor
    # for a reason unrelated to the user's config.
    if os.environ.get("FNO_TEST_MODE") == "1":
        return []
    try:
        from fno.claims.io import claims_dir, global_claims_root

        store = claims_dir(global_claims_root())
    except Exception as exc:
        return [f"could not resolve the claim store: {exc}"]
    # Probe the STORE, creating it if absent. An earlier version walked up to the
    # nearest existing ancestor to avoid creating state from a diagnostic, and
    # that answered about the wrong directory: a session sandboxed to its cwd but
    # able to write $HOME passed, while the message still named the store. The
    # creation is what a worker does on its first claim anyway, it is idempotent,
    # and a mkdir that fails is itself the answer.
    try:
        store.mkdir(parents=True, exist_ok=True)
        fd, probe_path = tempfile.mkstemp(prefix=".doctor-probe-", dir=str(store))
        os.close(fd)
        os.unlink(probe_path)
    except OSError as exc:
        harness = _detected_harness()
        remedy = _REMEDY.get(harness, "grant this session write access to the state root")
        who = f" (detected harness: {harness})" if harness else ""
        return [
            f"the claim store at {store} is not writable by this session{who}: "
            f"{exc.strerror or exc}. A worker here takes no node claim, so "
            f"`fno agents claim status` reports free while it works and a second worker "
            f"can be dispatched onto the same node. Remedy: {remedy}."
        ]
    return []


def check_agent_profiles(settings: object) -> list[str]:
    """Report stage lanes that cannot launch with their resolved posture."""
    from fno.agents.spawn_defaults import _substrate_compatible

    agents = getattr(settings, "agents", None)
    if agents is None:
        return []
    defaults = getattr(agents, "defaults", None)
    profiles = getattr(agents, "profiles", {}) or {}

    def value(obj: object, key: str) -> str:
        raw = obj.get(key, "") if isinstance(obj, dict) else getattr(obj, key, "")
        return raw.strip() if isinstance(raw, str) else ""

    problems: list[str] = []
    for verb, profile in profiles.items():
        lanes = getattr(profile, "lanes", [])
        targets = (
            [(lane, f"agents.profiles.{verb}.lanes[{index}]") for index, lane in enumerate(lanes)]
            if isinstance(lanes, list) and lanes
            else [(profile, f"agents.profiles.{verb}")]
        )
        for target, path in targets:
            provider = value(target, "provider") or value(profile, "provider") or value(defaults, "provider")
            substrate = value(target, "substrate") or value(profile, "substrate") or value(defaults, "substrate")
            if provider and substrate and not _substrate_compatible(substrate, provider):
                problems.append(
                    f"{path}.substrate = {substrate!r} is incompatible with "
                    f"resolved provider {provider!r}"
                )
    return problems


def run_doctor() -> int:
    """Run the doctor diagnostic. Returns 0 if clean, non-zero on errors or suspicious paths."""
    import os

    from fno import paths
    from fno.config import _candidate_paths, load_settings, loaded_from

    test_mode = os.environ.get("FNO_TEST_MODE") == "1"

    # Determine which settings file was (or would be) loaded.
    # If FNO_CONFIG points to a file that doesn't exist, report it.
    found_path: "Path | None" = None
    for candidate in _candidate_paths():
        if candidate.is_file():
            found_path = candidate
            break

    if found_path is None:
        # No settings.yaml found anywhere in the lookup chain
        env_path = os.environ.get("FNO_CONFIG")
        if env_path:
            missing = Path(env_path)
            print(f"[doctor] error: settings.yaml not found at {missing}")
        else:
            print("[doctor] error: no settings.yaml found")
        print("[doctor] run 'fno config setup migrate-paths' to create settings.yaml")
        return 1

    # Handle load errors gracefully (AC4-FR)
    try:
        s = load_settings()
    except Exception as exc:
        print(f"[doctor] error: could not load settings.yaml: {exc}")
        print(f"[doctor] settings source: {found_path}")
        print("[doctor] run 'fno config setup migrate-paths' to recreate settings.yaml")
        return 1

    # Use loader's authoritative path: load_settings() can fall through to
    # the next candidate when one is malformed, so found_path (first existing
    # file) may not match what was actually parsed.
    settings_path = loaded_from() or found_path

    print(f"[doctor] settings source: {settings_path}")
    print(f"[doctor] schema_version: {s.schema_version}")

    try:
        print(f"[doctor] state_dir: {paths.state_dir()}")
    except Exception as exc:
        print(f"[doctor] state_dir: ERROR ({exc})")

    issues: list[tuple[str, str, str]] = []
    errors: list[tuple[str, str]] = []

    for accessor_name in _ACCESSOR_NAMES:
        accessor = getattr(paths, accessor_name, None)
        if accessor is None:
            continue
        try:
            resolved = accessor()
        except Exception as exc:
            print(f"[doctor]   {accessor_name}: ERROR ({exc})")
            errors.append((accessor_name, str(exc)))
            continue

        resolved_str = str(resolved)
        # Skip /tmp/ suspicious checks in test mode (FNO_TEST_MODE=1) to avoid
        # false positives when pytest's tmp_path is under /tmp/ on Linux runners.
        suspicious = [
            (pat, reason) for pat, reason in SUSPICIOUS_PATHS
            if not (test_mode and pat in ("/tmp/", "/var/tmp/", "/private/tmp/"))
        ]
        for sus_pattern, reason in suspicious:
            try:
                expanded = str(Path(sus_pattern).expanduser().resolve())
            except Exception:
                expanded = sus_pattern.rstrip("/")
            if resolved_str.startswith(expanded) or resolved_str.startswith(sus_pattern):
                issues.append((accessor_name, resolved_str, reason))
                break  # only report the first matching pattern per path

    if issues:
        print(f"\n[doctor] {len(issues)} suspicious path(s) detected:")
        for name, path_str, reason in issues:
            print(f"  - {name} = {path_str}: {reason}")
        print("\nRun 'fno config setup migrate-paths --force' to regenerate paths.")

    cap_problems = check_wip_caps()
    if cap_problems:
        print(f"\n[doctor] {len(cap_problems)} malformed config.kanban.wip_caps entr(ies):")
        for reason in cap_problems:
            print(f"  - {reason}")
        print("\nEach column expects a positive integer (e.g. `now: 20`).")

    wt_problems = check_worktree_policy()
    if wt_problems:
        print(f"\n[doctor] {len(wt_problems)} worktree-policy issue(s):")
        for reason in wt_problems:
            print(f"  - {reason}")
        print("\nValid policy values: never | harness-native | external.")

    profile_problems = check_agent_profiles(s)
    if profile_problems:
        print(f"\n[doctor] {len(profile_problems)} agent-profile issue(s):")
        for reason in profile_problems:
            print(f"  - {reason}")
        print("\nSet a substrate each lane's resolved provider can actually launch.")

    store_problems = check_state_root_writable()
    if store_problems:
        print(f"\n[doctor] {len(store_problems)} state-root write issue(s):")
        for reason in store_problems:
            print(f"  - {reason}")
        print(
            "\nfno prints this line and never edits a harness settings file; the "
            "grant is yours to make."
        )

    if errors or issues or cap_problems or wt_problems or profile_problems or store_problems:
        return 1

    print("\n[doctor] OK; no suspicious paths detected.")
    return 0
