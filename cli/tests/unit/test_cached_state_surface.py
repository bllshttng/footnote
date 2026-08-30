from __future__ import annotations

import ast
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SOURCE_ROOT = REPO_ROOT / "cli" / "src" / "fno"
_ROOT_PARAMETER_NAMES = frozenset(
    {"cwd", "path", "registry_path", "repo_root", "root", "root_path", "state_path"}
)

# These caches are intentionally not declared-state readers. Their reasons are
# recorded here so a new decorator cannot be silently skipped by the guard.
_NON_STATE_CACHE_REASONS = {
    ("fno.state_fence", "running_from_source"): "cache key is the module path, not fno state",
    ("fno.claims.hostid", "machine_id"): "cache key is the process machine identity, not fno state",
    ("fno.graph._reconcile", "_gh_executable"): "cache key is tool discovery, not fno state",
    ("fno.agents.mux_spawn", "_codex_cli_version"): "cache key is tool version discovery, not fno state",
}


@dataclass(frozen=True)
class _CachedReader:
    path: Path
    qualified_name: str
    line: int
    node: ast.FunctionDef | ast.AsyncFunctionDef


def _decorator_name(node: ast.expr) -> str | None:
    if isinstance(node, ast.Call):
        node = node.func
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        return node.attr
    return None


def _module_name(path: Path, source_root: Path) -> str:
    relative = path.relative_to(source_root).with_suffix("")
    parts = list(relative.parts)
    if parts[-1] == "__init__":
        parts.pop()
    return ".".join(("fno", *parts))


def _cached_readers(source_root: Path = SOURCE_ROOT) -> list[_CachedReader]:
    readers: list[_CachedReader] = []
    for path in sorted(source_root.rglob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        module = _module_name(path, source_root)
        for node in ast.walk(tree):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            if not any(
                _decorator_name(decorator) in {"cache", "lru_cache"}
                for decorator in node.decorator_list
            ):
                continue
            readers.append(
                _CachedReader(
                    path=path,
                    qualified_name=f"{module}.{node.name}",
                    line=node.lineno,
                    node=node,
                )
            )
    return readers


def _registered_clearers() -> set[str]:
    conftest = REPO_ROOT / "cli" / "tests" / "conftest.py"
    tree = ast.parse(conftest.read_text(encoding="utf-8"), filename=str(conftest))
    for node in ast.walk(tree):
        targets: list[ast.expr] = []
        value: ast.expr | None = None
        if isinstance(node, ast.Assign):
            targets = node.targets
            value = node.value
        elif isinstance(node, ast.AnnAssign):
            targets = [node.target]
            value = node.value
        if not any(
            isinstance(target, ast.Name)
            and target.id == "HERMETIC_CACHED_STATE_CLEARERS"
            for target in targets
        ):
            continue
        entries = ast.literal_eval(value) if value is not None else ()
        return {f"{module}.{function}" for module, function in entries}
    raise AssertionError("conftest has no cached-state clearer registry")


def _root_parameter(reader: _CachedReader) -> str | None:
    arguments = [
        *reader.node.args.posonlyargs,
        *reader.node.args.args,
        *reader.node.args.kwonlyargs,
    ]
    for argument in arguments:
        if argument.arg in _ROOT_PARAMETER_NAMES:
            return argument.arg
    return None


def _violations(source_root: Path = SOURCE_ROOT) -> list[str]:
    registered = _registered_clearers()
    violations: list[str] = []
    for reader in _cached_readers(source_root):
        if reader.qualified_name in registered:
            continue
        module, _, function = reader.qualified_name.rpartition(".")
        if (module, function) in _NON_STATE_CACHE_REASONS:
            continue
        if _root_parameter(reader) is not None:
            continue
        violations.append(
            f"{reader.path}:{reader.line}: {reader.qualified_name} is cached without a "
            "test clear or root-keyed argument; choose one: clear it per test, or "
            "key it on the declared state root and record the reason"
        )
    return violations


def test_cached_state_probe_finds_the_known_surface():
    names = {reader.qualified_name for reader in _cached_readers()}

    assert {
        "fno.config.load_settings",
        "fno.paths._settings",
        "fno.paths.resolve_repo_root",
        "fno.plan.reconcile_status._node_status_map",
        "fno.mail.envelope.fleet_has_crown_at",
    } <= names


def test_every_cached_reader_has_a_clear_or_keying_decision():
    violations = _violations()

    assert not violations, "\n".join(violations)


def test_guard_fails_on_an_unregistered_cached_reader(tmp_path: Path):
    source = tmp_path / "new_reader.py"
    source.write_text(
        "from functools import lru_cache\n\n"
        "@lru_cache(maxsize=1)\n"
        "def new_state_reader():\n"
        "    return 'state'\n",
        encoding="utf-8",
    )

    violations = _violations(tmp_path)

    assert len(violations) == 1
    assert str(source) in violations[0]
    assert ":4:" in violations[0]
    assert "clear it per test" in violations[0]
    assert "key it on the declared state root" in violations[0]


def test_guard_accepts_a_cached_reader_keyed_by_a_root(tmp_path: Path):
    source = tmp_path / "root_reader.py"
    source.write_text(
        "from functools import lru_cache\n\n"
        "@lru_cache(maxsize=1)\n"
        "def root_state_reader(root):\n"
        "    return root\n",
        encoding="utf-8",
    )

    assert _violations(tmp_path) == []
