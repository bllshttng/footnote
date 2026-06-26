#!/usr/bin/env bash
# Regenerate the public roadmap: the styled HTML page (served by GitHub Pages
# from /docs) and the markdown table inside README.md, both from the public
# backlog (nodes flagged `public: true`). Source of truth is the local
# graph.json - never committed - so this MUST run locally; CI has no graph data.
#
#   scripts/publish-roadmap.sh [--project NAME] [--commit]
#
# Without --commit it only rewrites the two files (safe to run anytime).
# With --commit it also stages, commits, and pushes them (used by the launchd
# timer in scripts/launchd/sh.fno.publish-roadmap.plist).
set -euo pipefail

PROJECT="fno"
COMMIT=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --project) PROJECT="$2"; shift 2 ;;
    --commit)  COMMIT=1; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HTML_OUT="$ROOT/docs/roadmap/index.html"
README="$ROOT/README.md"
mkdir -p "$(dirname "$HTML_OUT")"

# Render via the repo's own module so this works even when the installed `fno`
# is stale (pre-`fno update`). PYTHONPATH points at the in-repo source.
PYTHONPATH="$ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}" python3 - "$PROJECT" "$HTML_OUT" "$README" <<'PY'
import sys, re
from fno.graph.store import read_graph
from fno.graph._constants import GRAPH_JSON
from fno.graph.roadmap_public import render_public_roadmap_html, _columns, _card_bits, _PUBLIC_COLUMNS

project, html_out, readme = sys.argv[1], sys.argv[2], sys.argv[3]
entries = read_graph(GRAPH_JSON)

# 1) Styled HTML page for GitHub Pages.
with open(html_out, "w") as fh:
    fh.write(render_public_roadmap_html(entries, project))

# 2) Markdown table for the README (When | Item | Priority). Same column +
#    leak-free fields as the HTML; one row per public node.
cols = _columns(entries, project)
label = {c: lbl for c, lbl in _PUBLIC_COLUMNS}
rows = ["| When | Item | Priority |", "|------|------|----------|"]
total = 0
for col, lbl in _PUBLIC_COLUMNS:
    for e in cols[col]:
        title, meta = _card_bits(e)
        prio = (e.get("priority") or "").strip()
        title = title.replace("|", "\\|")
        rows.append(f"| {lbl} | {title} | {prio} |")
        total += 1
table = "\n".join(rows) if total else "_No public roadmap items yet._"

start, end = "<!-- ROADMAP:START -->", "<!-- ROADMAP:END -->"
body = open(readme).read()
block = f"{start}\n{table}\n{end}"
new = re.sub(re.escape(start) + r".*?" + re.escape(end), lambda _: block, body, count=1, flags=re.S)
if new == body and start not in body:
    raise SystemExit(f"README markers not found: {start} / {end}")
open(readme, "w").write(new)
print(f"roadmap: wrote {html_out} and {total} README rows for project '{project}'")
PY

if [[ "$COMMIT" == "1" ]]; then
  cd "$ROOT"
  if ! git diff --quiet -- docs/roadmap/index.html README.md; then
    git add docs/roadmap/index.html README.md
    git commit -q -m "chore(roadmap): refresh public roadmap"
    git push -q
    echo "roadmap: committed and pushed"
  else
    echo "roadmap: no changes"
  fi
fi
