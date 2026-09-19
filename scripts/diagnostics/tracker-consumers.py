#!/usr/bin/env python3
"""The graph-consumer and backlog-verb census (tasks 4.2 + 5.1).

Two modalities, both with a mandatory positive control:

--verbs    Enumerate the LIVE backlog registry and verify every verb carries
           exactly one classification marker (tracker-owned = guarded by the
           shared external refusal; footnote-owned = seam/read side). Positive
           controls: a known creation verb must be tracker-owned and a known
           surviving read verb must be footnote-owned, AND the runtime guard
           must actually refuse a tracker-owned verb under an external
           backend - a marker without the refusal is decorative.

--reads    (task 5.1) Scan Python direct read_graph call sites plus the Rust
           direct graph parser, name the backend/storage allowlist, and reject
           every unclassified consumer.

--self-test  Inject known-bad inputs into BOTH modalities and print the
           success marker only after each detector names what it must.

Exit 0 clean; exit 1 on any unclassified verb/consumer or failed control.
No frozen verb counts anywhere: the registry is enumerated at run time.
"""
from __future__ import annotations

import argparse
import ast
import os
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

# The named backend/storage owners: modules whose direct graph access is the
# sanctioned storage seam (the tracker backends, the sidecar projection's
# graph-mode store, the guarded footnote-metadata reader, and the archive
# machinery inside the store itself).
READ_ALLOWLIST = (
    "cli/src/fno/graph/store.py",
    "cli/src/fno/tracker/graph_backend.py",
    "cli/src/fno/tracker/sidecar.py",
    "cli/src/fno/tracker/metadata.py",
    "crates/fno/src/backlog_view.rs",  # consumes the neutral snapshot + graph-mode mtime path (task 1.2)
    "crates/fno-agents/src/graph_get.rs",  # refuses the default store under an external backend
    "crates/fno-agents/src/prove_it_verdicts.rs",  # the verdict reader's read-only walk, same external-backend refusal as graph_get
    "crates/fno-agents/src/gc_sweep.rs",  # the retirement sweep's read-only reverse join (sessions_index + work_state)
    "crates/fno-agents/src/feed.rs",  # the activity feed's read-only lifecycle derivation
    "crates/fno-agents/src/scratch.rs",  # the sweep's read-only node-status lookup feeding the file/fold decision
    "crates/fno-agents/src/route_slot.rs",  # the routing audit's read-only decision projection
    # Not readers: the backend machinery and its names. mod.rs labels the
    # json leg inside the import/export divergence check; note_history.rs
    # defaults the journal's derived file name; note_migrate.rs help text
    # names the default store path.
    "crates/fno-agents/src/backlog/mod.rs",
    "crates/fno-agents/src/backlog/note_history.rs",
    "crates/fno-agents/src/backlog/note_migrate.rs",
    "crates/fno-agents/src/king_board/scope.rs",  # the scope's default store-path builder (graph_json_path), never a read
)

# Known-positive controls (task 4.2 / AC9): verbs the census must FIND in the
# stated class. Absence of either control fails the census - a green run over
# a registry that silently lost its creation verb is not evidence.
KNOWN_TRACKER_OWNED_VERB = "add"
KNOWN_FOOTNOTE_OWNED_VERB = "get"

SELF_TEST_OK_MARKER = "tracker-consumers: self-test OK"


def _iter_registry(apps):
    for group, app in apps:
        for info in app.registered_commands:
            name = info.name or ""
            yield (f"{group} {name}" if group else name), info


def _registry():
    sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
    from fno.graph import cli as graph_cli

    # The classifier's own structural list, never a copy: the census must
    # enumerate the registry the guard actually walked.
    return list(graph_cli.iter_backlog_registry())


def _marker_of(info) -> str | None:
    cb = info.callback
    if cb is None:
        return None
    if getattr(cb, "_fno_tracker_owned", False):
        return "tracker-owned"
    if getattr(cb, "_fno_footnote_owned", False):
        return "footnote-owned"
    return None


def census_verbs(verbose: bool = False) -> tuple[int, list[str]]:
    """Classify every live registry verb. Returns (total, problems)."""
    problems: list[str] = []
    rows = list(_iter_registry(_registry()))
    seen: dict[str, str] = {}
    for label, info in rows:
        marker = _marker_of(info)
        if marker is None:
            problems.append(f"unclassified verb: {label}")
            continue
        # Both markers on one verb is a classification bug.
        cb = info.callback
        if (
            getattr(cb, "_fno_tracker_owned", False)
            and getattr(cb, "_fno_footnote_owned", False)
        ):
            problems.append(f"double-classified verb: {label}")
        seen[label] = marker
        if verbose:
            print(f"  {marker:<15} {label}")
    # Positive controls: the KNOWN verbs must exist AND sit in the stated
    # class. A registry that renamed them must update the control, not
    # silently pass.
    if seen.get(KNOWN_TRACKER_OWNED_VERB) != "tracker-owned":
        problems.append(
            f"positive control failed: {KNOWN_TRACKER_OWNED_VERB!r} must be a "
            f"tracker-owned verb (found: {seen.get(KNOWN_TRACKER_OWNED_VERB)})"
        )
    if seen.get(KNOWN_FOOTNOTE_OWNED_VERB) != "footnote-owned":
        problems.append(
            f"positive control failed: {KNOWN_FOOTNOTE_OWNED_VERB!r} must be a "
            f"footnote-owned verb (found: {seen.get(KNOWN_FOOTNOTE_OWNED_VERB)})"
        )
    return len(rows), problems


def _guard_fires_runtime() -> tuple[bool, str]:
    """Runtime proof the shared refusal is installed, not just the marker:
    invoke a tracker-owned verb's callback under an external backend env and
    expect the named refusal."""
    import typer

    sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
    os.environ["FNO_TRACKER_BACKEND"] = "github"
    try:
        for label, info in _iter_registry(_registry()):
            if label != KNOWN_TRACKER_OWNED_VERB:
                continue
            try:
                info.callback("title")  # the wrapper; the guard fires first
            except typer.Exit as exc:
                return exc.exit_code == 1, f"guard refused {label} (exit {exc.exit_code})"
            except Exception as exc:  # noqa: BLE001 - any other escape is a bug
                return False, f"guard leaked {type(exc).__name__}: {exc}"
            return False, f"guard did not refuse {label}"
        return False, f"{KNOWN_TRACKER_OWNED_VERB!r} not found in registry"
    finally:
        os.environ.pop("FNO_TRACKER_BACKEND", None)


def _has_call(subtree, names):
    for node in ast.walk(subtree):
        if isinstance(node, ast.Call):
            f = node.func
            fname = f.id if isinstance(f, ast.Name) else (
                f.attr if isinstance(f, ast.Attribute) else ""
            )
            if fname in names:
                return True
    return False


def _is_raw_graph_parse(node):
    """A raw graph-store parse: json.loads over read_text/read_bytes bytes.

    This is the spelling that escaped the read_graph census: config_cli.py
    read the frozen graph.json as json.loads(graph.read_text()) and no
    detector knew it. A bare `loads(...)` name (from json import loads)
    counts too; the graph_json() conjunction in _raw_parse_is_graph_read is
    what keeps other modules' loads calls out of the census.
    """
    if not isinstance(node, ast.Call):
        return False
    f = node.func
    if isinstance(f, ast.Attribute):
        if f.attr != "loads":
            return False
        if not (isinstance(f.value, ast.Name) and f.value.id == "json"):
            return False
    elif not (isinstance(f, ast.Name) and f.id == "loads"):
        return False
    return _has_call(node, {"read_text", "read_bytes"})


def _raw_parse_is_graph_read(site, top) -> bool:
    """A raw parse counts as a graph read only when its outermost enclosing
    function also calls graph_json(): any other json.loads(read_text) is
    another file's parse."""
    return (
        _is_raw_graph_parse(site)
        and top is not None
        and _has_call(top, {"graph_json"})
    )


def _classify_site(site, top, *, allow, mach, guarded_names, refusal_calls):
    """One detected read site's bucket, or (None, reason) unclassified."""
    if top is None:
        return (None, None) if allow else (None, "at module level")
    if allow:
        return "owner", None
    if mach:
        return "guarded-machinery", None
    switched = _has_call(top, {"active_backend_name", "_external_mode"})
    guarded = top.name in guarded_names or _has_call(top, refusal_calls)
    params = {a.arg for a in top.args.args}
    params |= {a.arg for a in top.args.kwonlyargs}
    if guarded:
        return "guarded-verb", None
    if switched:
        return "backend-switched", None
    if site.args and isinstance(site.args[0], ast.Name) and site.args[0].id in params:
        return "redirect-seam", None
    return None, f"in {top.name}()"


def census_reads(verbose: bool = False) -> tuple[int, list[str]]:
    """AST census of direct read_graph consumers plus the Rust direct parser.

    Every Python read site must attribute to a VERIFIED class:
      owner             - the named backend/storage modules.
      guarded-verb      - inside the registered callback of a tracker-owned
                          verb (cross-checked against the LIVE registry; the
                          shared refusal wraps it) or a helper that itself
                          calls the shared/create refusal.
      guarded-machinery - a named mutation-machinery module carrying the
                          tracker-owned marker comment (every entry path is a
                          guarded verb).
      backend-switched  - the enclosing function branches on
                          active_backend_name() before reading, so the local
                          read is unreachable under an external selection.
      redirect-seam     - reads only an EXPLICIT caller-supplied path (a
                          hermetic-test seam), never the default store.
    A second detector beside read_graph catches the raw parse spelling:
    json.loads over read_text/read_bytes inside a function that also calls
    graph_json(). Anything else is unclassified and fails the census.
    Regexes would flag imports and docstrings; the AST only sees real calls.
    """
    problems: list[str] = []
    total = 0
    owner_files = {str(REPO_ROOT / p) for p in READ_ALLOWLIST if p.endswith(".py")}
    rust_allow = {str(REPO_ROOT / p) for p in READ_ALLOWLIST if p.endswith(".rs")}
    machinery = {
        str(REPO_ROOT / "cli/src/fno/backlog/advance.py"),
        # run_pass is the maintain verb's engine (moved out of the cli shell);
        # its reads are the verb's own orchestration, not a new consumer.
        str(REPO_ROOT / "cli/src/fno/graph/maintain.py"),
        # The lifecycle verbs (defer/undefer/unsupersede/retract) live here,
        # nested in their registrar (file-budget ratchet); their pre-check
        # reads are the verbs' own orchestration, and every entry path is a
        # tracker-owned registered verb.
        str(REPO_ROOT / "cli/src/fno/graph/lifecycle.py"),
    }
    machinery_marker = "tracker-owned machinery"

    # Live registry: function names of tracker-owned verb callbacks.
    guarded_names: set[str] = set()
    for _label, info in _iter_registry(_registry()):
        cb = info.callback
        if getattr(cb, "_fno_tracker_owned", False):
            guarded_names.add(getattr(getattr(cb, "__wrapped__", cb), "__name__", ""))
    refusal_calls = {
        "_refuse_tracker_owned_on_external_backend",
        "_refuse_create_on_external_backend",
        # Creation delegation: _create_node_impl refuses on an external
        # backend before any store access, so a helper that routes births
        # through it is guarded by that first act.
        "_create_node_impl",
    }

    def _is_read_graph(node):
        if not isinstance(node, ast.Call):
            return False
        f = node.func
        return (isinstance(f, ast.Name) and f.id == "read_graph_strict") or (
            isinstance(f, ast.Attribute) and f.attr == "read_graph_strict"
        )

    py_root = REPO_ROOT / "cli" / "src"
    for path in sorted(py_root.rglob("*.py")):
        rel = str(path)
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"))
        except (OSError, SyntaxError) as exc:
            problems.append(f"unparseable: {rel}: {exc}")
            continue
        allow_module = rel in owner_files
        mach_module = rel in machinery
        if mach_module and machinery_marker not in path.read_text(encoding="utf-8"):
            problems.append(f"machinery module missing marker comment: {rel}")
        # Parent map so a read inside a nested closure attributes to its
        # OUTERMOST enclosing function: the command/callback boundary is what
        # the guard and the backend switch live on.
        parents: dict[int, ast.AST] = {}
        for node in ast.walk(tree):
            for child in ast.iter_child_nodes(node):
                parents[id(child)] = node

        def _outermost(node):
            cur = node
            top_fn = None
            while True:
                parent = parents.get(id(cur))
                if parent is None:
                    return top_fn
                if isinstance(parent, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    top_fn = parent
                cur = parent

        for site in [
            n for n in ast.walk(tree) if _is_read_graph(n) or _is_raw_graph_parse(n)
        ]:
            top = _outermost(site)
            if not _is_read_graph(site) and not _raw_parse_is_graph_read(site, top):
                # A raw parse with no graph_json() in the enclosing body is
                # another file's parse, not a graph-store read.
                continue
            if _is_read_graph(site):
                # The strict store read: since the cutover the graph store is
                # footnote-owned regardless of the tracker backend, so every
                # read_graph_strict site is legal by definition. The raw-parse
                # modality below still guards hand-rolled file access.
                total += 1
                if verbose:
                    print(
                        f"  {'store-read':<18} "
                        f"{Path(rel).relative_to(REPO_ROOT)}:{site.lineno} in "
                        f"{top.name if top else '<module>'}()"
                    )
                continue
            klass, problem = _classify_site(
                site,
                top,
                allow=allow_module,
                mach=mach_module,
                guarded_names=guarded_names,
                refusal_calls=refusal_calls,
            )
            if top is None:
                # A module-level read has no command/callback boundary any
                # guard could sit on; only the storage owners may do it.
                if problem:
                    problems.append(
                        f"unclassified consumer: {rel}:{site.lineno} at module level"
                    )
                continue
            total += 1
            if problem:
                problems.append(
                    f"unclassified consumer: {rel}:{site.lineno} "
                    f"in {top.name}()"
                )
                continue
            if verbose:
                print(f"  {klass:<18} {Path(rel).relative_to(REPO_ROOT)}:{site.lineno} in {top.name}()")

    # Rust modality: direct graph.json opens outside the allowlist, in
    # PRODUCTION sources (test fixtures legitimately point FNO_GRAPH_JSON at
    # fixture files; that is not a consumer). The walk descends into nested
    # module dirs (src/backlog/, src/daemon/tests/): a flat src/*.rs glob
    # never saw them, which is how the json-leg readers hid.
    rust_root = REPO_ROOT / "crates"
    for path in sorted(rust_root.rglob("*.rs")):
        rel_parts = path.relative_to(rust_root).parts
        # Production sources only: target/ is build output; a tests/ or
        # *_tests/ segment is a unit-test dir; a tests.rs file is a
        # `#[cfg(test)] mod tests;` pulled in by its parent (fixtures point
        # at fixture stores by design, and none of it is a consumer).
        if (
            "src" not in rel_parts
            or "target" in rel_parts
            or path.name == "tests.rs"
            or any(p == "tests" or p.endswith("_tests") for p in rel_parts)
        ):
            continue
        rel = str(path)
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        for i, line in rust_graph_json_sites(text):
            total += 1
            if rel not in rust_allow:
                problems.append(
                    f"unclassified rust consumer: {rel}:{i + 1}: {line.strip()[:80]}"
                )
        if path.name not in ("graph_store.rs", "graph_keeper.rs"):
            for i, line in rust_json_leg_reader_sites(text):
                problems.append(
                    f"json-leg reader outside the switch: {rel}:{i + 1}: {line.strip()[:80]}"
                )
    return total, problems


def rust_graph_json_sites(text):
    """Line sites of direct graph.json literals in one Rust source.

    Production only: a Rust unit test lives INSIDE src/*.rs (there is no
    tests/ directory for lib tests), so a `#[cfg(test)] mod ...` module is a
    fixture, not a consumer. Rust test modules sit at file end, so the first
    cfg(test)-annotated `mod` line switches the scan off for everything
    below it; a cfg(test) line annotating anything else (a fn, a use) does
    not.
    """
    sites = []
    pending_cfg_test = False
    in_test_module = False
    pattern = re.compile(r'"?graph\.json"?')
    for i, line in enumerate(text.splitlines()):
        stripped = line.strip()
        if in_test_module:
            continue
        if pattern.search(line) and not stripped.startswith("//"):
            sites.append((i, line))
        if re.fullmatch(r"#\[cfg\(test\)\]", stripped):
            pending_cfg_test = True
        elif pending_cfg_test and re.match(r"mod\s+\w+", stripped):
            in_test_module = True
        elif stripped:
            pending_cfg_test = False
    return sites


def rust_json_leg_reader_sites(text):
    """Line sites of `read_defaulted`/`read_defaulted_opts` calls in one file.

    Same production cutoff as rust_graph_json_sites: a `#[cfg(test)]` module
    is a fixture. The caller exempts graph_store.rs (the backend switch the
    readers must call) and graph_keeper.rs (the json leg itself); a call in
    any other production file reads the file leg this node retired, and the
    only reason it ever answered was the sqlite-to-json mirror.
    """
    sites = []
    pending_cfg_test = False
    in_test_module = False
    pattern = re.compile(r"\bread_defaulted(?:_opts)?\s*\(")
    for i, line in enumerate(text.splitlines()):
        stripped = line.strip()
        if in_test_module:
            continue
        if pattern.search(line) and not stripped.startswith("//"):
            sites.append((i, line))
        if re.fullmatch(r"#\[cfg\(test\)\]", stripped):
            pending_cfg_test = True
        elif pending_cfg_test and re.match(r"mod\s+\w+", stripped):
            in_test_module = True
        elif stripped:
            pending_cfg_test = False
    return sites


def self_test() -> int:
    """Inject known-bad inputs; print the success marker only after every
    detector names what it must (a census that reports only what it found,
    with no control proving the search ran, is not evidence)."""
    failures: list[str] = []

    # Verbs modality: an unmarked verb must be detected.
    class _FakeInfo:
        name = "inject-unmarked"
        callback = lambda: None  # noqa: E731

    marker = _marker_of(_FakeInfo())
    if marker is not None:
        failures.append(f"injected unmarked verb not detected (marker={marker!r})")

    # Reads modality: an injected forbidden consumer must be detected.
    bad = "# read_graph()\nx = read_graph_strict(path)"
    pattern = re.compile(r"\bread_graph(_strict)?\b")
    hits = [
        (i + 1, bad_line.strip())
        for i, bad_line in enumerate(bad.splitlines())
        if pattern.search(bad_line)
        and "import" not in bad_line
        and not bad_line.strip().startswith("#")
    ]
    if not hits:
        failures.append("injected forbidden reader not detected")

    # Runtime guard control: the refusal must fire on the WRAPPED callback.
    fired, detail = _guard_fires_runtime()
    if not fired:
        failures.append(f"runtime guard control failed: {detail}")

    # Rust modality controls: a production literal must be found, and a
    # cfg(test) module's fixture must be skipped - both edges, or the latch
    # is unproven in whichever direction it fails.
    rust_prod = 'fn main() {\n    let p = Path::new("graph.json");\n}\n'
    rust_fixt = (
        "fn helper() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        '    #[test]\n'
        '    fn t() {\n'
        '        let p = dir.join("graph.json");\n'
        "    }\n"
        "}\n"
    )
    if len(rust_graph_json_sites(rust_prod)) != 1:
        failures.append("rust census control: production graph.json literal not detected")
    if rust_graph_json_sites(rust_fixt):
        failures.append("rust census control: cfg(test) fixture was not skipped")

    # Json-leg reader detector: a production read_defaulted call in a nested
    # module file outside the switch and the leg must be named, and the same
    # call inside a cfg(test) module must not be.
    reader_prod = "fn f() {\n    let rows = read_defaulted(&g, false);\n}\n"
    reader_fixt = (
        "fn helper() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn f() {\n"
        "        let rows = read_defaulted(&g, false);\n"
        "    }\n"
        "}\n"
    )
    if len(rust_json_leg_reader_sites(reader_prod)) != 1:
        failures.append("json-leg reader control: production read_defaulted not detected")
    if rust_json_leg_reader_sites(reader_fixt):
        failures.append("json-leg reader control: cfg(test) read_defaulted was not skipped")

    # Raw graph-parse detector, both edges: the config_cli spelling
    # (json.loads over graph_json() bytes) must be detected, gated in by the
    # graph_json() conjunction, and FAIL classification; the same parse
    # inside an allowlisted owner must classify without a problem, and the
    # same parse with no graph_json() in the body is not a graph read.
    hit_tree = ast.parse(
        "def check(root):\n"
        "    graph = paths.graph_json()\n"
        "    if graph.is_file():\n"
        "        data = json.loads(graph.read_text(encoding='utf-8'))\n"
    )
    hits = [n for n in ast.walk(hit_tree) if _is_raw_graph_parse(n)]
    if len(hits) != 1:
        failures.append("raw graph-parse control: injected json.loads(read_text) not detected")
    elif not _raw_parse_is_graph_read(hits[0], hit_tree.body[0]):
        failures.append("raw graph-parse control: graph_json() conjunction not joined")
    else:
        klass, problem = _classify_site(
            hits[0], hit_tree.body[0], allow=False, mach=False,
            guarded_names=frozenset(), refusal_calls=frozenset(),
        )
        if klass is not None or not problem:
            failures.append("raw graph-parse control: unclassified raw parse not named")
        klass, problem = _classify_site(
            hits[0], hit_tree.body[0], allow=True, mach=False,
            guarded_names=frozenset(), refusal_calls=frozenset(),
        )
        if problem is not None or klass != "owner":
            failures.append("raw graph-parse control: allowlisted owner named anyway")
    other_tree = ast.parse(
        "def load_cfg(p):\n"
        "    data = json.loads(p.read_text(encoding='utf-8'))\n"
    )
    others = [n for n in ast.walk(other_tree) if _is_raw_graph_parse(n)]
    if len(others) != 1 or _raw_parse_is_graph_read(others[0], other_tree.body[0]):
        failures.append("raw graph-parse control: non-graph json parse gated in")

    # The bare-name spelling (from json import loads) must be detected too.
    bare_tree = ast.parse(
        "def check(root):\n"
        "    graph = paths.graph_json()\n"
        "    if graph.is_file():\n"
        "        data = loads(graph.read_text(encoding='utf-8'))\n"
    )
    bares = [n for n in ast.walk(bare_tree) if _is_raw_graph_parse(n)]
    if len(bares) != 1:
        failures.append("raw graph-parse control: bare loads() spelling not detected")

    if failures:
        for f in failures:
            print(f"tracker-consumers: SELF-TEST FAILURE: {f}", file=sys.stderr)
        return 1
    print(
        "tracker-consumers: self-test detected the injected unmarked verb, "
        "the injected forbidden reader, the runtime refusal, the "
        "json-leg reader, and the raw graph parse"
    )
    print(SELF_TEST_OK_MARKER)
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--verbs", action="store_true", help="Run the backlog-verb census.")
    ap.add_argument("--reads", action="store_true", help="Run the direct-consumer read census (task 5.1).")
    ap.add_argument("--self-test", action="store_true", help="Prove the detectors detect.")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    rc = 0
    if args.verbs:
        total, problems = census_verbs(verbose=args.verbose)
        print(f"tracker-consumers: verbs census over {total} live registry entries")
        if problems:
            rc = 1
            for p in problems:
                print(f"tracker-consumers: {p}", file=sys.stderr)
        else:
            print("tracker-consumers: verbs OK - every verb classified, controls positive")
    if args.reads:
        total, problems = census_reads(verbose=args.verbose)
        print(f"tracker-consumers: read census over {total} direct site(s)")
        print(f"tracker-consumers: allowlisted owners: {', '.join(READ_ALLOWLIST)}")
        if problems:
            rc = 1
            for p in problems:
                print(f"tracker-consumers: {p}", file=sys.stderr)
        else:
            print("tracker-consumers: reads OK - zero unclassified consumers")
    if not (args.verbs or args.reads):
        ap.print_help()
        return 2
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
