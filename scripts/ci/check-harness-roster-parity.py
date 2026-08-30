#!/usr/bin/env python3
"""Harness-roster parity gate: the shipped roster against its evidence.

``KNOWN_HARNESSES`` in ``cli/src/fno/harness_names.py`` is the COMPLETE roster
of harnesses footnote supports. Three shipped evidence surfaces must union to
exactly that set, and every one-sided difference is NAMED, never counted:

  setup docs     ``docs/SETUP-<name>.md`` filenames: the harnesses the shipped
                 setup docs tell operators they can host the loop family under
  provider arms  the literal match arms of ``for_name()`` in
                 ``crates/fno-agents/src/provider.rs``: the native dispatch cases
  adapter rows   literal ``_register("<name>", Class)`` calls in
                 ``cli/src/fno/adapters/__init__.py``: the Python adapter registry

The gate also pins the ``HERMES_SESSION_ID`` invariant independently of roster
parity: the variable must sit in ``_EXTRA_IDENTITY_NAMES`` (the scrub set) and
be absent from ``HARNESS_SESSION_MARKERS`` and
``LEGACY_HARNESS_SESSION_MARKERS``. Promoting it to a resolver marker would
turn the Hermes in-session spawn refusal into a permitted spawn, so that move
goes red here even when roster parity itself is green. Marker equality is
never inferred from roster equality.

Fail-closed everywhere: a source that is missing, unreadable, unparseable, or
extracts zero names is an error, never an empty set that reads as agreement.

Standard library only, so the guard runs on a bare checkout with no package
install, no Rust build, no network. Exit 0 parity receipt; 1 divergence or a
fail-closed extraction error; 2 misuse. ``--selftest`` runs fixture trees
through the same engine (collect -> check -> report), proving the gate can
fail, not only pass.
"""
from __future__ import annotations

import argparse
import ast
import io
import re
import sys
import tempfile
from contextlib import redirect_stdout
from pathlib import Path
from typing import IO, Any, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]

NAMES_PY = "cli/src/fno/harness_names.py"
PROVIDER_RS = "crates/fno-agents/src/provider.rs"
ADAPTERS_PY = "cli/src/fno/adapters/__init__.py"
IDENTITY_PY = "cli/src/fno/harness_identity.py"
SETUP_GLOB = "SETUP-*.md"

MARKER_VARS = (
    "HARNESS_SESSION_MARKERS",
    "LEGACY_HARNESS_SESSION_MARKERS",
    "_EXTRA_IDENTITY_NAMES",
)


class GateError(Exception):
    """A source could not be read or extracted; the gate fails closed."""


def _read(root: Path, rel: str) -> str:
    path = root / rel
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise GateError(f"{rel}: unreadable ({exc})") from exc


def _string_tuple(tree: ast.Module, var: str, rel: str) -> tuple[str, ...]:
    """First-elements of ``var``'s tuple-of-pairs assignment, literal-only."""
    for node in ast.walk(tree):
        targets: list[ast.expr] = []
        if isinstance(node, ast.Assign):
            targets = list(node.targets)
        elif isinstance(node, (ast.AnnAssign,)) and node.target:
            targets = [node.target]
        if not any(isinstance(t, ast.Name) and t.id == var for t in targets):
            continue
        value = getattr(node, "value", None)
        if not isinstance(value, ast.Tuple):
            raise GateError(f"{rel}: {var} is not a tuple literal")
        names: list[str] = []
        for elt in value.elts:
            if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                names.append(elt.value)  # a bare string tuple (KNOWN_HARNESSES)
            elif (
                isinstance(elt, ast.Tuple)
                and len(elt.elts) == 2
                and isinstance(elt.elts[0], ast.Constant)
                and isinstance(elt.elts[0].value, str)
            ):
                names.append(elt.elts[0].value)  # a pair tuple (marker tables)
            else:
                raise GateError(f"{rel}: {var} holds a non-literal element")
        if not names:
            raise GateError(f"{rel}: {var} is empty")
        return tuple(names)
    raise GateError(f"{rel}: no assignment to {var} found")


def extract_known_harnesses(text: str) -> tuple[str, ...]:
    try:
        tree = ast.parse(text)
    except SyntaxError as exc:
        raise GateError(f"{NAMES_PY}: unparseable ({exc})") from exc
    return _string_tuple(tree, "KNOWN_HARNESSES", NAMES_PY)


def extract_setup_harnesses(root: Path) -> tuple[str, ...]:
    docs = sorted((root / "docs").glob(SETUP_GLOB))
    if not docs:
        raise GateError(f"docs/{SETUP_GLOB}: no setup docs found")
    names = tuple(sorted(p.stem[len("SETUP-") :].lower() for p in docs))
    # A doc named exactly SETUP-.md extracts the empty string, which would
    # print as an invisible entry in the divergence report; refuse it as a
    # malformed source instead of reporting a nameless harness.
    empty = [p.name for p in docs if not p.stem[len("SETUP-") :]]
    if empty:
        raise GateError(f"docs/{SETUP_GLOB}: {', '.join(empty)} carries no harness name")
    return names


def extract_provider_arms(text: str) -> tuple[str, ...]:
    """Literal ``"name" =>`` arms inside ``for_name``'s body, and only there.

    The body is brace-matched from the signature's opening brace, and the
    ``_ => None`` catch-all must be the terminal arm: a quoted arm or an
    ``_ =>`` hit elsewhere in provider.rs cannot satisfy the gate."""
    sig = re.search(r"fn for_name\s*\(", text)
    if sig is None:
        raise GateError(f"{PROVIDER_RS}: fn for_name not found")
    open_at = text.find("{", sig.end())
    if open_at == -1:
        raise GateError(f"{PROVIDER_RS}: for_name has no body")
    depth = 0
    end = -1
    # Scrub string literals first (length-preserving, so scan offsets stay
    # text offsets): a brace inside a quoted arm name would otherwise close
    # the body early and mis-scope the extraction.
    scan = re.sub(
        r'"(?:[^"\\]|\\.)*"',
        lambda m: '"' + " " * (len(m.group(0)) - 2) + '"',
        text[open_at:],
    )
    for i, ch in enumerate(scan):
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                end = i
                break
    if end == -1:
        raise GateError(f"{PROVIDER_RS}: for_name body is unbalanced")
    body = text[open_at : open_at + end + 1]
    catchall = re.search(r"_\s*=>\s*None", body)
    arms = list(re.finditer(r'"([A-Za-z0-9_-]+)"\s*=>', body))
    if catchall is None or not arms:
        raise GateError(f"{PROVIDER_RS}: for_name extracts no dispatch arms")
    if arms[-1].end() > catchall.start():
        raise GateError(f"{PROVIDER_RS}: for_name's _ => None is not the terminal arm")
    return tuple(sorted(m.group(1) for m in arms))


def extract_adapter_names(text: str) -> tuple[str, ...]:
    try:
        tree = ast.parse(text)
    except SyntaxError as exc:
        raise GateError(f"{ADAPTERS_PY}: unparseable ({exc})") from exc
    names: list[str] = []
    for node in ast.walk(tree):
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "_register"
        ):
            if not node.args or not isinstance(node.args[0], ast.Constant):
                raise GateError(f"{ADAPTERS_PY}: a _register call has a non-literal name")
            names.append(str(node.args[0].value))
    if not names:
        raise GateError(f"{ADAPTERS_PY}: no _register calls found")
    return tuple(sorted(set(names)))


def extract_marker_tables(text: str) -> dict[str, tuple[str, ...]]:
    try:
        tree = ast.parse(text)
    except SyntaxError as exc:
        raise GateError(f"{IDENTITY_PY}: unparseable ({exc})") from exc
    return {var: _string_tuple(tree, var, IDENTITY_PY) for var in MARKER_VARS}


def collect(root: Path) -> dict[str, Any]:
    """Read every evidence surface; any fail-closed error propagates."""
    return {
        "canonical": extract_known_harnesses(_read(root, NAMES_PY)),
        "setup docs": extract_setup_harnesses(root),
        "provider dispatch": extract_provider_arms(_read(root, PROVIDER_RS)),
        "adapter registry": extract_adapter_names(_read(root, ADAPTERS_PY)),
        "markers": extract_marker_tables(_read(root, IDENTITY_PY)),
    }


def check(facts: dict[str, Any]) -> list[str]:
    """Every divergence, as printable lines. Empty list is the pass."""
    canonical = set(facts["canonical"])
    surfaces = {
        name: set(facts[name])
        for name in ("setup docs", "provider dispatch", "adapter registry")
    }
    problems: list[str] = []
    for name, values in surfaces.items():
        if not values:
            problems.append(f"{name}: extracted zero names")
    union: set[str] = set()
    for values in surfaces.values():
        union |= values
    missing = union - canonical
    extra = canonical - union
    if missing:
        problems.append(
            "missing_from_known (evidence naming a harness the roster does not "
            f"carry): {', '.join(sorted(missing))}"
        )
    if extra:
        problems.append(
            "known_without_evidence (roster entry no surface names): "
            f"{', '.join(sorted(extra))}"
        )
    # The HERMES_SESSION_ID invariant is checked independently: marker tables
    # are a different contract from roster membership, and a green roster must
    # never launder a promoted marker past this gate.
    markers: dict[str, tuple[str, ...]] = facts["markers"]
    in_markers = [
        var
        for var in ("HARNESS_SESSION_MARKERS", "LEGACY_HARNESS_SESSION_MARKERS")
        if "HERMES_SESSION_ID" in markers[var]
    ]
    if in_markers:
        problems.append(
            "HERMES_SESSION_ID sits in "
            f"{', '.join(in_markers)}; it must stay in _EXTRA_IDENTITY_NAMES "
            "(scrubbed, non-resolving) so the in-session shell-spawn refusal "
            "stays armed"
        )
    if "HERMES_SESSION_ID" not in markers["_EXTRA_IDENTITY_NAMES"]:
        problems.append(
            "HERMES_SESSION_ID is absent from _EXTRA_IDENTITY_NAMES; the scrub "
            "set must keep scrubbing it"
        )
    return problems


def report(facts: dict[str, Any], out: IO[str]) -> None:
    canonical: tuple[str, ...] = facts["canonical"]
    print(f"canonical (KNOWN_HARNESSES, {len(canonical)}): {', '.join(canonical)}", file=out)
    for name in ("setup docs", "provider dispatch", "adapter registry"):
        values: tuple[str, ...] = facts[name]
        print(f"{name}: {', '.join(values)}", file=out)


def run_gate(root: Path, out: IO[str] = sys.stdout) -> int:
    try:
        facts = collect(root)
    except GateError as exc:
        print(f"harness roster parity: FAIL-CLOSED: {exc}", file=out)
        return 1
    print(f"harness roster parity: checking {root}", file=out)
    report(facts, out)
    problems = check(facts)
    for line in problems:
        print(f"DIVERGENT: {line}", file=out)
    if problems:
        print(
            "harness roster parity: RED - fix by aligning KNOWN_HARNESSES with "
            "the named surface (or the surface with the roster) in the same change",
            file=out,
        )
        return 1
    print(
        f"harness roster parity: GREEN - {len(facts['canonical'])} names, every "
        "one evidence-backed, HERMES_SESSION_ID scrubbed and non-resolving",
        file=out,
    )
    return 0


# --- selftest: fixture trees through the REAL engine ------------------------

BASE_ROSTER = (
    "claude",
    "codex",
    "gemini",
    "agy",
    "opencode",
    "pi",
    "hermes",
    "openclaw",
)
BASE_PROVIDERS = ("claude", "codex", "gemini", "agy", "opencode", "pi")
BASE_MARKERS = ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID")
BASE_EXTRA = ("CLAUDECODE_SESSION_ID", "HERMES_SESSION_ID")


def _names_py(roster: tuple[str, ...]) -> str:
    body = "".join(f'    "{name}",\n' for name in roster)
    return f"KNOWN_HARNESSES: tuple[str, ...] = (\n{body})\n"


def _provider_rs(arms: tuple[str, ...]) -> str:
    body = "".join(
        f'        "{name}" => Some(Box::new(Provider{name.capitalize()})),\n'
        for name in arms
    )
    return (
        "pub fn for_name(name: &str) -> Option<Box<dyn Provider>> {\n"
        "    match name {\n"
        f"{body}"
        "        _ => None,\n"
        "    }\n"
        "}\n"
    )


def _adapters_py(names: tuple[str, ...]) -> str:
    return "".join(f'_register("{name}", Thing)\n' for name in names)


def _identity_py(
    markers: tuple[str, ...],
    legacy: tuple[str, ...],
    extra: tuple[str, ...],
) -> str:
    def table(var: str, values: tuple[str, ...]) -> str:
        rows = "".join(f'    ("{v}", "x"),\n' for v in values)
        return f"{var}: tuple[tuple[str, str], ...] = (\n{rows})\n"

    return (
        table("HARNESS_SESSION_MARKERS", markers)
        + table("LEGACY_HARNESS_SESSION_MARKERS", legacy)
        + table("_EXTRA_IDENTITY_NAMES", extra)
    )


def _write_tree(
    root: Path,
    *,
    roster: tuple[str, ...] = BASE_ROSTER,
    providers: tuple[str, ...] = BASE_PROVIDERS,
    adapters: tuple[str, ...] = ("hermes",),
    markers: tuple[str, ...] = BASE_MARKERS,
    legacy: tuple[str, ...] = ("CLAUDE_SESSION_ID",),
    extra: tuple[str, ...] = BASE_EXTRA,
    setup_docs: tuple[str, ...] = ("hermes", "openclaw"),
    adapters_text: Optional[str] = None,
    provider_text: Optional[str] = None,
    skip_adapters: bool = False,
) -> None:
    (root / "docs").mkdir(parents=True, exist_ok=True)
    (root / "cli/src/fno/agents").mkdir(parents=True, exist_ok=True)
    (root / "cli/src/fno/adapters").mkdir(parents=True, exist_ok=True)
    (root / "crates/fno-agents/src").mkdir(parents=True, exist_ok=True)
    (root / NAMES_PY).write_text(_names_py(roster), encoding="utf-8")
    (root / PROVIDER_RS).write_text(
        provider_text if provider_text is not None else _provider_rs(providers),
        encoding="utf-8",
    )
    if not skip_adapters:
        (root / ADAPTERS_PY).write_text(
            adapters_text if adapters_text is not None else _adapters_py(adapters),
            encoding="utf-8",
        )
    (root / IDENTITY_PY).write_text(
        _identity_py(markers, legacy, extra), encoding="utf-8"
    )
    for name in setup_docs:
        # Verbatim, not uppercased: a setup doc named SETUP-.md (empty harness
        # name) is itself a fixture, and uppercasing would hide it from the glob.
        (root / "docs" / f"SETUP-{name}.md").write_text(
            f"# {name} setup\n", encoding="utf-8"
        )


def _case(
    label: str,
    expected_code: int,
    expect_in_output: tuple[str, ...],
    **kwargs: object,
) -> tuple[bool, str]:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / "repo"
        root.mkdir()
        _write_tree(root, **kwargs)  # type: ignore[arg-type]
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = run_gate(root, out=buffer)
        text = buffer.getvalue()
    ok = code == expected_code and all(s in text for s in expect_in_output)
    return ok, f"{label}: exit={code} (want {expected_code})\n{text}"


def run_selftest() -> int:
    cases = [
        _case("exact match passes", 0, ("GREEN", "hermes", "openclaw")),
        _case(
            "setup-only ghost fails naming the docs surface",
            1,
            ("missing_from_known", "ghost", "setup docs"),
            setup_docs=("hermes", "openclaw", "ghost"),
        ),
        _case(
            "roster entry with no evidence fails",
            1,
            ("known_without_evidence", "openclaw"),
            setup_docs=("hermes",),
        ),
        _case(
            "adapter registry with zero rows fails closed",
            1,
            ("FAIL-CLOSED", "no _register calls found"),
            adapters_text="# registry emptied\n_REGISTRY: dict = {}\n",
        ),
        _case(
            "provider.rs without for_name fails closed",
            1,
            ("FAIL-CLOSED", "fn for_name not found"),
            provider_text="pub fn other() {}\n",
        ),
        _case(
            "missing adapters file fails closed",
            1,
            ("FAIL-CLOSED", "unreadable"),
            skip_adapters=True,
        ),
        _case(
            "HERMES_SESSION_ID promoted to a marker fails on green parity",
            1,
            ("HARNESS_SESSION_MARKERS", "spawn refusal"),
            markers=BASE_MARKERS + ("HERMES_SESSION_ID",),
        ),
        _case(
            "HERMES_SESSION_ID dropped from the scrub set fails",
            1,
            ("absent from _EXTRA_IDENTITY_NAMES"),
            extra=("CLAUDECODE_SESSION_ID",),
        ),
        _case(
            "a setup doc with no harness name fails closed",
            1,
            ("FAIL-CLOSED", "carries no harness name"),
            setup_docs=("hermes", "openclaw", ""),
        ),
        _case(
            "a brace inside a quoted arm name does not mis-scope the body",
            0,
            ("GREEN",),
            provider_text=(
                "pub fn for_name(name: &str) -> Option<Box<dyn Provider>> {\n"
                "    match name {\n"
                '        "a{b" => Some(Box::new(BraceProvider)),\n'
                '        "claude" => Some(Box::new(ClaudeProvider)),\n'
                '        "codex" => Some(Box::new(CodexProvider)),\n'
                '        "gemini" => Some(Box::new(GeminiProvider)),\n'
                '        "agy" => Some(Box::new(AgyProvider)),\n'
                '        "opencode" => Some(Box::new(OpencodeProvider)),\n'
                '        "pi" => Some(Box::new(PiProvider)),\n'
                "        _ => None,\n"
                "    }\n"
                "}\n"
            ),
        ),
    ]
    failures = [detail for ok, detail in cases if not ok]
    for detail in failures:
        print(detail, file=sys.stderr)
    print(
        f"harness roster parity selftest: {len(cases) - len(failures)}/{len(cases)} "
        "cases behaved as designed"
    )
    return 1 if failures else 0


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--selftest",
        action="store_true",
        help="run fixture trees through the same engine, proving the gate fails",
    )
    args = parser.parse_args(argv)
    if args.selftest:
        return run_selftest()
    return run_gate(REPO_ROOT)


if __name__ == "__main__":
    raise SystemExit(main())
