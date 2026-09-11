#!/usr/bin/env bash
# canon-doc-marker.sh - the one reader for canon-doc marker blocks.
#
# hooks/precompact-canon-doc.sh fences blocks with `<!-- fno:<name> -->`
# markers; king-postcompact-reinject.sh and the reign check-in body read
# them back. The parsing lives here so the three sites cannot drift.

# canon_doc_extract_marker <file> <marker-name>
#
# Prints the raw text between `<!-- fno:<marker-name> -->` and its closing
# `<!-- /fno:<marker-name> -->`, marker lines excluded, nothing trimmed.
# A missing closing marker (a partial hand edit) is CONTENT, never a parse
# error: the capture runs to the next machine boundary instead - another
# marker fence, one of the writer's own section headings, or end of file.
# The operator's own `## ` headings never bound the capture: bounding on
# any heading dropped everything the operator wrote below their own
# formatting. Exit 0 when the open marker exists (content may be empty),
# 1 when it does not or the file is unreadable; callers seed their
# placeholder on 1.
canon_doc_extract_marker() {
  local file="$1" marker="$2"
  [[ -n "$file" && -f "$file" && -n "$marker" ]] || return 1
  awk -v m="$marker" '
    !grab && index($0, "<!-- fno:" m " -->") { grab = 1; found = 1; next }
    grab && index($0, "<!-- /fno:" m " -->") { exit }
    grab && /^<!-- fno:[A-Za-z0-9._-]+ -->[[:space:]]*$/ { exit }
    grab && /^## (Merge order and why|Open decisions awaiting the operator|Gaps and open thinking|Workarounds in force|User notes) \(/ { exit }
    grab { print }
    END { exit found ? 0 : 1 }
  ' "$file" 2>/dev/null
}

# canon_doc_user_placeholder
#
# The seed line the writer plants when no fno:user block exists yet. Spelled
# once here: a reader that compared against a drifted copy would surface the
# placeholder itself as if the user had typed it.
canon_doc_user_placeholder() {
  printf '%s\n' "_(write here; the machine reads this every refresh and never edits it)_"
}

# canon_doc_is_placeholder <text>
#
# True when the captured user-block text is only the seed placeholder.
canon_doc_is_placeholder() {
  local stripped want
  stripped="$(printf '%s' "$1" | tr -d '[:space:]')"
  want="$(canon_doc_user_placeholder | tr -d '[:space:]')"
  [[ -n "$stripped" && "$stripped" == "$want" ]]
}
