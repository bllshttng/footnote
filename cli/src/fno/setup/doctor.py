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
        print("[doctor] run 'fno setup migrate-paths' to create settings.yaml")
        return 1

    # Handle load errors gracefully (AC4-FR)
    try:
        s = load_settings()
    except Exception as exc:
        print(f"[doctor] error: could not load settings.yaml: {exc}")
        print(f"[doctor] settings source: {found_path}")
        print("[doctor] run 'fno setup migrate-paths' to recreate settings.yaml")
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
        print("\nRun 'fno setup migrate-paths --force' to regenerate paths.")

    cap_problems = check_wip_caps()
    if cap_problems:
        print(f"\n[doctor] {len(cap_problems)} malformed config.kanban.wip_caps entr(ies):")
        for reason in cap_problems:
            print(f"  - {reason}")
        print("\nEach column expects a positive integer (e.g. `now: 20`).")

    if errors or issues or cap_problems:
        return 1

    print("\n[doctor] OK; no suspicious paths detected.")
    return 0
