#!/usr/bin/env bash
# node-id.sh -- single source of truth for "what kind of work id is this arg".
#
# Sourced by graph-resolve.sh (target) and parse-claims-arg.sh (blueprint) so
# the fno-vs-external-vs-none classification lives in one place. When footnote
# adopts an opaque external id as a work handle, this is the only site that has
# to learn the new shape; the resolvers that source it stay unchanged. That is
# the point of centralizing it: the fno-id-shape test used to be duplicated in
# every resolver, each deciding independently what "not an fno id" meant.
#
# Source this file, then call node_id_kind:
#   source scripts/lib/node-id.sh
#   case "$(node_id_kind "$arg")" in fno|external|none) ... ;;
#
# Public API:
#   node_id_kind <arg>   echoes fno | external | none, and sets:
#     node_id_value   the arg on fno/external; empty on none
#
# Classification:
#   fno       a footnote graph node id (<prefix>-<4..8 hex>), resolvable
#             against graph.json.
#   external  a recognized external tracker id. footnote treats it as an opaque
#             work handle: it keys claims and sidecars on it but never resolves
#             it against graph.json.
#   none      not an id (a file path, a feature description, or anything else).
#
# The lowercase-all-hex corner is genuinely ambiguous: an external key shaped
# like `eng-4411` matches the fno regex, so it is classified fno and soft-fails
# through the normal graph lookup rather than mis-routed. External trackers in
# practice use uppercase keys (ENG-441, PROJ-88) or carry a path separator
# (owner/repo#123), neither of which collides.

# A footnote node id: lowercase prefix, 4-8 hex suffix. This is the shape both
# resolvers already matched; centralized here so it cannot drift between them.
# The Python authority is fno.graph._constants.is_wellformed_node_id; this regex
# must stay aligned with it, pinned by test_node_id_sh.py. The shell copy exists
# because the resolvers have a legacy fallback for environments where the fno
# Python package is unavailable.
_NODE_ID_FNO_RE='^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$'
# Recognized external tracker shapes. Add a clause here when a new backend
# ships; the sourcing resolvers need no other change.
_NODE_ID_LINEAR_JIRA_RE='^[A-Z][A-Z0-9_]+-[0-9]+$'
_NODE_ID_GITHUB_ISSUE_RE='^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+#[0-9]+$'

node_id_kind() {
    local arg="${1-}"
    node_id_value=""
    [[ -z "$arg" ]] && { echo none; return 0; }
    if [[ "$arg" =~ $_NODE_ID_FNO_RE ]]; then
        node_id_value="$arg"; echo fno; return 0
    fi
    if [[ "$arg" =~ $_NODE_ID_LINEAR_JIRA_RE ]] || [[ "$arg" =~ $_NODE_ID_GITHUB_ISSUE_RE ]]; then
        node_id_value="$arg"; echo external; return 0
    fi
    echo none; return 0
}
