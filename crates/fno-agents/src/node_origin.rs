//! Request-origin normalization for node birth (x-1005).
//!
//! One owner for the decision "who requested this work", derived from the
//! birth provenance a node already carries plus the minimal evidence field
//! this contract adds. The categories are the board's four buckets:
//!
//! - `operator_request`      — a human asked for this (explicit `source_kind`)
//! - `agent_discovery`       — an agent found it, with explicit evidence
//! - `automated_followup`    — a machine follow-up (retro land, decompose)
//! - `unknown`               — everything else, and unknown stays unknown
//!
//! The rules this module refuses to relax: a recorder harness or an organic
//! default never establishes origin; a later ruling never rewrites birth; and
//! request intent is never backfilled from substrings of the title or
//! description ("OPERATOR RAISED" in prose is prose, not provenance).
//!
//! Python birth assembly is a transport caller: it posts birth records to
//! `fno-agents node-origin resolve` and stamps the receipt. The decision
//! lives only here.

use serde::{Deserialize, Serialize};

/// The four board buckets, in the vocabulary Python mirrors in
/// `fno.graph._constants.REQUEST_ORIGINS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOrigin {
    OperatorRequest,
    AgentDiscovery,
    AutomatedFollowup,
    Unknown,
}

impl RequestOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestOrigin::OperatorRequest => "operator_request",
            RequestOrigin::AgentDiscovery => "agent_discovery",
            RequestOrigin::AutomatedFollowup => "automated_followup",
            RequestOrigin::Unknown => "unknown",
        }
    }

    /// Parse the stored vocabulary; out-of-vocabulary values re-derive rather
    /// than echo, so a corrupt field can never pin a wrong origin forever.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "operator_request" => Some(RequestOrigin::OperatorRequest),
            "agent_discovery" => Some(RequestOrigin::AgentDiscovery),
            "automated_followup" => Some(RequestOrigin::AutomatedFollowup),
            "unknown" => Some(RequestOrigin::Unknown),
            _ => None,
        }
    }
}

/// The birth record a creation caller posts. Every field is birth-time fact:
/// who explicitly declared the source kind, which caller is giving birth
/// (`birth_channel`), and the producing-event reference the contract requires
/// before an agent or automated origin may be claimed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BirthRecord {
    /// A previously stamped origin. Birth is written once: a present,
    /// in-vocabulary value wins and everything else is ignored.
    #[serde(default)]
    pub request_origin: Option<String>,
    /// The explicit source-kind declaration at birth (operator_request,
    /// from_observation, organic, ...). Absent for callers that never ask.
    #[serde(default)]
    pub source_kind: Option<String>,
    /// Which birth caller is writing (idea, new, capture_promote, intake,
    /// retro_land, decompose, ...).
    #[serde(default)]
    pub birth_channel: Option<String>,
    /// Explicit producing-event reference (fu-id, mail id, event id, plan
    /// source). The gate for every non-operator origin.
    #[serde(default)]
    pub origin_evidence: Option<String>,
}

/// The stamp Python writes onto the node at birth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub origin: RequestOrigin,
    pub evidence: Option<String>,
}

/// Channels whose births are machine follow-ups by construction. A decomposed
/// child derives from its parent's plan, so it must not silently gain its
/// parent's human origin.
fn is_automated_channel(channel: Option<&str>) -> bool {
    matches!(channel, Some("retro_land") | Some("decompose"))
}

/// Source kinds that explicitly declare an agent observed or supervised the
/// work. With evidence these are agent discoveries; without, unknown.
fn is_agent_declared_kind(kind: Option<&str>) -> bool {
    matches!(kind, Some("from_observation") | Some("from_supervisor"))
}

/// Resolve one birth record. Total function: every input maps to a category,
/// and the maps that matter are the refusals.
pub fn resolve(record: &BirthRecord) -> Resolution {
    let evidence = record
        .origin_evidence
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    // Birth is written once. An in-vocabulary prior wins outright; a corrupt
    // one re-derives below rather than pinning the corruption forever.
    if let Some(prior) = record.request_origin.as_deref() {
        if let Some(origin) = RequestOrigin::parse(prior) {
            return Resolution { origin, evidence };
        }
    }

    // Explicit operator origin wins, evidence or not: the flag IS the
    // operator's declaration; evidence only travels beside it.
    if record.source_kind.as_deref() == Some("operator_request") {
        return Resolution {
            origin: RequestOrigin::OperatorRequest,
            evidence,
        };
    }

    // Agent and automated origins need explicit producing-event evidence.
    // A recorder harness, an organic default, or bare prose never qualifies.
    let evidenced = evidence.is_some();
    let category = if record.source_kind.as_deref() == Some("operator_request") {
        RequestOrigin::OperatorRequest
    } else if evidenced && is_automated_channel(record.birth_channel.as_deref()) {
        RequestOrigin::AutomatedFollowup
    } else if evidenced && is_agent_declared_kind(record.source_kind.as_deref()) {
        RequestOrigin::AgentDiscovery
    } else {
        RequestOrigin::Unknown
    };

    Resolution {
        origin: category,
        evidence,
    }
}

/// `node-origin resolve --payload <json|->`: one batch call over birth
/// records. Reads a JSON array, prints `{"results":[...]}` aligned by index.
/// Exit 0 on any resolvable payload; exit 2 on a malformed one so a caller
/// can fail open to unknown instead of stamping a guess.
pub fn run_node_origin(args: &[String]) -> i32 {
    let payload = match parse_args(args) {
        Ok(payload) => payload,
        Err(message) => {
            eprintln!("fno-agents node-origin: {message}");
            eprintln!("usage: fno-agents node-origin resolve --payload <json|->");
            return 2;
        }
    };

    let records: Vec<BirthRecord> = match serde_json::from_str(&payload) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("fno-agents node-origin: payload is not a birth-record array: {error}");
            return 2;
        }
    };

    let results: Vec<Resolution> = records.iter().map(resolve).collect();
    match serde_json::to_string(&serde_json::json!({ "results": results })) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(error) => {
            eprintln!("fno-agents node-origin: serialization error: {error}");
            1
        }
    }
}

fn parse_args(args: &[String]) -> Result<String, String> {
    let mut payload: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "resolve" => {}
            "--payload" => {
                payload = Some(
                    iter.next()
                        .ok_or_else(|| "--payload needs a value".to_string())?
                        .clone(),
                )
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    match payload.as_deref() {
        Some("-") | None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|error| format!("reading payload from stdin: {error}"))?;
            Ok(buf)
        }
        Some(value) => Ok(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        source_kind: Option<&str>,
        channel: Option<&str>,
        evidence: Option<&str>,
    ) -> BirthRecord {
        BirthRecord {
            request_origin: None,
            source_kind: source_kind.map(str::to_string),
            birth_channel: channel.map(str::to_string),
            origin_evidence: evidence.map(str::to_string),
        }
    }

    /// AC1-HP: the same recorder context, two births, two origins. The
    /// recorder fields are identical and never consulted; only the explicit
    /// declaration and evidence separate the two.
    #[test]
    fn explicit_request_and_evidenced_discovery_diverge_under_one_recorder() {
        let requested = record(Some("operator_request"), Some("idea"), None);
        let discovered = record(
            Some("from_observation"),
            Some("idea"),
            Some("mail-inject:fu-k3j2d1"),
        );
        assert_eq!(resolve(&requested).origin, RequestOrigin::OperatorRequest);
        assert_eq!(resolve(&discovered).origin, RequestOrigin::AgentDiscovery);
        assert_eq!(
            resolve(&discovered).evidence.as_deref(),
            Some("mail-inject:fu-k3j2d1")
        );
    }

    /// AC1-EDGE: a legacy organic row and a recorder harness alone stay
    /// Unknown, and "OPERATOR RAISED" in prose is never read as provenance
    /// (the resolver has no field that could carry it — asserted by shape).
    #[test]
    fn organic_harness_alone_and_prose_never_become_human_origin() {
        let organic = record(Some("organic"), Some("idea"), None);
        let harness_only = record(None, Some("new"), None);
        assert_eq!(resolve(&organic).origin, RequestOrigin::Unknown);
        assert_eq!(resolve(&harness_only).origin, RequestOrigin::Unknown);
        // The struct carries no title/details field, so substring backfill is
        // impossible by construction; the JSON round-trip drops unknown keys.
        let raw = serde_json::json!({
            "source_kind": "organic",
            "title": "OPERATOR RAISED: fix the thing"
        });
        let parsed: BirthRecord = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(resolve(&parsed).origin, RequestOrigin::Unknown);
    }

    #[test]
    fn agent_discovery_requires_evidence_even_when_declared() {
        let bare = record(Some("from_observation"), Some("idea"), None);
        assert_eq!(resolve(&bare).origin, RequestOrigin::Unknown);
        let blank = record(Some("from_supervisor"), Some("idea"), Some("   "));
        assert_eq!(resolve(&blank).origin, RequestOrigin::Unknown);
    }

    #[test]
    fn automated_channels_map_to_automated_followup_with_evidence() {
        let landed = record(
            Some("organic"),
            Some("retro_land"),
            Some("retro:2026-09-08"),
        );
        let child = record(None, Some("decompose"), Some("parent:x-37af"));
        assert_eq!(resolve(&landed).origin, RequestOrigin::AutomatedFollowup);
        assert_eq!(resolve(&child).origin, RequestOrigin::AutomatedFollowup);
        // The same channels without evidence stay unknown.
        assert_eq!(
            resolve(&record(Some("organic"), Some("retro_land"), None)).origin,
            RequestOrigin::Unknown
        );
    }

    /// AC2-EDGE at the decision layer: a later read of an already-born node
    /// echoes the stamped origin, even as other inputs change underneath.
    #[test]
    fn prior_origin_is_immutable_birth() {
        let mut reborn = record(Some("operator_request"), Some("idea"), Some("brief:p1"));
        reborn.request_origin = Some("agent_discovery".into());
        let resolved = resolve(&reborn);
        assert_eq!(resolved.origin, RequestOrigin::AgentDiscovery);
        assert_eq!(resolved.evidence.as_deref(), Some("brief:p1"));
    }

    #[test]
    fn intake_preserves_evidence_without_claiming_origin() {
        let intaken = record(
            None,
            Some("intake"),
            Some("plan:20260907-request-origin.md"),
        );
        let resolved = resolve(&intaken);
        assert_eq!(resolved.origin, RequestOrigin::Unknown);
        assert_eq!(
            resolved.evidence.as_deref(),
            Some("plan:20260907-request-origin.md")
        );
    }

    #[test]
    fn corrupt_prior_rederives_instead_of_echoing() {
        let mut corrupt = record(Some("operator_request"), Some("idea"), None);
        corrupt.request_origin = Some("human".into());
        assert_eq!(resolve(&corrupt).origin, RequestOrigin::OperatorRequest);
    }

    #[test]
    fn verb_payload_round_trips_a_batch() {
        let records = vec![
            record(Some("operator_request"), Some("idea"), None),
            record(Some("from_observation"), Some("idea"), Some("ev:1")),
        ];
        let payload = serde_json::to_string(&records).expect("serialize");
        let parsed: Vec<BirthRecord> = serde_json::from_str(&payload).expect("round-trip");
        let results: Vec<Resolution> = parsed.iter().map(resolve).collect();
        assert_eq!(results[0].origin, RequestOrigin::OperatorRequest);
        assert_eq!(results[1].origin, RequestOrigin::AgentDiscovery);
    }
}
