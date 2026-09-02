"""The Rust/Python seam-crossing ratchet (``fno doctor lint seam-crossings``).

Three assertions, one baseline, both-directional failure like state-roots:

1. **crossing** - every Rust line that launches the Python ``fno`` porcelain
   (a literal ``Command::new("fno")``) or calls a baselined porcelain-resolver
   helper (``fno_bin()`` / ``loopcheck_fno_bin()``) is a SITE. A new site
   fails; a baselined site fixed in source without removing its baseline line
   also fails, so a removal can never pay for an addition.
2. **resolver** - every production Rust function whose body resolves the
   porcelain path - through the env seam (``FNO_BIN`` /
   ``FNO_LOOPCHECK_FNO_BIN``) or by constructing it next to an existing
   binary (``join("fno")``) - is a RESOLVER FUNCTION, baselined by its
   definition line. A literal crossing ratchet is defeated by one new helper
   under a new name; this rule fails on the helper's SHAPE, never on its
   name. A pure delegation helper (a body that only calls an existing
   ``fno_bin()`` spelling) carries no env read, so it is caught by rule 1
   instead: its body line IS a new helper-call site.
3. **pydoor** - ``cli/src/fno/rust_binary.py`` is the Python side's single
   door to the Rust runtime. Any other production Python file that puts a
   literal ``fno-agents`` in argv[0] position, or execs one through
   ``subprocess``/``os``/``asyncio``, fails. There are no baselined pydoor
   sites: the door stays single, full stop.

The baseline is ``scripts/ci/seam-crossings-baseline.txt``. Matching is keyed
on (rule, path, line-content) as a MULTISET, never on the line number alone:
a line number moves on every unrelated edit above it, and a ratchet that
churns on unrelated PRs teaches people to regenerate it, which is how a
ratchet stops ratcheting. The line number is still recorded and printed; it
is prose, not the key. Duplicate identical lines in one file are counted, so
deleting one of two identical sites still fails.
"""

from __future__ import annotations

import re
from collections import Counter
from pathlib import Path

BASELINE_RELPATH = "scripts/ci/seam-crossings-baseline.txt"

# The one Python file allowed to exec the Rust runtime binary by literal name.
PY_DOOR_FILE = "cli/src/fno/rust_binary.py"

# Every production Rust source under both crates. Tests live INSIDE these
# files in `#[cfg(test)]` mod blocks (skipped) and in `crates/*/tests/`
# (outside the seam surface by construction).
_RUST_ROOTS = ("crates",)

# A porcelain launch. Covers `Command::new("fno")` and the fully qualified
# `std::process::Command::new("fno")` / `tokio::process::Command::new("fno")`.
_CROSSING_LITERAL_RE = re.compile(r'\bCommand::new\(\s*"fno"\s*\)')

# Calls to baselined resolver helpers. The names come from the BASELINE's
# resolver entries, not a second hand-maintained list, so the two baselined
# sets cannot disagree about who the helpers are. The lookbehind keeps
# `loopcheck_fno_bin(` from matching a hypothetical `my_fno_bin(`.
_HELPER_CALL_TEMPLATE = r"(?<![A-Za-z0-9_]){name}\s*\("

# The porcelain env seam. A function body reading either key is resolving (or
# propagating) the porcelain path, whatever the function is named - that is
# the shape the resolver rule ratchets on. The join form catches an
# env-free, current_exe-relative construction; the closing quote keeps
# `join("fno-agents-daemon")` (a Rust runtime binary, not porcelain) out.
_PORCELAIN_ENV_RE = re.compile(
    r'\b(?:var|var_os)\(\s*"(?:FNO_BIN|FNO_LOOPCHECK_FNO_BIN)"\s*,?\s*\)'
)
_PORCELAIN_JOIN_RE = re.compile(r'join\(\s*"fno"\s*,?\s*\)')

_FN_DEF_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)

# A subprocess/os/asyncio exec call opening, for the direct string-argument
# form (`subprocess.run("fno-agents ...")`, `os.execl("fno-agents", ...)`).
# The argv[0]-position literal below is checked on EVERY line by itself, not
# in a window after a call: an argv assembled in a variable puts the list
# literal on its own line, and a forward window from the call never sees it.
_PY_DOOR_CALL_RE = re.compile(
    r"\b(?:subprocess|os)\.(?:Popen|run|call|check_output|check_call|"
    r"getoutput|getstatusoutput|exec[lv]p?e?|spawn[lv]p?e?|posix_spawn)\b"
    r"|\basyncio\.create_subprocess_(?:exec|shell)\b"
)
# A list literal whose first element is the binary. BRACKET only: an open
# PAREN before the literal is a call argument (`shutil.which("fno-agents")`,
# a locator, not an exec) or a name tuple, and treating it as argv0 ratchets
# the tree's locators as execs.
_PY_DOOR_ARGV0_RE = re.compile(r"""\[\s*["']fno-agents["']""")
# The direct string-argument forms accept a whole command string, so the
# literal is followed by a space, a quote, or end of line - never required to
# be the entire argument.
_PY_DOOR_DIRECT_RE = re.compile(r"""\(\s*["']fno-agents(?:["'\s]|$)""")
_PY_DOOR_BARE_DIRECT_RE = re.compile(
    r"""\b(?:Popen|run|call|check_output|check_call|exec[lv]p?e?|spawn[lv]p?e?)\(\s*["']fno-agents(?:["'\s]|$)"""
)

SITE_RULE = "crossing"
RESOLVER_RULE = "resolver"
PYDOOR_RULE = "pydoor"
_RULES = (SITE_RULE, RESOLVER_RULE, PYDOOR_RULE)


def _is_comment(line: str) -> bool:
    stripped = line.lstrip()
    return stripped.startswith(("//", "#", "*", "/*"))


def _rust_scan_files(repo_root: Path) -> list[Path]:
    out: list[Path] = []
    root = repo_root / "crates"
    if not root.is_dir():
        return out
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(repo_root).as_posix()
        parts = rel.split("/")
        if "tests" in parts or "target" in parts:
            continue
        out.append(path)
    return out


def _py_scan_files(repo_root: Path) -> list[Path]:
    root = repo_root / "cli" / "src" / "fno"
    if not root.is_dir():
        return []
    out: list[Path] = []
    for path in sorted(root.rglob("*.py")):
        rel = path.relative_to(repo_root).as_posix()
        if rel == PY_DOOR_FILE:
            continue
        parts = rel.split("/")
        if "tests" in parts or path.name.startswith("test_"):
            continue
        out.append(path)
    return out


def _lines(path: Path) -> list[str]:
    try:
        return path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return []


def _find_resolvers(
    repo_root: Path,
) -> list[tuple[str, int, str, str]]:
    """Resolver functions as ``(rel, line_no, def_line, fn_name)``.

    A function is a resolver when a line inside it reads a porcelain env key
    or constructs the porcelain path with ``join("fno")``. Each match line is
    attributed to the fn definition NEAREST ABOVE it - no brace counting.
    A body span needs a Rust parser to get right: brace characters live in
    char literals (``starts_with('{')``), format strings, and comment prose,
    and a span that never closes swallows the NEXT function's env read and
    baselines a fn that resolves nothing. The nearest-preceding attribution
    cannot swallow: a nested closure still attributes to its owning fn, and
    a read in a later fn attributes to that later fn. The fn NAME is for the
    failure message only; detection is by shape, so a renamed or brand-new
    helper is caught all the same.
    """
    hits: list[tuple[str, int, str, str]] = []
    for path in _rust_scan_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        lines = _lines(path)
        if not lines:
            continue
        from fno.lint_cli import _rust_cfg_test_lines

        skip = _rust_cfg_test_lines(lines)
        fns: list[tuple[int, str, str]] = []  # (idx, def_line, name)
        attributed: dict[int, tuple[str, str]] = {}
        for i, line in enumerate(lines):
            if i + 1 in skip or _is_comment(line):
                continue
            match = _FN_DEF_RE.match(line)
            if match is not None:
                fns.append((i, line.strip(), match.group(1)))
                continue
            # A three-line FORWARD window, because rustfmt wraps a long
            # argument call across three lines (`var(\n    "FNO_BIN",\n)`);
            # the match still anchors at line i, so attribution to the fn
            # above is unchanged. The paren guard keeps a fn's bare closing
            # brace from anchoring a window that reaches the next function's
            # first wrapped call.
            if "(" not in line:
                continue
            window = "\n".join(lines[i : i + 3])
            if (
                _PORCELAIN_ENV_RE.search(window) is None
                and _PORCELAIN_JOIN_RE.search(window) is None
            ):
                continue
            if fns:
                owner_idx, def_line, name = fns[-1]
                attributed[owner_idx] = (def_line, name)
        for owner_idx, (def_line, name) in sorted(attributed.items()):
            hits.append((rel, owner_idx + 1, def_line, name))
    return hits


def _helper_names(resolvers: list[tuple[str, int, str, str]]) -> list[str]:
    """Baselined helper names, derived from the resolver definitions.

    Shortest name first so a compound name (`loopcheck_fno_bin`) never hides
    a plain one (`fno_bin`) during call matching - not that it could with the
    lookbehind, but the sort makes the intent explicit.
    """
    names = {fn_name for _r, _l, _d, fn_name in resolvers if fn_name.endswith("fno_bin")}
    return sorted(names, key=len)


def _find_crossings(
    repo_root: Path, helper_res: list[re.Pattern[str]]
) -> list[tuple[str, int, str]]:
    """Crossing sites as ``(rel, line_no, content)``."""
    hits: list[tuple[str, int, str]] = []
    for path in _rust_scan_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        lines = _lines(path)
        if not lines:
            continue
        from fno.lint_cli import _rust_cfg_test_lines

        skip = _rust_cfg_test_lines(lines)
        for i, line in enumerate(lines):
            if i + 1 in skip or _is_comment(line):
                continue
            if _CROSSING_LITERAL_RE.search(line) is not None:
                hits.append((rel, i + 1, line.strip()))
            elif any(res.search(line) is not None for res in helper_res):
                hits.append((rel, i + 1, line.strip()))
    return hits


def _find_pydoor(repo_root: Path) -> list[tuple[str, int, str]]:
    """Literal ``fno-agents`` argv[0] execs outside the single door.

    Two independent shapes. The argv[0]-position literal is a violation on
    ANY line: a list assembled into a variable crosses through that line,
    whether or not the exec call sits nearby. The direct form needs the exec
    call and a bare string argument on the same line.
    """
    hits: list[tuple[str, int, str]] = []
    for path in _py_scan_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        lines = _lines(path)
        for i, line in enumerate(lines):
            if _is_comment(line):
                continue
            if (
                _PY_DOOR_ARGV0_RE.search(line) is not None
                or _PY_DOOR_BARE_DIRECT_RE.search(line) is not None
                or (
                    _PY_DOOR_CALL_RE.search(line) is not None
                    and _PY_DOOR_DIRECT_RE.search(line) is not None
                )
            ):
                hits.append((rel, i + 1, line.strip()))
    return hits


def _read_baseline(baseline_path: Path) -> Counter[tuple[str, str, str]]:
    """Parse the ratchet into a multiset of ``(rule, path, content)``."""
    entries: Counter[tuple[str, str, str]] = Counter()
    if not baseline_path.is_file():
        return entries
    for line in baseline_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        # rule<TAB>path<TAB>line<TAB>content. maxsplit=3 keeps any tab inside
        # the content column part of the key, and the empty-column guard
        # stops a blank field from shifting content into the key silently.
        fields = [f.strip() for f in stripped.split("\t", 3)]
        if len(fields) < 4 or not all(fields[:3]):
            continue
        rule, rel, _line, content = fields
        entries[(rule, rel, content)] += 1
    return entries


def enumerate_sites(repo_root: Path) -> dict[str, list[tuple[str, int, str]]]:
    """All three rule surfaces, uncached, as ``{rule: [(rel, line, text)]}``."""
    resolvers = _find_resolvers(repo_root)
    helper_res = [
        re.compile(_HELPER_CALL_TEMPLATE.format(name=name))
        for name in _helper_names(resolvers)
    ]
    return {
        SITE_RULE: _find_crossings(repo_root, helper_res),
        RESOLVER_RULE: [(rel, line, text) for rel, line, text, _ in resolvers],
        PYDOOR_RULE: _find_pydoor(repo_root),
    }


def build_baseline(repo_root: Path) -> str:
    """The full baseline file: header plus sorted data lines."""
    sites = enumerate_sites(repo_root)
    out: list[str] = []
    for rule in _RULES:
        for rel, line, text in sorted(sites[rule]):
            out.append(f"{rule}\t{rel}\t{line}\t{text}")
    return _baseline_header() + "\n".join(out) + ("\n" if out else "")


def _verify(
    repo_root: Path, baseline: Counter[tuple[str, str, str]]
) -> tuple[list[str], list[str], dict[str, tuple[int, int]]]:
    """Compare live sites against the baseline multiset.

    Returns ``(new_site_messages, stale_site_messages, per_rule_counts)``
    where counts map rule -> ``(measured, baselined)``.
    """
    sites = enumerate_sites(repo_root)
    live: Counter[tuple[str, str, str]] = Counter()
    for rule, rows in sites.items():
        for rel, _line, text in rows:
            live[(rule, rel, text)] += 1

    new_msgs: list[str] = []
    for (rule, rel, text), count in sorted(live.items()):
        extra = count - baseline.get((rule, rel, text), 0)
        if extra <= 0:
            continue
        if rule == PYDOOR_RULE:
            new_msgs.append(
                f"{rule}: {rel}: {text}\n"
                f"    {PY_DOOR_FILE} is the Python side's ONE door to the Rust"
                " runtime.\n"
                "    Route this exec through it. The pydoor rule takes no"
                " baseline."
            )
        else:
            new_msgs.append(
                f"{rule}: {rel}: unbaselined site: {text}\n"
                f"    Add it to {BASELINE_RELPATH} only with the node that owns"
                " draining it,\n    or fix the site in this change."
            )

    stale_msgs: list[str] = []
    for (rule, rel, text), count in sorted(baseline.items()):
        extra = count - live.get((rule, rel, text), 0)
        if extra <= 0:
            continue
        stale_msgs.append(
            f"{rule}: {rel}: baselined but no longer matches: {text}\n"
            f"    Delete this line from {BASELINE_RELPATH}. A drained"
            " exemption left in\n    the baseline is a permanent one."
        )

    counts = {
        rule: (
            len(sites[rule]),
            sum(n for (r, _p, _t), n in baseline.items() if r == rule),
        )
        for rule in _RULES
    }
    return new_msgs, stale_msgs, counts


def baseline_path(repo_root: Path) -> Path:
    return repo_root / BASELINE_RELPATH


def run(repo_root: Path, update: bool = False) -> int:
    """Run (or regenerate) the ratchet. Returns the process exit code."""
    import typer

    if update:
        target = baseline_path(repo_root)
        target.parent.mkdir(parents=True, exist_ok=True)
        rendered = build_baseline(repo_root)
        target.write_text(rendered, encoding="utf-8")
        rows = sum(
            1
            for line in rendered.splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        )
        typer.echo(
            f"seam-crossings: regenerated {BASELINE_RELPATH} ({rows} site(s))"
        )
        return 0

    baseline = _read_baseline(baseline_path(repo_root))
    new_msgs, stale_msgs, counts = _verify(repo_root, baseline)
    for msg in new_msgs + stale_msgs:
        typer.echo(msg, err=True)
    if new_msgs or stale_msgs:
        typer.echo(
            "seam-crossings: FAIL "
            + " ".join(
                f"{rule} {measured}/{baselined}"
                for rule, (measured, baselined) in counts.items()
            ),
            err=True,
        )
        return 1
    typer.echo(
        "seam-crossings: ok ("
        + ", ".join(
            f"{rule} {measured} measured / {baselined} baselined"
            for rule, (measured, baselined) in counts.items()
        )
        + ")"
    )
    return 0


def _baseline_header() -> str:
    return (
        "# Rust/Python seam ratchet, regenerated by\n"
        "# `fno doctor lint seam-crossings --update`.\n"
        "# Format: rule<TAB>path<TAB>line<TAB>content. Matching keys on"
        " (rule, path, content)\n"
        "# as a multiset; the line column is prose. pydoor takes NO baseline:"
        f" {PY_DOOR_FILE}\n# stays the only Python file that execs fno-agents"
        " by literal name.\n"
    )
