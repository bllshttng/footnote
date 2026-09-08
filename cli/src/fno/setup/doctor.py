"""fno config doctor - diagnostic command.

Reports each resolved path, flags suspicious values, and recommends fixes.
Read-only; never modifies state.

Exit 0 means clean. Non-zero means at least one suspicious path was detected
or settings could not be loaded.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Callable

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


def _settings_candidates_for(path: Path) -> list[Path]:
    """The ``config.toml``-first pair at one settings location.

    A check reading only ``settings.yaml`` stopped running at the migration.
    """
    return [path.with_name("config.toml"), path]


def _scan_config_files(paths: list[Path], probe: "Callable[[object], list[str]]") -> list[str]:
    """Run ``probe`` over the MERGED config the runtime resolves from ``paths``.

    ``paths`` is highest precedence first. Probing each layer alone made a
    stale value in an overridden layer a finding nothing reads.
    """
    from fno.config_io import _deep_merge, _load_raw, _unwrap_config_dict

    merged: dict[str, object] = {}
    seen: set[Path] = set()
    for path in reversed(paths):
        if not path.is_file() or path.resolve() in seen:
            continue
        seen.add(path.resolve())
        parsed, ok = _load_raw(path)
        if ok:
            merged = _deep_merge(merged, _unwrap_config_dict(parsed))
    return list(dict.fromkeys(probe(merged)))


def _wip_cap_problems_in(data: object) -> list[str]:
    """Malformed ``kanban.wip_caps`` entries in one FLAT config dict."""
    if not isinstance(data, dict):
        return []
    kanban = data.get("kanban")
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


def check_wip_caps() -> list[str]:
    """Report malformed ``config.kanban.wip_caps`` entries.

    The board renderer drops a malformed cap so a typo never crashes a backlog
    mutation. The cost is zero feedback, which this check pays back at doctor
    time from the same global location the renderer reads.
    """
    try:
        from fno.config import _global_settings_path

        paths = _settings_candidates_for(_global_settings_path())
    except Exception:
        return []
    return _scan_config_files(paths, _wip_cap_problems_in)


_VALID_WORKTREE_POLICIES = ("never", "harness-native", "external")


def _worktree_policy_problems_in(data: object) -> list[str]:
    """An out-of-enum ``worktree.policy`` in one flat config dict."""
    if not isinstance(data, dict):
        return []
    wt = data.get("worktree")
    policy = wt.get("policy") if isinstance(wt, dict) else None
    if policy is None or policy in _VALID_WORKTREE_POLICIES:
        return []
    return [
        f"config.worktree.policy = {policy!r} is not one of "
        f"{' | '.join(_VALID_WORKTREE_POLICIES)}; worktree creation will refuse"
    ]


def check_worktree_policy() -> list[str]:
    """Report an out-of-enum ``config.worktree.policy``.

    The bad value refuses worktree creation, and fail-closed is correct, but
    the operator gets no doctor-time hint. Scans the global config and the
    invoking repo's own, because a per-project override lives in either.
    """
    try:
        from fno.config import _global_settings_path

        paths: list[Path] = _settings_candidates_for(_global_settings_path())
    except Exception:
        return []
    try:
        from fno.paths import resolve_repo_root

        repo_fno = Path(resolve_repo_root()) / ".fno"
        paths[:0] = _settings_candidates_for(repo_fno / "settings.yaml")
    except Exception:
        pass
    # An unreadable file is check_config_files_read's finding, not this one's.
    return _scan_config_files(paths, _worktree_policy_problems_in)


def _detected_harness() -> str:
    """Best-effort name of the harness running this shell, for the remedy line.

    Delegates to the tables in :mod:`fno.harness_identity`; a second copy
    drifted immediately. Only the ambient tier is local, because it is a
    remedy-line nicety and not an identity decision.
    """
    import os

    from fno.harness_identity import (
        HARNESS_SESSION_MARKERS,
        LEGACY_HARNESS_SESSION_MARKERS,
        SELF_SET_HARNESS_MARKERS,
    )

    # CLAUDE_CONFIG_DIR and CODEX_HOME name where config lives, not which
    # binary is running, so they stay out of the shared identity table.
    ambient = (
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("CODEX_HOME", "codex"),
    )
    for env, name in (
        *HARNESS_SESSION_MARKERS,
        *LEGACY_HARNESS_SESSION_MARKERS,
        *SELF_SET_HARNESS_MARKERS,
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


def _check_accounts_in_dict(raw_data: dict[str, Any], source_label: str) -> list[str]:
    """Blocks under accounts/providers that are not tables.

    Unknown KEYS here are `check_unknown_keys`'s job: it derives the same
    report from SettingsModel per file, so the four frozensets that used to
    live here were a hand-copied schema with nothing forcing them to agree.
    A non-table is different: load_providers coerces it to defaults rather
    than refusing, so nothing else says so.

    Record ENTRIES are deliberately not scanned. ProviderRecord is
    extra="allow", so unknown record metadata round-trips by design;
    structural record errors surface through load_providers().
    """
    problems: list[str] = []
    raw_config = raw_data.get("config")
    config = raw_config if isinstance(raw_config, dict) else {}
    for block_key in ("accounts", "providers"):
        for scope, prefix in ((raw_data, block_key), (config, f"config.{block_key}")):
            block = scope.get(block_key)
            if not isinstance(block, dict):
                continue
            for sub in ("quota", "failover"):
                value = block.get(sub)
                if value is not None and not isinstance(value, dict):
                    problems.append(
                        f"{source_label}: {prefix}.{sub} is not a table "
                        f"(got {type(value).__name__}); it will be coerced to defaults"
                    )
    return problems


def check_accounts() -> list[str]:
    """Validate configured accounts / providers and combos."""
    from fno.adapters.providers.loader import (
        _provider_candidates,
        _read_parsed,
        load_combos,
        load_providers,
    )
    from fno.adapters.providers.model import ProviderConfigError

    problems: list[str] = []
    try:
        load_providers()
    except ProviderConfigError as exc:
        problems.append(f"accounts/providers: {exc}")
    except Exception as exc:
        problems.append(f"accounts/providers load error: {exc}")

    try:
        load_combos()
    except ProviderConfigError as exc:
        problems.append(f"combos: {exc}")
    except Exception as exc:
        problems.append(f"combos load error: {exc}")

    # Scan the SAME file set the loads above merge, parsed by the SAME
    # reader (_read_parsed, including its settings.yaml fallback), so a
    # change in reader behavior cannot split the two halves again.
    seen_paths: set[Path] = set()
    for candidate in _provider_candidates():
        if candidate in seen_paths:
            continue
        seen_paths.add(candidate)
        data = _read_parsed(candidate)
        if isinstance(data, dict) and data:
            problems.extend(_check_accounts_in_dict(data, str(candidate)))

    return list(dict.fromkeys(problems))


def run_doctor() -> int:
    """Run the doctor diagnostic. Returns 0 if clean, non-zero on errors or suspicious paths."""
    import os

    from fno import paths
    from fno.config import _candidate_paths, load_settings, loaded_from

    # Imported HERE, never at module level: a static fno.config edge from this
    # module forms a mypy SCC in which graph._constants' lazy __getattr__
    # re-exports degrade to Optional[Path] and fail unrelated modules. The same
    # edge fno.config._revoke_unbacked_optouts keeps out of the import graph.
    from fno.config_readback import (
        check_config_files_read,
        check_enabled_with_empty_population,
        check_unknown_keys,
        contributing_files,
        source_note,
    )

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

    # Contributors, not presences: the old line named the highest-priority file
    # PRESENT, so an unreadable project config was printed as the source of
    # values the global file decided.
    print(f"[doctor] settings source: {', '.join(contributing_files()) or settings_path}")
    print(f"[doctor] schema_version: {s.schema_version}")

    # A key that degraded to its default rather than raising. The degrade keeps
    # one typo from making every fno command exit; this line is what keeps it
    # from being invisible, which reads exactly like a value nobody set.
    from fno.config._sweeps import DEGRADED

    for key, raw in sorted(DEGRADED.items()):
        print(f"[doctor] {key}: bad value {raw} ignored; using the default")

    try:
        print(f"[doctor] state_dir: {paths.state_dir()}")
        print(f"[doctor] space_dir: {paths.space_dir()}  worktree: {paths.worktree_space_dir()}")
    except Exception as exc:
        print(f"[doctor] state_dir/space_dir: ERROR ({exc})")

    issues: list[tuple[str, str, str]] = []
    errors: list[tuple[str, str]] = []
    # FNO_TEST_MODE skips the /tmp/ patterns: pytest's tmp_path is under /tmp/
    # on Linux runners, where they are false positives.
    suspicious = [
        (pat, reason) for pat, reason in SUSPICIOUS_PATHS
        if not (test_mode and pat in ("/tmp/", "/var/tmp/", "/private/tmp/"))
    ]

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
        key = accessor_name if accessor_name == "state_dir" else f"paths.{accessor_name}"
        note = source_note(key) or "default"
        print(f"[doctor]   {accessor_name}: {resolved_str}  (config.{key} {note})")
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

    # One shape, eight checks: heading, the reasons, the remedy line.
    reports: tuple[tuple[str, list[str], str], ...] = (
        (
            "unreadable settings file(s)",
            check_config_files_read(),
            "A file that does not parse contributes NOTHING; every key in it is "
            "silently at its default. config.toml is TOML, settings.yaml is YAML.",
        ),
        (
            "unknown config key(s)",
            check_unknown_keys(),
            "An unknown key is ignored for forward compatibility, so it sets nothing.",
        ),
        (
            "switch(es) enabled with an empty population",
            check_enabled_with_empty_population(),
            "",
        ),
        (
            "malformed config.kanban.wip_caps entr(ies)",
            check_wip_caps(),
            "Each column expects a positive integer (e.g. `now = 20`).",
        ),
        (
            "worktree-policy issue(s)",
            check_worktree_policy(),
            "Valid policy values: never | harness-native | external.",
        ),
        (
            "agent-profile issue(s)",
            check_agent_profiles(s),
            "Set a substrate each lane's resolved provider can actually launch.",
        ),
        (
            "state-root write issue(s)",
            check_state_root_writable(),
            "fno prints this line and never edits a harness settings file; the "
            "grant is yours to make.",
        ),
        (
            "account / provider issue(s)",
            check_accounts(),
            "Fix the accounts or combos configuration in config.toml.",
        ),
    )
    reported = False
    for heading, problems, remedy in reports:
        if not problems:
            continue
        reported = True
        print(f"\n[doctor] {len(problems)} {heading}:")
        for reason in problems:
            print(f"  - {reason}")
        if remedy:
            print(f"\n{remedy}")

    if errors or issues or reported:
        return 1

    print("\n[doctor] OK; no suspicious paths detected.")
    return 0
