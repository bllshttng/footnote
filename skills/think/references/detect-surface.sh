#!/usr/bin/env bash
# detect-surface.sh - structural surface-detection helper for /think.
#
# Reads design-doc text on stdin and emits one of:
#   frontend-touching   one or more frontend signals matched, no backend signal
#   backend-only        one or more backend signals matched, no frontend signal
#   mixed               both frontend AND backend signals matched
#   unknown             neither family matched (treat like backend-only at
#                       call sites: no prompt fires)
#
# Detection is deliberately structural: word-boundary matches against a closed
# vocabulary plus a small set of filename glob hits. The plan's Domain Pitfall
# #2 specifically calls out LLM detection non-determinism, so the rules must
# reproduce identical results for identical input. This helper is the anchor.
#
# Locked vocabulary (per plan 2026-05-04-think-spec-executor-routing-prompts):
#
#   Frontend nouns:        UI, page, screen, component, button, form, modal,
#                          dropdown, sidebar, layout
#   Frontend frameworks:   React, Vue, Svelte, Next.js, Angular, Solid
#   Frontend filenames:    .tsx, .jsx, components/, routes/, src/styles/
#
#   Backend nouns:         API, schema, migration, queue, worker, batch, ETL,
#                          ingest
#
# Word-boundary matching avoids the classic substring trap (e.g. "inform"
# would otherwise match "form"). The grep `\b` anchor handles this; case
# folding via `-i` keeps detection independent of the author's typography.
# bash 3.2 compatible (macOS default).

set -uo pipefail

INPUT="$(cat)"
[[ -z "$INPUT" ]] && { echo "unknown"; exit 0; }

has_frontend=0
has_backend=0

# `\b` works under GNU and BSD grep alike. `-E` for extended regex; `-i` for
# case-insensitive matching. `-q` silences output - we only care about the
# exit status. The two arms (nouns and frameworks) are merged into one alt
# group to keep the helper to a single grep per family.
fe_words='\b(UI|page|screen|component|button|form|modal|dropdown|sidebar|layout|React|Vue|Svelte|Next\.js|Angular|Solid)\b'
fe_paths='(\.tsx|\.jsx|components/|routes/|src/styles/)'
be_words='\b(API|schema|migration|queue|worker|batch|ETL|ingest)\b'

if printf '%s' "$INPUT" | grep -Eqi "$fe_words"; then
    has_frontend=1
fi
# The filename arm runs only when the noun arm missed. Both arms set the
# same has_frontend bit, so a second match is wasted work. The `has_backend`
# arm cannot short-circuit because mixed-surface classification needs both
# bits regardless of which family fired first.
if [[ "$has_frontend" -eq 0 ]] && printf '%s' "$INPUT" | grep -Eq "$fe_paths"; then
    # Filename hits use case-sensitive matching: extensions and folder names
    # are conventionally lowercase. Case-insensitive here would let "API"
    # match "api/" routes accidentally even though the filename arm only
    # cares about literal frontend folder conventions.
    has_frontend=1
fi
if printf '%s' "$INPUT" | grep -Eqi "$be_words"; then
    has_backend=1
fi

if [[ "$has_frontend" -eq 1 && "$has_backend" -eq 1 ]]; then
    echo "mixed"
elif [[ "$has_frontend" -eq 1 ]]; then
    echo "frontend-touching"
elif [[ "$has_backend" -eq 1 ]]; then
    echo "backend-only"
else
    echo "unknown"
fi
