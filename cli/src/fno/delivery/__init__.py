"""Public contracts and evaluator for generic delivery evidence."""

from fno.delivery.adapters import (
    LEGACY_PR_ADAPTER_VERSION,
    LEGACY_RESEARCH_ADAPTER_VERSION,
    LegacyDeliveryShadow,
    LegacyPRSnapshot,
    LegacyResearchSnapshot,
    adapt_legacy_pr,
    adapt_legacy_research,
)

from fno.delivery.contracts import (
    DELIVERY_EVALUATOR_VERSION,
    DELIVERY_EVIDENCE_FACT_VERSION,
    DeliveryEvidenceFact,
    DeliveryRequirementVerdict,
    DeliveryVerdict,
)
from fno.delivery.evaluator import evaluate_delivery

__all__ = [
    "LEGACY_PR_ADAPTER_VERSION",
    "LEGACY_RESEARCH_ADAPTER_VERSION",
    "DELIVERY_EVALUATOR_VERSION",
    "DELIVERY_EVIDENCE_FACT_VERSION",
    "DeliveryEvidenceFact",
    "DeliveryRequirementVerdict",
    "DeliveryVerdict",
    "LegacyDeliveryShadow",
    "LegacyPRSnapshot",
    "LegacyResearchSnapshot",
    "adapt_legacy_pr",
    "adapt_legacy_research",
    "evaluate_delivery",
]
