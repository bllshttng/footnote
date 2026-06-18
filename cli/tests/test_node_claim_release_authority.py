"""Guard: node:<id> claim-release authority is locked to two sanctioned sites.

Regression guard for ab-588326a7: a helper/worker subprocess exit released the
parent target session's ``node:<id>`` claim mid-run, leaving the node
double-dispatchable. That bug was eliminated by x-73cc / PR #534 - the
``dispatch:<id>`` vs ``node:<id>`` holder separation - but nothing PINNED the
invariant, so future drift could reintroduce it (a new helper path that
constructs the ``target-session:<sid>`` holder and releases the node claim, or
an exit handler that unlinks it).

The ONLY source files permitted to release a ``node:<id>`` claim:
  1. ``skills/target/scripts/handoff.sh``      - deliberate self-handoff (holder-verified)
  2. ``crates/fno-agents/src/loop_megawalk.rs`` - the megawalk walker (owner, on success)

A "node-release site" is a ``claim release`` invocation whose released KEY
resolves to a ``node:`` prefix (a literal, or a local variable / ``format!``
bound to ``node:``). This is deliberately NOT mere co-occurrence of the string
``node:`` in the file: ``backlog/advance.py`` carries a ``node:`` liveness probe
AND a ``dispatch:`` reservation release in the same file and must NOT be flagged.
Generic prefix-agnostic wrappers that release a ``key`` parameter (the
``fno claim release`` CLI, ``advance._safe_release``, the pr_watch
``ClaimAdapter``) and releasers of other prefixes
(``session:`` / ``fleet:`` / ``dispatch:`` / ``pending-plan:`` / ``walker:``) are
not node-release sites.

The matcher errs toward FALSE POSITIVES (a new site must be justified in the
allowlist) over false negatives (a silently-missed releaser would give false
confidence - the worst outcome for this specific bug).

Filter: uv run pytest cli/tests/test_node_claim_release_authority.py -q
"""
from __future__ import annotations

import ast
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

# crates/ is in scope because the megawalk walker (the legitimate node-claim
# owner) and any future Rust releaser live there.
SCAN_DIRS = ["cli/src/fno", "scripts", "skills", "hooks", "crates"]

# The two sanctioned node:<id> release sites. Each releases a node claim as a
# legitimate owner/successor with holder verification; see ab-588326a7 / x-73cc.
# A NEW entry here requires an equivalent justification (a holder-verified,
# single-authority release at a sanctioned lifecycle boundary).
ALLOWLIST = {
    "skills/target/scripts/handoff.sh",
    "crates/fno-agents/src/loop_megawalk.rs",
}

_EXTS = {".py", ".sh", ".bash", ".rs"}


def _rel(path: Path) -> str:
    return path.relative_to(REPO_ROOT).as_posix()


def _skip(rel: str) -> bool:
    return (
        "/tests/" in rel
        or "/test/" in rel
        or Path(rel).name.startswith("test_")
        or Path(rel).name.startswith("test-")
        or "/__pycache__/" in rel
        or "/internal/" in rel
        or "/.claude/" in rel
        or rel.endswith((".pyc", ".jsonl"))
    )


def _iter_source_files():
    for d in SCAN_DIRS:
        base = REPO_ROOT / d
        if not base.exists():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix not in _EXTS:
                continue
            rel = _rel(path)
            if _skip(rel):
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            yield rel, text


# --------------------------------------------------------------------------
# shell: a `claim release <KEY>` where KEY is a node: literal, or a variable
# assigned a node: value anywhere in the file.
# --------------------------------------------------------------------------

_SH_RELEASE = re.compile(r"\bclaim\s+release\s+(\S+)")
_SH_VAR = re.compile(r'^["\']?\$\{?([A-Za-z_]\w*)\}?["\']?$')


def _shell_node_release_lines(text: str) -> list[int]:
    hits: list[int] = []
    lines = text.splitlines()
    for n, line in enumerate(lines, start=1):
        if line.lstrip().startswith("#"):
            continue
        m = _SH_RELEASE.search(line)
        if not m:
            continue
        key = m.group(1)
        if key.strip().strip("\"'").startswith("node:"):
            hits.append(n)
            continue
        vm = _SH_VAR.match(key)
        if vm:
            var = vm.group(1)
            # Resolve the variable: its assigned VALUE must START with `node:`
            # (a node key). A plain substring test would mis-flag a value like
            # `dispatch:$id` paired with a `dispatch-node:$$` holder on the same
            # line - "node:" occurs inside "dispatch-node:" but the released key
            # is a dispatch reservation, not a node claim.
            assign = re.compile(
                r"^\s*(?:local\s+|export\s+)?" + re.escape(var) + r"=(\S+)"
            )
            for l2 in lines:
                am = assign.match(l2)
                if am and am.group(1).strip("\"'").startswith("node:"):
                    hits.append(n)
                    break
    return hits


# --------------------------------------------------------------------------
# rust: `["claim", "release", <KEY>, ...]` where KEY is a node: literal or a
# variable bound to a `format!("node:...` / `"node:...` value in the file.
# --------------------------------------------------------------------------

# An `["claim", "release", <KEY>, ...]` argv. Matched over the WHOLE file text
# (not line-by-line) so a multi-line `.args([...])` or `vec![...]` argv is not
# missed (codex P2), and tolerating `.to_string()` / `.into()` adornments on the
# literals (the `claim_release_argv` helper style).
_RS_ADORN = r"(?:\.to_string\(\)|\.into\(\)|\.to_owned\(\))?"
_RS_RELEASE = re.compile(
    r'"claim"' + _RS_ADORN + r'\s*,\s*"release"' + _RS_ADORN + r"\s*,\s*(&?\s*[^,\]\)]+)"
)
_RS_BIND = re.compile(r"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*(?::[^=]+)?=\s*(.+)$")


def _rust_node_release_lines(text: str) -> list[int]:
    lines = text.splitlines()
    node_vars: set[str] = set()
    for line in lines:
        if line.lstrip().startswith("//"):
            continue
        bm = _RS_BIND.search(line)
        if bm and ('format!("node:' in line or '"node:' in bm.group(2)):
            node_vars.add(bm.group(1))
    # Scrub `//` line comments (keep newlines) so a commented-out release in the
    # whole-text scan is not a false positive; line numbers stay accurate.
    scrubbed = re.sub(r"//[^\n]*", "", text)
    hits: list[int] = []
    for m in _RS_RELEASE.finditer(scrubbed):
        key = m.group(1).strip().lstrip("&").strip()
        # `"node:` anywhere in the key catches both a bare literal (`"node:foo"`)
        # and an inlined `&format!("node:{}", ...)` (gemini HIGH); `node_vars`
        # membership catches a `&claim_key` bound to `format!("node:...)`
        # elsewhere. The leading-quote anchor avoids the `dispatch-node:` holder
        # substring trap.
        if '"node:' in key or key in node_vars:
            hits.append(scrubbed.count("\n", 0, m.start()) + 1)
    return sorted(set(hits))


# --------------------------------------------------------------------------
# python (AST): a `release_claim(...)` / `force_release_claim(...)` whose first
# arg (positional, or `key=`) resolves to a node: literal / f-string. A bare
# Name is resolved against assignments in the enclosing function + module scope
# (so a `key` parameter is correctly classified non-node).
# --------------------------------------------------------------------------


def _is_node_const(node: ast.AST) -> bool:
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value.startswith("node:")
    if isinstance(node, ast.JoinedStr):
        for v in node.values:
            if isinstance(v, ast.Constant) and isinstance(v.value, str):
                return v.value.startswith("node:")
            # leading element is an interpolation -> cannot be a node: prefix
            return False
    return False


def _python_node_release_lines(text: str) -> list[int]:
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return []

    parents: dict[ast.AST, ast.AST] = {}
    for node in ast.walk(tree):
        for child in ast.iter_child_nodes(node):
            parents[child] = node

    def enclosing_func(node: ast.AST):
        cur = parents.get(node)
        while cur is not None:
            if isinstance(cur, (ast.FunctionDef, ast.AsyncFunctionDef)):
                return cur
            cur = parents.get(cur)
        return None

    def name_is_node(name: str, func) -> bool:
        scopes: list[ast.AST] = [tree]
        if func is not None:
            scopes.append(func)
        for scope in scopes:
            for node in ast.walk(scope):
                targets: list[ast.AST] = []
                if isinstance(node, ast.Assign):
                    targets = list(node.targets)
                elif isinstance(node, ast.AnnAssign) and node.target is not None:
                    targets = [node.target]
                else:
                    continue
                for t in targets:
                    if isinstance(t, ast.Name) and t.id == name and _is_node_const(node.value):
                        return True
        return False

    hits: list[int] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        fn = node.func
        fname = (
            fn.id if isinstance(fn, ast.Name)
            else fn.attr if isinstance(fn, ast.Attribute)
            else None
        )
        if fname not in ("release_claim", "force_release_claim"):
            continue
        arg = None
        if node.args:
            arg = node.args[0]
        else:
            for kw in node.keywords:
                if kw.arg == "key":
                    arg = kw.value
                    break
        if arg is None:
            continue
        if _is_node_const(arg):
            hits.append(node.lineno)
        elif isinstance(arg, ast.Name) and name_is_node(arg.id, enclosing_func(node)):
            hits.append(node.lineno)
    return hits


def _node_release_lines(rel: str, text: str) -> list[int]:
    if rel.endswith((".sh", ".bash")):
        return _shell_node_release_lines(text)
    if rel.endswith(".rs"):
        return _rust_node_release_lines(text)
    if rel.endswith(".py"):
        return _python_node_release_lines(text)
    return []


def _discover() -> dict[str, list[int]]:
    sites: dict[str, list[int]] = {}
    for rel, text in _iter_source_files():
        lines = _node_release_lines(rel, text)
        if lines:
            sites[rel] = lines
    return sites


# --------------------------------------------------------------------------
# tests
# --------------------------------------------------------------------------


def test_no_unsanctioned_node_claim_release():
    """The dangerous direction: a NEW file releasing a node:<id> claim."""
    sites = _discover()
    unexpected = {r: ls for r, ls in sites.items() if r not in ALLOWLIST}
    detail = "\n".join(
        f"  {r}:{','.join(map(str, ls))}" for r, ls in sorted(unexpected.items())
    )
    assert not unexpected, (
        "Unsanctioned node:<id> claim-release site(s) found. A node claim must "
        "be released ONLY by handoff.sh (deliberate self-handoff) or the "
        "megawalk walker (owner) - never by a helper/worker subprocess "
        "(ab-588326a7). Route the release through a sanctioned site, or add a "
        "justified entry to ALLOWLIST:\n" + detail
    )


def test_both_sanctioned_sites_detected():
    """The matcher must keep SEEING the two known sites. A miss here means the
    matcher silently broke or a sanctioned site moved - either way the guard
    would pass vacuously and must fail loud instead."""
    sites = _discover()
    missing = ALLOWLIST - set(sites)
    assert not missing, (
        "Expected node-release site(s) not detected by the matcher: "
        f"{sorted(missing)}. Either the matcher regressed (it would now miss a "
        "real new releaser too) or the site moved - update the matcher/allowlist."
    )


def test_matcher_classifies_keys_by_prefix():
    """AC1-ERR / AC1-EDGE: in-test fixtures, no tree mutation. The matcher flags
    a node: release and ignores other-prefix / generic-param releases."""
    # shell: node literal flagged; dispatch var ignored
    assert _shell_node_release_lines('fno claim release "node:$ID" --holder x') == [1]
    assert _shell_node_release_lines(
        'RES="dispatch:$ID"\nfno claim release "$RES" --holder x'
    ) == []
    # shell: a node: var IS resolved and flagged
    assert _shell_node_release_lines(
        'K="node:$ID"\nfno claim release "$K" --holder x'
    ) == [2]
    # rust: node format via binding, inlined format!, and bare literal flagged;
    # session literal ignored (gemini HIGH + MEDIUM).
    assert _rust_node_release_lines(
        'let claim_key = format!("node:{}", id);\n'
        '.args(["claim", "release", &claim_key, "--holder", &h])'
    ) == [2]
    assert _rust_node_release_lines(
        '.args(["claim", "release", &format!("node:{}", id), "--holder", &h])'
    ) == [1]
    assert _rust_node_release_lines(
        '.args(["claim", "release", "node:foo", "--holder", &h])'
    ) == [1]
    assert _rust_node_release_lines(
        '.args(["claim", "release", "session:x", "--holder", &h])'
    ) == []
    # rust: a multi-line / vec! argv is still caught (codex P2)
    assert _rust_node_release_lines(
        'let claim_key = format!("node:{}", id);\n'
        "let argv = vec![\n"
        '    "claim".to_string(),\n'
        '    "release".to_string(),\n'
        "    claim_key,\n"
        "];"
    ) != []
    # rust: a commented-out release is not a false positive
    assert _rust_node_release_lines(
        '// .args(["claim", "release", "node:foo", "--holder", &h])'
    ) == []
    # python: node f-string flagged; generic key param + session literal ignored
    assert _python_node_release_lines('release_claim(f"node:{i}", h)') == [1]
    assert _python_node_release_lines(
        "def f(key):\n    release_claim(key, h)"
    ) == []
    assert _python_node_release_lines('release_claim(f"session:{u}", h)') == []


def test_guard_actually_scans_something():
    """Anti-vacuous sweep: a silently-empty walk would let the invariant pass
    for the wrong reason."""
    seen = sum(1 for _ in _iter_source_files())
    assert seen > 200, f"scanner only saw {seen} files; SCAN_DIRS likely wrong"


if __name__ == "__main__":  # debug: print discovered sites
    for r, ls in sorted(_discover().items()):
        print(f"{r}: {ls}")
