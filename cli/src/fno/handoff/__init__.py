"""Handoff module: exit codes and shared CLI output helpers.

The dispatch-payload protocol (``fno loop --continue`` exit-42 resume
commands) was deleted with the ``fno loop`` stub in step-5 group 3
(ab-9fd662c6); the unified loop (``fno-agents loop run``) resumes from
world state, not from dispatch payloads. What remains here is the exit
code contract and the JSON-mode output helpers the sub-apps share.
"""

from fno.handoff.exit_codes import ExitCode
from fno.handoff.output import (
    emit,
    emit_error,
    json_mode,
    merge_json_flag,
    write_output_file,
)

__all__ = [
    "ExitCode",
    "emit",
    "emit_error",
    "json_mode",
    "merge_json_flag",
    "write_output_file",
]
