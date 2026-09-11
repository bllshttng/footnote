"""The recursive-grep guard: what it refuses and what it must not.

Both halves matter. The guard exists because a `grep -r` over this repo's
nested worktrees ran for 1 hour 56 minutes; a guard that denies too little
does not stop that, and a guard that denies `rg` or a source dir named
`target` is a guard someone disables.
"""
from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "recursive-grep-guard.py"


def _load():
    spec = importlib.util.spec_from_file_location("recursive_grep_guard", HOOK)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


guard = _load()


def _git_repo(path: Path) -> Path:
    subprocess.run(
        ["git", "init", "-q", str(path)], check=True, capture_output=True
    )
    return path


def _make_cache(path: Path) -> Path:
    """A Cargo-shaped cache: identity is the CACHEDIR.TAG marker file."""
    path.mkdir(parents=True, exist_ok=True)
    (path / "CACHEDIR.TAG").write_text(
        "Signature: 8a477f597d28d172789f06886806bc55\n", encoding="utf-8"
    )
    return path


@pytest.fixture(scope="module")
def tagged_repo(tmp_path_factory):
    """A repo shaped like this one: nested worktree caches under
    .claude/worktrees/, a worktree-local crate cache, and untagged `target`
    source directories that must never read as caches."""
    root = _git_repo(tmp_path_factory.mktemp("tagged"))
    _make_cache(
        root / ".claude" / "worktrees" / "wt" / "crates" / "fno-agents" / "target"
    )
    _make_cache(root / "crates" / "fno" / "target")
    (root / "cli" / "src" / "fno" / "target").mkdir(parents=True)
    (root / "skills" / "target").mkdir(parents=True)
    (root / "tests" / "target").mkdir(parents=True)
    return root


@pytest.fixture(scope="module")
def untagged_repo(tmp_path_factory):
    """Source dirs named `target` with no marker: the 2026-09-02 sweep shape
    the guard must never classify from a basename."""
    root = _git_repo(tmp_path_factory.mktemp("untagged"))
    (root / "cli" / "src" / "fno" / "target").mkdir(parents=True)
    (root / "crates" / "target").mkdir(parents=True)
    (root / "tests" / "target").mkdir(parents=True)
    return root


@pytest.fixture(scope="module")
def no_repo(tmp_path_factory):
    """A plain directory with no git repository at all."""
    return tmp_path_factory.mktemp("norepo")


# The refusal cases: every one is a real recursive grep in command position.
DENIED = [
    # The node's own specimens, including the 1h56m one.
    "grep -rn ownership_defect cli/src/fno/ crates/ scripts/ tests/",
    "grep -rln 'without a provider stamp' ./cli/src ./crates",
    "grep -r -l -E _clear_completion_fields crates",
    # Every spelling the contract names.
    "grep -r token crates/",
    "grep -R token .",
    "grep --recursive token .",
    "grep --dereference-recursive token .",
    "/usr/bin/grep -rn token crates/",
    "grep -rln token .",
    "grep -nr token .",
    "grep -Rl token .",
    # An absolute target path changes nothing: the boundary is the repository,
    # not the operand.
    "grep -rn token /tmp",
    # Anywhere in a compound command.
    "cd cli && grep -rn token .",
    "grep -r token crates/ && echo done",
    "true || grep -rn token .",
    "echo hi\ngrep -rn token crates/",
    "x=$(echo 1); grep -rn token .",
    "case $x in y) grep -rn token .;; esac",
    "for f in a b; do grep -rn token .; done",
    "if [ -f x ]; then grep -rn token .; fi",
    # Downstream of a pipe: the last stage has no reader to save it.
    "cat list | grep -r token",
    # Inside a shell payload, under wrappers, behind assignments. A time bound
    # does NOT rescue one: a cache walk is refused outright, `timeout` only
    # bills the same walk for 300 seconds first.
    "bash -c 'grep -rn token .'",
    'sh -c "grep -rn token crates/"',
    "sudo grep -rn token crates/",
    "env grep -rn token crates/",
    "sudo -u me grep -rn token crates/",
    "caffeinate -t 3600 grep -rn token .",
    "FOO=1 grep -rn token .",
    "nohup grep -rn token . &",
    "timeout 300 grep -rn token crates/",
    # A trailing comment survives as tokens on purpose; the -r is still real.
    "grep -rn token .  # scoped to source, should be fine",
    # A heredoc BODY is data, but everything after its terminator is not.
    "cat > f.txt <<'EOF'\nhello\nEOF\ngrep -rn token .",
]

# Benign even where a confirmed cache exists.
ALLOWED = [
    "rg token crates/",
    "rg -n token .",
    "RIPGREP_CONFIG_PATH= rg -uu token crates/",
    "grep -n token crates/",
    "grep token file.txt",
    "grep -c token file.txt",
    # The PATTERN is "-r": a value-taking option consumed it.
    "grep -e -r file.txt",
    "grep -- -r file.txt",
    "grep -f patterns.txt files/",
    # A bundle LED by a value-taking option is that flag plus its attached
    # value, never a flag list: `-fr` reads a pattern FILE named r.
    "grep -fr patterns.txt files/",
    "grep -er token files/",
    "grep -m5r token files/",
    # Prose containing the words is not a search.
    "echo 'use grep -r, not grep'",
    "echo grep -r",
    "git commit -m 'grep -r walks worktrees'",
    "ls -la",
    "cargo build",
    # `xargs grep -r` is a known fail-open: the grep is not in this shell's
    # command position, and the find feeding it names its own scope.
    "find . -name '*.py' | xargs grep -r token",
    # A case-pattern alternation splits at the `|`; neither part resolves to a
    # grep head. Miss is in the accepted fail-open direction.
    "case $x in y|yes) echo pick;; esac",
]


@pytest.mark.parametrize("command", DENIED)
def test_denies_recursive_grep_over_a_confirmed_cache(command: str, tagged_repo) -> None:
    assert guard.decide(command, str(tagged_repo)) is not None, (
        f"should have refused: {command}"
    )


@pytest.mark.parametrize("command", ALLOWED)
def test_allows_benign_commands_over_a_confirmed_cache(command: str, tagged_repo) -> None:
    assert guard.decide(command, str(tagged_repo)) is None, (
        f"should have allowed: {command}"
    )


@pytest.mark.parametrize(
    "command",
    [
        "grep -rn token crates/",
        "grep -R token .",
        "/usr/bin/grep -rn token .",
        "cd cli && grep -rn token .",
    ],
)
def test_a_bare_target_basename_is_not_a_cache(command: str, untagged_repo) -> None:
    """The 2026-09-02 shape: only untagged `target` source dirs, so nothing
    the guard could point at is real. Every recursive grep must pass."""
    assert guard.decide(command, str(untagged_repo)) is None, (
        f"basename classified as a cache: {command}"
    )


@pytest.mark.parametrize(
    "command", ["grep -rn token crates/", "grep -R token ."]
)
def test_no_repository_means_no_refusal(command: str, no_repo) -> None:
    assert guard.decide(command, str(no_repo)) is None


def test_a_symlink_resolving_to_a_tagged_cache_is_refused(tmp_path_factory) -> None:
    root = _git_repo(tmp_path_factory.mktemp("symlink"))
    _make_cache(root / ".cache-store" / "wt-target")
    (root / "target-link").symlink_to(root / ".cache-store" / "wt-target")
    assert guard.decide("grep -rn token .", str(root)) is not None


def test_a_symlink_outside_the_repository_is_not_a_cache(tmp_path_factory) -> None:
    """The contract is a cache BELOW the current repository; a symlink that
    resolves outside it is not one, and refusing it would be inventing a
    cache the walk never saw."""
    root = _git_repo(tmp_path_factory.mktemp("outside"))
    outside = _make_cache(tmp_path_factory.mktemp("external") / "target")
    (root / "target-link").symlink_to(outside)
    assert guard.decide("grep -rn token .", str(root)) is None


def test_the_repo_root_itself_carrying_the_marker_is_refused(tmp_path_factory) -> None:
    root = _git_repo(tmp_path_factory.mktemp("rootcache"))
    _make_cache(root)
    assert guard.decide("grep -rn token .", str(root)) is not None


def test_unbalanced_quotes_fail_open(tagged_repo) -> None:
    assert guard.decide("grep -rn 'unterminated .", str(tagged_repo)) is None


def _run_hook(payload, cwd):
    proc = subprocess.run(
        [sys.executable, str(HOOK)],
        input=json.dumps(payload).encode(),
        capture_output=True,
        cwd=str(cwd),
        timeout=60,
    )
    return proc


def test_end_to_end_deny_envelope_names_the_remedies(tagged_repo) -> None:
    proc = _run_hook(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "grep -rn ownership_defect crates/"},
        },
        tagged_repo,
    )
    assert proc.returncode == 0
    out = json.loads(proc.stdout.decode())
    decision = out["hookSpecificOutput"]
    assert decision["permissionDecision"] == "deny"
    assert "grep -rn ownership_defect crates/" in decision["permissionDecisionReason"]
    assert "Grep tool" in decision["permissionDecisionReason"]
    assert "rg" in decision["permissionDecisionReason"]


def test_end_to_end_allow_is_silent(tagged_repo) -> None:
    proc = _run_hook(
        {"tool_name": "Bash", "tool_input": {"command": "rg -n token crates/"}},
        tagged_repo,
    )
    assert proc.returncode == 0
    assert proc.stdout.decode().strip() == ""


def test_end_to_end_non_bash_tool_is_ignored(tagged_repo) -> None:
    proc = _run_hook(
        {
            "tool_name": "Read",
            "tool_input": {"command": "grep -rn token crates/"},
        },
        tagged_repo,
    )
    assert proc.returncode == 0
    assert proc.stdout.decode().strip() == ""


def test_end_to_end_malformed_stdin_allows(tagged_repo) -> None:
    proc = subprocess.run(
        [sys.executable, str(HOOK)],
        input=b"not json at all",
        capture_output=True,
        cwd=str(tagged_repo),
        timeout=60,
    )
    assert proc.returncode == 0
    assert proc.stdout.decode().strip() == ""
