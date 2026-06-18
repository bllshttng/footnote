"""Native review orchestrator (phase 03).

This package ports the sigma-review skill into the CLI so ``ab review``
can run the 6-agent panel without dispatching a skill. The full
implementation is split across:

- :mod:`fno.review.orchestrator` - dispatches workers, aggregates findings
- :mod:`fno.review.confidence_scorer` - Haiku-backed scoring pass
- :mod:`fno.review.report_builder` - writes the gate artifact
- :mod:`fno.review.prompts` - 6 bundled agent prompts

Phase 03 ships the module skeleton and prompt extraction. Full
6-worker parallel dispatch and Haiku confidence scoring land in a
follow-up spec; the orchestrator runs in a minimal synchronous mode
until then so the import graph is complete and ``ab gate check
quality_check_passed`` has a well-defined producer.
"""

from fno.review.orchestrator import (
    AGENT_NAMES,
    Finding,
    OrchestratorResult,
    load_prompts,
)

__all__ = ["AGENT_NAMES", "Finding", "OrchestratorResult", "load_prompts"]
