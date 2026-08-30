#!/usr/bin/env python3
"""
Standalone repo-map generator extracted from aider's RepoMap.

Generates a PageRank-weighted map of a repository's most important symbols
and their relationships. No LLM dependency — just tree-sitter + networkx.

Usage:
    python repo_map.py /path/to/repo                    # Print repo map
    python repo_map.py /path/to/repo --json              # JSON output (symbols + graph edges)
    python repo_map.py /path/to/repo --orphans           # List files with no inbound references
    python repo_map.py /path/to/repo --tokens 2048       # Adjust token budget
    python repo_map.py /path/to/repo /path/to/other/repo # Map multiple repos

Based on: https://github.com/Aider-AI/aider/blob/main/aider/repomap.py
License: Apache-2.0 (same as aider)
"""

import argparse
import json
import math
import os
import sys
import warnings
from collections import Counter, defaultdict, namedtuple
from pathlib import Path

# Suppress tree-sitter FutureWarning
warnings.simplefilter("ignore", category=FutureWarning)

try:
    import networkx as nx
except ImportError:
    print("Missing dependency: pip install networkx", file=sys.stderr)
    sys.exit(1)

try:
    from grep_ast import TreeContext, filename_to_lang
    from grep_ast.tsl import get_language, get_parser
except ImportError:
    print("Missing dependency: pip install grep-ast", file=sys.stderr)
    sys.exit(1)

try:
    from tree_sitter import Query, QueryCursor
except ImportError:
    Query = None
    QueryCursor = None

try:
    from pygments.lexers import guess_lexer_for_filename
    from pygments.token import Token
except ImportError:
    print("Missing dependency: pip install pygments", file=sys.stderr)
    sys.exit(1)


Tag = namedtuple("Tag", "rel_fname fname line name kind".split())

# Files that should always appear near the top of repo maps
IMPORTANT_FILES = {
    "README.md", "README.txt", "README.rst", "README",
    "package.json", "pyproject.toml", "setup.py", "Cargo.toml",
    "go.mod", "Gemfile", "requirements.txt", "Pipfile",
    ".gitignore", "Makefile", "Dockerfile", "docker-compose.yml",
    "tsconfig.json", "vite.config.ts", "next.config.js",
}


def estimate_tokens(text: str) -> int:
    """Estimate token count without an LLM. ~4 chars per token is standard."""
    return len(text) // 4


def read_text(fname: str) -> str | None:
    """Read a file, returning None on error."""
    try:
        with open(fname, "r", encoding="utf-8", errors="ignore") as f:
            return f.read()
    except (OSError, UnicodeDecodeError):
        return None


REPOGRAM_DIR = Path(__file__).resolve().parent
QUERIES_DIR = REPOGRAM_DIR / "queries"


def get_scm_fname(lang: str) -> Path | None:
    """Find the tree-sitter query file for a language."""
    # Bundled queries (shipped with repogram)
    path = QUERIES_DIR / f"{lang}-tags.scm"
    if path.exists():
        return path

    # Fallback: grep_ast's bundled queries
    from importlib import resources
    try:
        path = resources.files("grep_ast").joinpath("queries", f"{lang}-tags.scm")
        if path.exists():
            return path
    except (KeyError, ModuleNotFoundError):
        pass

    return None


def get_tags(fname: str, rel_fname: str) -> list[Tag]:
    """Extract definition and reference tags from a file using tree-sitter."""
    lang = filename_to_lang(fname)
    if not lang:
        return []

    try:
        language = get_language(lang)
        parser = get_parser(lang)
    except Exception:
        return []

    query_scm = get_scm_fname(lang)
    if not query_scm or not query_scm.exists():
        return []

    query_scm_text = query_scm.read_text()
    code = read_text(fname)
    if not code:
        return []

    tree = parser.parse(bytes(code, "utf-8"))

    # tree-sitter 0.25.x: Query + QueryCursor API
    # tree-sitter <0.25: language.query().captures() directly
    try:
        if QueryCursor is not None:
            query = Query(language, query_scm_text)
            cursor = QueryCursor(query)
            captures = cursor.captures(tree.root_node)
        else:
            query = language.query(query_scm_text)
            captures = query.captures(tree.root_node)
    except Exception as e:
        print(f"  query error for {rel_fname}: {e}", file=sys.stderr)
        return []

    tags = []
    saw = set()

    # captures is a dict {capture_name: [nodes]} in 0.25.x
    if isinstance(captures, dict):
        all_nodes = []
        for tag_name, nodes in captures.items():
            all_nodes += [(node, tag_name) for node in nodes]
    else:
        all_nodes = list(captures)

    for node, tag_name in all_nodes:
        if tag_name.startswith("name.definition."):
            kind = "def"
        elif tag_name.startswith("name.reference."):
            kind = "ref"
        else:
            continue

        saw.add(kind)
        tags.append(Tag(
            rel_fname=rel_fname,
            fname=fname,
            name=node.text.decode("utf-8"),
            kind=kind,
            line=node.start_point[0],
        ))

    # If we only saw defs (no refs), use pygments to backfill refs
    if "def" in saw and "ref" not in saw:
        try:
            lexer = guess_lexer_for_filename(fname, code)
            tokens = list(lexer.get_tokens(code))
            for token in tokens:
                if token[0] in Token.Name:
                    tags.append(Tag(
                        rel_fname=rel_fname,
                        fname=fname,
                        name=token[1],
                        kind="ref",
                        line=-1,
                    ))
        except Exception:
            pass

    return tags


def collect_files(root: str) -> list[str]:
    """Walk a directory and collect source files, respecting .gitignore via git ls-files."""
    root = os.path.abspath(root)

    # Try git ls-files first (respects .gitignore)
    try:
        import subprocess
        result = subprocess.run(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard"],
            cwd=root, capture_output=True, text=True, timeout=30
        )
        if result.returncode == 0:
            files = []
            for line in result.stdout.strip().split("\n"):
                if line:
                    full = os.path.join(root, line)
                    if os.path.isfile(full):
                        files.append(full)
            return files
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    # Fallback: walk the directory
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        # Skip hidden dirs and common non-source dirs
        dirnames[:] = [d for d in dirnames if not d.startswith(".") and d not in
                       ("node_modules", "__pycache__", ".git", "dist", "build", "venv", ".venv")]
        for f in filenames:
            if not f.startswith("."):
                files.append(os.path.join(dirpath, f))
    return files


def build_graph(root: str, files: list[str], verbose: bool = False):
    """Build the dependency graph and run PageRank. Returns (ranked_tags, graph, defines, references)."""
    defines = defaultdict(set)
    references = defaultdict(list)
    definitions = defaultdict(set)

    for fname in files:
        rel_fname = os.path.relpath(fname, root)
        if verbose:
            print(f"  Scanning: {rel_fname}", file=sys.stderr)

        tags = get_tags(fname, rel_fname)
        for tag in tags:
            if tag.kind == "def":
                defines[tag.name].add(rel_fname)
                definitions[(rel_fname, tag.name)].add(tag)
            elif tag.kind == "ref":
                references[tag.name].append(rel_fname)

    if not references:
        references = {k: list(v) for k, v in defines.items()}

    idents = set(defines.keys()).intersection(set(references.keys()))
    G = nx.MultiDiGraph()

    # Self-edges for defs with no references
    for ident in defines.keys():
        if ident not in references:
            for definer in defines[ident]:
                G.add_edge(definer, definer, weight=0.1, ident=ident)

    for ident in idents:
        definers = defines[ident]
        mul = 1.0

        is_snake = ("_" in ident) and any(c.isalpha() for c in ident)
        is_kebab = ("-" in ident) and any(c.isalpha() for c in ident)
        is_camel = any(c.isupper() for c in ident) and any(c.islower() for c in ident)

        if (is_snake or is_kebab or is_camel) and len(ident) >= 8:
            mul *= 10
        if ident.startswith("_"):
            mul *= 0.1
        if len(defines[ident]) > 5:
            mul *= 0.1

        for referencer, num_refs in Counter(references[ident]).items():
            for definer in definers:
                num_refs_scaled = math.sqrt(num_refs)
                G.add_edge(referencer, definer, weight=mul * num_refs_scaled, ident=ident)

    try:
        ranked = nx.pagerank(G, weight="weight")
    except ZeroDivisionError:
        return [], G, defines, references

    # Distribute rank across definitions
    ranked_definitions = defaultdict(float)
    for src in G.nodes:
        src_rank = ranked[src]
        total_weight = sum(data["weight"] for _s, _d, data in G.out_edges(src, data=True))
        if total_weight == 0:
            continue
        for _s, dst, data in G.out_edges(src, data=True):
            data["rank"] = src_rank * data["weight"] / total_weight
            ranked_definitions[(dst, data["ident"])] += data["rank"]

    ranked_tags = []
    for (fname, ident), rank in sorted(ranked_definitions.items(), reverse=True, key=lambda x: x[1]):
        ranked_tags += list(definitions.get((fname, ident), []))

    # Add files that had no tags
    tagged_fnames = {rt[0] if isinstance(rt, Tag) else rt for rt in ranked_tags}
    all_rel = sorted(set(os.path.relpath(f, root) for f in files))
    for rel in all_rel:
        if rel not in tagged_fnames:
            ranked_tags.append((rel,))

    return ranked_tags, G, defines, references


def render_tree_map(root: str, ranked_tags: list, max_tokens: int = 1024) -> str:
    """Render the ranked tags into a tree-context format within a token budget."""
    output_parts = []
    current_file = None
    current_lines = []

    for tag in ranked_tags:
        if isinstance(tag, Tag):
            rel_fname = tag.rel_fname
        else:
            rel_fname = tag[0]

        if rel_fname != current_file:
            if current_file is not None:
                output_parts.append(f"\n{current_file}:")
                if current_lines:
                    # Try to render with TreeContext
                    abs_fname = os.path.join(root, current_file)
                    rendered = render_file_context(abs_fname, current_file, current_lines)
                    if rendered:
                        output_parts.append(rendered)
                current_lines = []
            current_file = rel_fname

        if isinstance(tag, Tag):
            current_lines.append(tag.line)

        # Check token budget
        current_output = "".join(output_parts)
        if estimate_tokens(current_output) > max_tokens:
            break

    # Flush last file
    if current_file and current_lines:
        output_parts.append(f"\n{current_file}:")
        abs_fname = os.path.join(root, current_file)
        rendered = render_file_context(abs_fname, current_file, current_lines)
        if rendered:
            output_parts.append(rendered)

    result = "".join(output_parts)
    # Truncate long lines
    result = "\n".join(line[:100] for line in result.splitlines()) + "\n"
    return result


def render_file_context(abs_fname: str, rel_fname: str, lines_of_interest: list[int]) -> str:
    """Use grep_ast's TreeContext to render relevant lines with surrounding context."""
    code = read_text(abs_fname)
    if not code:
        return ""
    if not code.endswith("\n"):
        code += "\n"

    try:
        context = TreeContext(
            rel_fname, code,
            color=False, line_number=False, child_context=False,
            last_line=False, margin=0, mark_lois=False, loi_pad=0,
            show_top_of_file_parent_scope=False,
        )
        context.add_lines_of_interest(lines_of_interest)
        context.add_context()
        return context.format()
    except Exception:
        # Fallback: just show the lines
        lines = code.splitlines()
        out = []
        for loi in sorted(set(lines_of_interest)):
            if 0 <= loi < len(lines):
                out.append(f"  {lines[loi]}")
        return "\n".join(out) + "\n"


def find_orphans(root: str, G: nx.MultiDiGraph, files: list[str]) -> list[dict]:
    """Find files with no inbound edges (orphaned — nothing references them)."""
    all_rel = set(os.path.relpath(f, root) for f in files)
    graph_nodes = set(G.nodes)

    orphans = []

    for rel in sorted(all_rel):
        # Not in graph at all
        if rel not in graph_nodes:
            orphans.append({"file": rel, "reason": "not_in_graph", "inbound_refs": 0})
            continue

        # In graph but no inbound edges (only self-edges or outbound)
        in_edges = [e for e in G.in_edges(rel) if e[0] != rel]
        if not in_edges:
            out_edges = [e for e in G.out_edges(rel) if e[1] != rel]
            orphans.append({
                "file": rel,
                "reason": "no_inbound_refs",
                "inbound_refs": 0,
                "outbound_refs": len(out_edges),
            })

    return orphans


def build_json_output(root: str, ranked_tags: list, G: nx.MultiDiGraph,
                      defines: dict, references: dict, files: list[str]) -> dict:
    """Build structured JSON output with graph data."""
    all_rel = sorted(set(os.path.relpath(f, root) for f in files))

    # File rankings from PageRank
    try:
        ranked = nx.pagerank(G, weight="weight")
    except ZeroDivisionError:
        ranked = {}

    file_rankings = sorted(
        [(fname, rank) for fname, rank in ranked.items()],
        key=lambda x: x[1], reverse=True
    )

    # Symbol definitions: {symbol_name: [files_that_define_it]}
    symbol_defs = {}
    for name, fnames in defines.items():
        symbol_defs[name] = sorted(fnames) if isinstance(fnames, set) else fnames

    # Orphans
    orphans = find_orphans(root, G, files)

    return {
        "root": root,
        "total_files": len(all_rel),
        "files_in_graph": len(set(G.nodes)),
        "files_not_in_graph": len(all_rel) - len(set(G.nodes)),
        "file_rankings": [{"file": f, "rank": round(r, 6)} for f, r in file_rankings[:50]],
        "orphan_candidates": orphans,
        "total_symbols_defined": len(defines),
        "total_symbols_referenced": len(references),
        "edges": G.number_of_edges(),
        "nodes": G.number_of_nodes(),
    }


def main():
    parser = argparse.ArgumentParser(
        description="Generate a PageRank-weighted repo map",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s .                          # Map current directory
  %(prog)s ~/code/my-project          # Map a specific repo
  %(prog)s . --json                   # JSON output with graph data
  %(prog)s . --orphans                # List orphaned files
  %(prog)s . --tokens 4096            # Larger token budget for more detail
  %(prog)s repo-a repo-b              # Map multiple repos
        """,
    )
    parser.add_argument("repos", nargs="+", help="Repository path(s) to map")
    parser.add_argument("--tokens", type=int, default=2048, help="Max token budget for tree output (default: 2048)")
    parser.add_argument("--json", action="store_true", help="Output structured JSON instead of tree map")
    parser.add_argument("--orphans", action="store_true", help="List orphaned files (no inbound references)")
    parser.add_argument("--verbose", "-v", action="store_true", help="Show progress on stderr")

    args = parser.parse_args()

    for repo_path in args.repos:
        root = os.path.abspath(repo_path)
        if not os.path.isdir(root):
            print(f"Error: {root} is not a directory", file=sys.stderr)
            sys.exit(1)

        if args.verbose:
            print(f"Scanning: {root}", file=sys.stderr)

        files = collect_files(root)
        if args.verbose:
            print(f"  Found {len(files)} files", file=sys.stderr)

        ranked_tags, G, defines, references = build_graph(root, files, verbose=args.verbose)

        if args.json:
            output = build_json_output(root, ranked_tags, G, defines, references, files)
            print(json.dumps(output, indent=2))

        elif args.orphans:
            orphans = find_orphans(root, G, files)
            if orphans:
                print(f"# Orphan Candidates in {root}")
                print(f"# {len(orphans)} files with no inbound references\n")

                not_in_graph = [o for o in orphans if o["reason"] == "not_in_graph"]
                no_inbound = [o for o in orphans if o["reason"] == "no_inbound_refs"]

                if not_in_graph:
                    print(f"## Not in dependency graph ({len(not_in_graph)} files)")
                    print("# These files have no recognized symbols (may be config, data, etc.)\n")
                    for o in not_in_graph:
                        print(f"  {o['file']}")

                if no_inbound:
                    print(f"\n## No inbound references ({len(no_inbound)} files)")
                    print("# These files define symbols but nothing references them\n")
                    for o in no_inbound:
                        out = o.get("outbound_refs", 0)
                        suffix = f"  (references {out} other files)" if out else ""
                        print(f"  {o['file']}{suffix}")
            else:
                print("No orphaned files found.")

        else:
            # Default: tree map output
            if len(args.repos) > 1:
                print(f"\n{'=' * 60}")
                print(f"# {root}")
                print(f"{'=' * 60}\n")

            tree_map = render_tree_map(root, ranked_tags, max_tokens=args.tokens)
            if tree_map.strip():
                print(tree_map)
            else:
                print(f"# No mappable symbols found in {root}", file=sys.stderr)


if __name__ == "__main__":
    main()
