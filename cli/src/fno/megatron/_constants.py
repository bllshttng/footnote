"""Shared constants for the megatron package.

Lives at the package's import-cycle floor: imports nothing from other
megatron modules. ``state.py`` and ``artifact.py`` both depend on this
module; this module depends on neither.

Adding a new terminal status? Update ``TERMINAL_STATUSES`` here and the
matching event-vocabulary mapping in ``state.py::_TERMINAL_EVENT_STATUSES``
in the same change. The single-source rule means the artifact writer
trigger and the state-machine guard cannot drift.
"""
from __future__ import annotations

# Terminal statuses for a mission. Used by:
#   - state.py::update_status to decide when to call write_mission_artifact
#   - artifact.py::write_mission_artifact as the early-return guard
#   - _ALLOWED_TRANSITIONS in state.py to express "terminal -> self only"
TERMINAL_STATUSES: frozenset[str] = frozenset({"complete", "cancelled", "failed"})
