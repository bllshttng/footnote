"""Public contracts and evaluator for generic delivery evidence."""

from fno.delivery.adapters import (
    LEGACY_PR_ADAPTER_VERSION,
    LEGACY_RESEARCH_ADAPTER_VERSION,
    LegacyDeliveryShadow,
    LegacyPRSnapshot,
    LegacyResearchSnapshot,
    adapt_legacy_pr,
    adapt_legacy_research,
    adapt_legacy_research_grade,
)

from fno.delivery.contracts import (
    DELIVERY_EVALUATE_RESPONSE_VERSION,
    DELIVERY_EVALUATOR_VERSION,
    DELIVERY_EVIDENCE_FACT_VERSION,
    DeliveryEvaluateResponse,
    DeliveryEvidenceFact,
    DeliveryEvidenceRejection,
    DeliveryRequirementVerdict,
    DeliveryRequirementBinding,
    DeliveryVerdict,
)
from fno.delivery.evaluator import evaluate_delivery
from fno.delivery.reader import evaluate_plan_delivery

__all__ = [
    "LEGACY_PR_ADAPTER_VERSION",
    "LEGACY_RESEARCH_ADAPTER_VERSION",
    "DELIVERY_EVALUATE_RESPONSE_VERSION",
    "DELIVERY_EVALUATOR_VERSION",
    "DELIVERY_EVIDENCE_FACT_VERSION",
    "DeliveryEvaluateResponse",
    "DeliveryEvidenceFact",
    "DeliveryEvidenceRejection",
    "DeliveryRequirementVerdict",
    "DeliveryRequirementBinding",
    "DeliveryVerdict",
    "LegacyDeliveryShadow",
    "LegacyPRSnapshot",
    "LegacyResearchSnapshot",
    "adapt_legacy_pr",
    "adapt_legacy_research",
    "adapt_legacy_research_grade",
    "evaluate_delivery",
    "evaluate_plan_delivery",
]
