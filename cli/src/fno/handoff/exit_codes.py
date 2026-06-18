"""Central exit code contract for every CLI subcommand.

Every subcommand must choose one of these codes. Callers (skill shims,
target loop, CI scripts) rely on the contract to branch correctly.

Never add a new exit code without documenting it here AND in the
affected subcommand's --help output. `fno setup`, `fno review`
each advertise only the subset they actually raise.
"""

from enum import IntEnum


class ExitCode(IntEnum):
    """Exit codes used across the fno CLI.

    SUCCESS (0):
        Phase complete or expected outcome reached. Always safe.

    ERROR (2):
        Hard error. Bad config, spawn failure, parse error, unwritable
        path. See stderr for diagnostic. Caller should not retry blindly.

    BLOCKING_FINDINGS (3):
        Review subcommand only. Artifact written, verdict is ``blocked``.
        Caller must re-run the implementation phase before the gate will
        pass.

    RESOURCE_LOCKED (11):
        Concurrent invocation blocked by lock file (review orchestrator,
        ship flow). Caller should wait or inspect the lock holder.

    DISPATCH_REQUIRED (42):
        Phase needs LLM reasoning that the CLI cannot do. stdout carries
        a JSON payload describing what the caller must produce before
        re-invoking the loop. Matches the ``exit 42`` handoff pattern
        used by target's skill shims.

    SIGINT (130):
        POSIX convention: interrupted by user (Ctrl-C). The CLI traps
        SIGINT, cleans up scratchpads/lock files, then re-raises this
        code so shells treat it as an interrupt.
    """

    SUCCESS = 0
    ERROR = 2
    BLOCKING_FINDINGS = 3
    RESOURCE_LOCKED = 11
    DISPATCH_REQUIRED = 42
    SIGINT = 130
