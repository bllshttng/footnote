"""Durable typed resume receipts (x-c3a2).

A receipt is durable EVIDENCE of where a target session left off, never write
AUTHORITY. Authority to resume is the live claim + liveness + git HEAD,
revalidated against the receipt before any successor write.

Receipts consume the existing schema-owned append-only event journal and its
derived reducers (fno.scoreboard.fold); they create no resume database and
never mutate historical observations. The reducer already canonicalizes
identity (dedup by signature) and selects latest-by-parsed-timestamp with a
deterministic complete-on-tie tiebreak; this module reuses that verbatim, so
the same event duplicated across the global + delivery-root journals folds to
one observation in timestamp order.
"""

from .receipt import (
    MalformedReceiptError,
    NextAction,
    RECEIPT_VERSION,
    RevalidationResult,
    ResumeReceipt,
    ReceiptIdentity,
    build_receipt,
    canonicalize_node_events,
    detect_duplicate_generation,
    latest_observation,
    load_receipt,
    revalidate,
    receipt_path,
    write_receipt,
)

__all__ = [
    "MalformedReceiptError",
    "NextAction",
    "RECEIPT_VERSION",
    "ReceiptIdentity",
    "RevalidationResult",
    "ResumeReceipt",
    "build_receipt",
    "canonicalize_node_events",
    "detect_duplicate_generation",
    "latest_observation",
    "load_receipt",
    "receipt_path",
    "revalidate",
    "write_receipt",
]
