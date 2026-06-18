"""fno.graph - Feature graph management module.

Public API (re-exported for callers):
    mutate_graph        alias for locked_mutate_graph
    recompute_statuses  status derivation
    render_graph_md     kanban rendering
"""
from __future__ import annotations

from fno.graph._constants import (  # noqa: F401
    GRAPH_JSON,
    GRAPH_MD,
    GRAPH_ARCHIVE_JSON,
    LEDGER_JSON,
    BRIEFS_DIR,
    ID_PREFIX,
    GRAPH_LOCK_FILE,
    LOCK_TTL_HOURS,
    PRIORITY_ORDER,
)
from fno.graph.store import (  # noqa: F401
    GraphCorruptError,
    _acquire_flock,
    _apply_graph_defaults,
    _read_json,
    _release_flock,
    _write_json,
    locked_mutate_graph,
    read_graph,
)
from fno.graph.statuses import (  # noqa: F401
    is_stale_lock,
    recompute_statuses,
)
from fno.graph.render import (  # noqa: F401
    render_graph_md,
    _kanban_column,
    _kanban_card,
    _graph_sort_key,
    _lane_sort_key,
    _project_key,
    UNSCOPED_LABEL,
)
from fno.graph.depends import (  # noqa: F401
    _collect_frontmatter_depends,
    _parse_frontmatter,
    _resolve_depends_on,
    _sequence_token,
    _derive_title,
    _parse_inline_yaml_list,
)

# Canonical alias
mutate_graph = locked_mutate_graph
