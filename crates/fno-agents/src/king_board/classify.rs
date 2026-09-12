//! Pure classification over the fetched sources: undispatched selection,
//! claimed-node reads, holder activity, driver state.
use super::prs::derived_status;
use super::{s_str, truthy, SourceRead, DEAD_CLAIM_STATES, KING_PRIORITIES, TERMINAL_RUNGS};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Live node claims resolved per board read; the cut is reported (the
/// x-f8e3 reference carried the same cap).
pub(crate) const MAX_CLAIMED_NODE_READS: usize = 20;

/// The activity vocabulary that counts as a staffed lane (reachability
/// `_ACTIVE_STATES`). Copied with a test pinning the Python side, because a
/// Rust module cannot import the Python frozenset; the pin makes the
/// vocabulary fix that adds a fourth word fail loudly here.
pub(crate) const ACTIVE_STATES: [&str; 3] = ["working", "watching", "your-move"];

/// Transcript age past which an active-looking holder reads stalled
/// (session_truth.STALLED_AFTER_S; same pin as ACTIVE_STATES).
pub(crate) const STALLED_AFTER_S: f64 = 2.0 * 3600.0;

// ---------------------------------------------------------------------------
// Claimed nodes + holder activity
// ---------------------------------------------------------------------------

/// The backlog row behind each LIVE node claim (board._read_claimed_nodes):
/// one graph read, exact id match (claims carry real ids; the slug fallback is
/// free and harmless), terminal claims dropped at the source.
pub(crate) fn read_claimed_nodes(
    claims: &SourceRead,
    entries: Option<&[Value]>,
) -> (SourceRead, Vec<String>, Vec<String>) {
    if !claims.is_ok() {
        return (
            SourceRead::err(claims.error.clone().unwrap_or_default()),
            Vec::new(),
            Vec::new(),
        );
    }
    let mut held: Vec<(String, String)> = Vec::new();
    for row in &claims.rows() {
        let Some(key) = s_str(row, "key") else {
            continue;
        };
        let Some(node_id) = key.strip_prefix("node:") else {
            continue;
        };
        let state = s_str(row, "state").unwrap_or("");
        if DEAD_CLAIM_STATES.contains(&state) {
            continue;
        }
        let holder = s_str(row, "holder").unwrap_or("");
        if !holder.is_empty() {
            held.push((node_id.to_string(), holder.to_string()));
        }
    }

    let Some(entries) = entries else {
        return (
            SourceRead::err("backlog get: graph unreadable"),
            Vec::new(),
            Vec::new(),
        );
    };
    let mut warnings: Vec<String> = Vec::new();
    if held.len() > MAX_CLAIMED_NODE_READS {
        warnings.push(format!(
            "stalled_holder: capped at {MAX_CLAIMED_NODE_READS} of {} live claims",
            held.len()
        ));
        held.truncate(MAX_CLAIMED_NODE_READS);
    }
    let mut nodes: Vec<Value> = Vec::new();
    let mut holders: Vec<String> = Vec::new();
    let mut seen_holders: HashSet<String> = HashSet::new();
    for (node_id, holder) in &held {
        let node = entries
            .iter()
            .find(|e| {
                s_str(e, "id")
                    .map(|i| i.eq_ignore_ascii_case(node_id))
                    .unwrap_or(false)
            })
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| s_str(e, "slug").map(|s| s == node_id).unwrap_or(false))
            });
        let Some(node) = node else {
            warnings.push(format!("stalled_holder: {node_id} unreadable: not found"));
            continue;
        };
        // A terminal node's claim is a reaper leak; dropping it here also keeps
        // its holder out of the transcript reads.
        if derived_status(node) == "done"
            || s_str(node, "status")
                .map(|s| TERMINAL_RUNGS.contains(&s))
                .unwrap_or(false)
            || node.get("superseded_by").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        nodes.push(node.clone());
        let priority = s_str(node, "priority").unwrap_or("");
        if KING_PRIORITIES.contains(&priority) && seen_holders.insert(holder.clone()) {
            holders.push(holder.clone());
        }
    }
    (SourceRead::ok(Value::Array(nodes)), holders, warnings)
}

/// The holder verdict, read off the probe the Python predicate already
/// classified (x-dead task 1.3). `holder_is_active` re-derived liveness from
/// `probe.state` + `probe.last_activity_age_s` and collapsed every UNKNOWN
/// into not-active - the exact fold this node measured rendering a dead
/// process live (state word `working`, process gone) and an unmeasurable
/// holder dead (stale claim, undatable transcript). `TruthProbe` carries the
/// shared verdict (`reachability` + `basis`); read it. Tri-state: Unmeasured
/// is its own answer and never collapses into either pole.
pub(crate) enum HolderReading {
    Active,
    Unmeasured,
    Inactive,
}

pub(crate) fn holder_reading(probe: Option<&crate::truth_probe::TruthProbe>) -> HolderReading {
    let Some(probe) = probe else {
        // A holder the probe never answered for is a hole in the evidence,
        // not a verdict (x-db9c).
        return HolderReading::Unmeasured;
    };
    if let Some(reachability) = probe.reachability.as_deref() {
        return match reachability {
            "reachable" => HolderReading::Active,
            "unreachable" => HolderReading::Inactive,
            _ => HolderReading::Unmeasured,
        };
    }
    // A truth build predating the field: fall back to the state+age mapping,
    // which cannot see a dead process. Kept for a mixed-version fleet, not
    // for new code: the probe field, not this arm, is the mechanism.
    if !ACTIVE_STATES.contains(&probe.state.as_str()) {
        return HolderReading::Inactive;
    }
    match probe.last_activity_age_s {
        None => HolderReading::Unmeasured,
        Some(age) if age <= STALLED_AFTER_S => HolderReading::Active,
        Some(_) => HolderReading::Inactive,
    }
}

/// Holder tokens from DEAD-STATED claims, the launch-window leases the board
/// must probe before it calls them dead. `read_claimed_nodes` skips these
/// rows, so without this feed the truth probe never measures them and a
/// stale lease under a writing worker can never classify active - the clock
/// would win by starvation instead of by ordering. Uncapped on purpose: the
/// claims list is already bounded by its own read, and a truncated feed
/// misclassifies the unprobed tail as dead, the exact harm this vocabulary
/// exists to stop.
pub(crate) fn dead_claim_holders(claims: &SourceRead) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !claims.is_ok() {
        return out;
    }
    for row in &claims.rows() {
        let state = s_str(row, "state").unwrap_or("");
        if !DEAD_CLAIM_STATES.contains(&state) {
            continue;
        }
        let holder = s_str(row, "holder").unwrap_or("");
        if holder.is_empty() || out.contains(&holder.to_string()) {
            continue;
        }
        out.push(holder.to_string());
    }
    out
}

/// The probe-map key for a claim: the holder string minus its harness prefix
/// (`spawn-handover:t-w` -> `t-w`), or the whole string when it has none.
pub(crate) fn holder_token(claim: &Value) -> String {
    let holder = s_str(claim, "holder").unwrap_or("");
    holder
        .split_once(':')
        .map(|(_, t)| t.to_string())
        .unwrap_or_else(|| holder.to_string())
}

/// A claim is dead only when the clock says so AND its holder does not
/// answer. The clock alone is a timer: a 15 minute handover lease expires
/// under a worker that runs for hours, and the holder probe is the honest
/// reading. Every board site that asks "is this lock dead?" asks here.
///
/// A holder the probe never answered for - or one the shared predicate could
/// not classify - reads NOT dead: absence of a measurement is not a
/// measurement of absence, and requeueing on the missing entry is how a live
/// worker loses its lock to a second dispatch. The claims layer's own
/// bounded grace (UNRESOLVED_GRACE_MS) is what eventually frees an
/// unmeasurable holder, not this board's clock.
pub(crate) fn claim_is_dead(
    claim: &Value,
    activity: &HashMap<String, crate::truth_probe::TruthProbe>,
) -> bool {
    if !DEAD_CLAIM_STATES.contains(&s_str(claim, "state").unwrap_or("")) {
        return false;
    }
    let Some(probe) = activity.get(&holder_token(claim)) else {
        return false;
    };
    matches!(holder_reading(Some(probe)), HolderReading::Inactive)
}

/// A node bound to a PR, by `pr_number` or any `additional_prs` entry.
pub(crate) fn node_has_pr(node: &Value) -> bool {
    node.get("pr_number").map(truthy).unwrap_or(false)
        || node
            .get("additional_prs")
            .and_then(Value::as_array)
            .map(|extras| {
                extras
                    .iter()
                    .any(|e| e.is_object() && e.get("number").map(truthy).unwrap_or(false))
            })
            .unwrap_or(false)
}

/// The roster's driver verdict for one node: `Some` when the roster answers,
/// `None` when it has no candidate or every candidate positively died. The
/// join is on the registry row's `node` field alone, so a crown's own row
/// (which names its scope, never the leaf) cannot read as a leaf's driver.
pub(crate) fn roster_verdict(
    node_id: &str,
    drivers: &crate::king_board::SourceRead,
    activity: &HashMap<String, crate::truth_probe::TruthProbe>,
) -> Option<&'static str> {
    if !drivers.is_ok() {
        // Degraded roster coverage never suppresses a row: a board that
        // cannot see the roster cannot say "nobody is driving".
        return Some("unmeasured");
    }
    let Some(rows) = drivers.payload.as_ref().and_then(Value::as_array) else {
        return None;
    };
    let mut saw_candidate = false;
    let mut saw_inactive = false;
    let mut saw_unmeasured = false;
    for row in rows {
        if !row
            .get("node")
            .and_then(Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(node_id))
        {
            continue;
        }
        saw_candidate = true;
        let token = row.get("token").and_then(Value::as_str).unwrap_or("");
        match activity.get(token) {
            Some(probe) => match holder_reading(Some(probe)) {
                HolderReading::Active => return Some("active"),
                HolderReading::Inactive => saw_inactive = true,
                HolderReading::Unmeasured => saw_unmeasured = true,
            },
            None => saw_unmeasured = true,
        }
    }
    // A candidate the probe could not measure stays unmeasured, never absent:
    // a codex transcript outside the claude project store must not read as a
    // dead driver, which is the false "dead" that invites reaping a live
    // worker.
    if saw_candidate && (!saw_inactive || saw_unmeasured) {
        Some("unmeasured")
    } else {
        None
    }
}

/// Who is driving this node: active, stalled, crowned, none, or unmeasured.
/// One answer, three queues: stalled_holder selects stalled, undriven_pr and
/// unheld_progress select none, and an unmeasured holder belongs to no queue
/// row. The ROSTER outranks the claim: a registry row whose `node`
/// field targets this node is the driver, probed for a live transcript, and
/// the claim lockfile below it only corroborates. `crowned` is a live crown
/// driving the epic it reigns over: scope ids reach the build only through a
/// king manifest, and
/// the session holding that manifest is the one building, so a scope hit is a
/// live crown. The epic carries no claim of its own (a crown is not a claim),
/// so without this state the reigning epic reads "none" and no verb can clear
/// the row. A PR bound to the epic keeps it reading none - undriven_pr owns
/// that shape, and a PR needs a driver of its own. In-scope leaves stay
/// claim-driven: a dead worker under a crown is still a dead handoff.
///
/// A CONTAINED node never reads none (x-dead task 1.4b): `contained_in` set
/// means an owner exists by definition and the node never dispatches alone,
/// so `none` - the word that fills unheld_progress, undriven_pr and
/// unreachable_worker - is not an available verdict for it. One check here
/// drops the node from all three queues at once; per-queue guards would be
/// three chances to drift.
///
/// The stalled arm asks for PROGRESS, never for a lease. A holder is stalled
/// exactly when its probe carries no positive transcript evidence; lease
/// presence or absence decides nothing. A fresh `spawn-handover` lease is not
/// health - a deadlocked worker was measured holding an unexpired one - so a
/// lease must never suppress the row, and (structurally: the claim scan strips
/// `expires_at`) it cannot. This ruling is pinned by the three
/// `*_asks_for_progress_*` tests below; x-caf7 is the failure the obvious
/// lease-keyed fix would have silenced.
///
/// The UNMEASURED arm is a measurement that did not happen: a probe the batch
/// never answered (x-db9c), a probe the shared predicate could not classify
/// (x-dead task 1.3), or - for a node with NO claim row - a worked feed that
/// could not answer whether a driverless-looking node has a roster worker
/// (x-dead task 1.4: a live worker with no claim row is not an undriven PR,
/// and PR 1747 was measured wearing exactly that shape). None of these is
/// `none`; `none` requires the worked feed to have answered and found
/// nothing.
pub(crate) fn node_driver<'a>(
    node: &Value,
    claim_by_node: &'a HashMap<String, Value>,
    activity: &'a HashMap<String, crate::truth_probe::TruthProbe>,
    crown_ids: Option<&HashSet<String>>,
    worked: Option<&crate::king_board::SourceRead>,
    drivers: Option<&crate::king_board::SourceRead>,
) -> (&'static str, Option<&'a Value>) {
    let node_id = s_str(node, "id").unwrap_or("");
    let crowned = crown_ids.is_some_and(|ids| ids.contains(node_id))
        && s_str(node, "type") == Some("epic")
        && !node_has_pr(node);
    // Before the claim lookup: a contained node with a stale claim edge is
    // still owned by its container, never queue-fodder.
    let contained = s_str(node, "contained_in").is_some_and(|c| !c.is_empty());
    if contained && crowned {
        return ("crowned", None);
    }
    if contained {
        return ("active", None);
    }
    // Before the claim lookup too: the roster outranks the claim.
    // A claim is a snapshot, a driver is a process - measured 2026-09-04,
    // five free-claim PRs, three of them mid-edit under a live worker. A
    // registry row targeting this node with an advancing transcript is a
    // driver no matter what the lockfile says; a roster that cannot answer
    // never leaves `none` on the table. The claim stays below as
    // corroboration, never the sole signal.
    if let Some(drivers) = drivers {
        if let Some(verdict) = roster_verdict(node_id, drivers, activity) {
            return (verdict, None);
        }
    }
    let claim = claim_by_node.get(node_id);
    let Some(claim) = claim else {
        if crowned {
            return ("crowned", None);
        }
        // No claim row: consult the worked feed before saying none. The feed
        // answering "no worker" is the only path to a positive none; an
        // unreadable feed is unmeasured, never none.
        let Some(read) = worked else {
            return ("unmeasured", None);
        };
        if !read.is_ok() {
            return ("unmeasured", None);
        }
        let driven = read
            .payload
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter()
                    .any(|row| row.get("id").and_then(Value::as_str) == Some(node_id))
            });
        if driven {
            return ("active", None);
        }
        return ("none", None);
    };
    // Before the dead check: an unprobed holder is neither dead nor active,
    // so the dead-state clock must not eat the missing measurement.
    let Some(probe) = activity.get(&holder_token(claim)) else {
        return ("unmeasured", Some(claim));
    };
    if claim_is_dead(claim, activity) {
        if crowned {
            return ("crowned", None);
        }
        return ("none", Some(claim));
    }
    match holder_reading(Some(probe)) {
        HolderReading::Active => ("active", Some(claim)),
        HolderReading::Unmeasured => ("unmeasured", Some(claim)),
        HolderReading::Inactive => ("stalled", Some(claim)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_worked(ids: &[&str]) -> crate::king_board::SourceRead {
        crate::king_board::SourceRead::ok(json!(ids
            .iter()
            .map(|id| json!({"id": id}))
            .collect::<Vec<Value>>()))
    }

    #[test]
    fn holder_reading_maps_the_shared_verdict() {
        // x-dead task 1.3: the board reads the Python predicate's verdict off
        // the wire instead of re-deriving liveness from state+age. Each pole
        // asserts its literal state word: a positive marker.
        let mut probed = crate::truth_probe::TruthProbe {
            state: "working".to_string(),
            provider_refusal: None,
            harness_title: None,
            reachability: Some("reachable".to_string()),
            basis: Some("transcript".to_string()),
            last_activity_age_s: Some(30.0),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        };
        assert!(matches!(
            holder_reading(Some(&probed)),
            HolderReading::Active
        ));
        probed.reachability = Some("unknown".to_string());
        assert!(matches!(
            holder_reading(Some(&probed)),
            HolderReading::Unmeasured
        ));
        probed.reachability = Some("unreachable".to_string());
        assert!(matches!(
            holder_reading(Some(&probed)),
            HolderReading::Inactive
        ));
        // A probe the batch never answered for is unmeasured, not dead.
        assert!(matches!(holder_reading(None), HolderReading::Unmeasured));
        // A truth build predating the field keeps the legacy mapping.
        let legacy = crate::truth_probe::TruthProbe {
            state: "working".to_string(),
            reachability: None,
            ..probed
        };
        assert!(matches!(
            holder_reading(Some(&legacy)),
            HolderReading::Active
        ));
    }

    #[test]
    fn a_live_crown_drives_the_epic_but_not_its_leaves() {
        let epic = json!({"id": "x-epic", "type": "epic"});
        let leaf = json!({"id": "x-leaf", "parent": "x-epic"});
        let claims: HashMap<String, Value> = HashMap::new();
        let activity = HashMap::new();
        let crown: HashSet<String> = ["x-epic", "x-leaf"]
            .map(str::to_string)
            .into_iter()
            .collect();
        assert_eq!(
            node_driver(&epic, &claims, &activity, Some(&crown), None, None).0,
            "crowned"
        );
        // an epic bound to its own PR keeps reading none: undriven_pr owns
        // that shape, and a PR needs a driver of its own (measured 2026-09-06:
        // six graph epics carry a pr_number)
        let epic_pr = json!({"id": "x-epic", "type": "epic", "pr_number": 42});
        assert_eq!(
            node_driver(
                &epic_pr,
                &claims,
                &activity,
                Some(&crown),
                Some(&ok_worked(&[])),
                None
            )
            .0,
            "none"
        );
        // without the crown the same epic is an unheld dead handoff
        assert_eq!(
            node_driver(&epic, &claims, &activity, None, Some(&ok_worked(&[])), None).0,
            "none"
        );
        // an in-scope leaf stays claim-driven
        assert_eq!(
            node_driver(
                &leaf,
                &claims,
                &activity,
                Some(&crown),
                Some(&ok_worked(&[])),
                None
            )
            .0,
            "none"
        );
        // a live claim outranks the crown; the probe ANSWERED for its holder
        // (`unknown`/not-found is the batch's per-handle nothing-shape), so
        // this is the stalled verdict, never unmeasured
        let mut held = claims.clone();
        held.insert(
            "x-epic".to_string(),
            json!({"key": "node:x-epic", "state": "live", "holder": "claude:h"}),
        );
        let mut answered = HashMap::new();
        answered.insert("h".to_string(), probe("unknown", 30.0));
        assert_eq!(
            node_driver(&epic, &held, &answered, Some(&crown), None, None).0,
            "stalled"
        );
    }

    fn probe(state: &str, age_s: f64) -> crate::truth_probe::TruthProbe {
        crate::truth_probe::TruthProbe {
            state: state.to_string(),
            provider_refusal: None,
            harness_title: None,
            reachability: None,
            basis: None,
            last_activity_age_s: Some(age_s),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        }
    }

    #[test]
    fn an_expired_lease_under_a_writing_holder_classifies_active() {
        // AC1-HP: the holder probe outranks the clock. The 2026-09-09 board
        // read five stale handover leases whose holders were writing at that
        // moment; the old ordering returned none before the probe ever ran.
        let node = json!({"id": "x-7471", "priority": "p1"});
        let mut claims = HashMap::new();
        claims.insert(
            "x-7471".to_string(),
            json!({
                "key": "node:x-7471", "state": "stale",
                "holder": "spawn-handover:target-7471-worker",
            }),
        );
        let mut activity = HashMap::new();
        activity.insert("target-7471-worker".to_string(), probe("working", 30.0));
        let (state, claim) = node_driver(&node, &claims, &activity, None, None, None);
        assert_eq!(state, "active");
        assert!(claim.is_some());
    }

    #[test]
    fn an_expired_lease_with_no_live_holder_is_still_none() {
        // AC2-HP: a genuinely abandoned lock keeps reading dead, so requeue
        // and reap keep working. Positive marker: the literal string. The
        // probe ANSWERED here - state `unknown` with reason `not-found` is the
        // batch's per-handle shape for a handle that resolves to nothing.
        let node = json!({"id": "x-gone", "priority": "p1"});
        let mut claims = HashMap::new();
        claims.insert(
            "x-gone".to_string(),
            json!({"key": "node:x-gone", "state": "stale", "holder": "spawn-handover:reaped-worker"}),
        );
        let mut activity = HashMap::new();
        activity.insert(
            "reaped-worker".to_string(),
            probe("unknown", STALLED_AFTER_S + 1.0),
        );
        assert_eq!(
            node_driver(&node, &claims, &activity, None, None, None).0,
            "none"
        );
    }

    #[test]
    fn an_unprobed_dead_state_claim_is_not_dead() {
        // The probe never answered for this holder (batch timed out around
        // it, or it joined after the flight): the clock alone must not kill
        // the claim. Absence of a measurement is not a measurement of absence.
        let claim = json!({
            "key": "node:x-stale",
            "state": "stale",
            "holder": "spawn-handover:t-stale-worker",
        });
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        assert!(!claim_is_dead(&claim, &activity));
    }

    #[test]
    fn a_holder_the_probe_never_answered_reads_unmeasured() {
        // A claim whose holder has NO probe entry reads unmeasured - neither
        // active, stalled, nor none - so no queue renders a verdict about a
        // worker nobody measured.
        let node = json!({"id": "x-silent", "priority": "p1"});
        let mut claims = HashMap::new();
        claims.insert("x-silent".to_string(), handover_claim("x-silent"));
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        let (state, claim) = node_driver(&node, &claims, &activity, None, None, None);
        assert_eq!(state, "unmeasured");
        assert!(claim.is_some());
    }

    #[test]
    fn a_live_state_claim_never_reads_dead_from_the_helper() {
        // The helper is a reorder, not a second clock: a live claim keeps its
        // existing stalled/active classification path untouched.
        let claim = json!({"key": "node:x-live", "state": "live", "holder": "h"});
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        assert!(!claim_is_dead(&claim, &activity));
    }

    #[test]
    fn dead_claim_holders_feed_names_the_clock_dead_but_unprobed() {
        // The probe feed must cover the rows read_claimed_nodes skips, or the
        // holder-first ordering can never see a probe for exactly the claims
        // it exists to answer about.
        let claims = crate::king_board::SourceRead::ok(json!([
            {"key": "node:x-live", "state": "live", "holder": "claude:lives"},
            {"key": "node:x-stale", "state": "stale", "holder": "spawn-handover:worker-a"},
            {"key": "node:x-also", "state": "corrupted", "holder": "spawn-handover:worker-b"},
            {"key": "node:x-bare", "state": "stale", "holder": ""},
            {"key": "node:x-dup", "state": "stale", "holder": "spawn-handover:worker-a"},
        ]));
        let holders = dead_claim_holders(&claims);
        assert_eq!(
            holders,
            vec![
                "spawn-handover:worker-a".to_string(),
                "spawn-handover:worker-b".to_string()
            ]
        );
    }

    #[test]
    fn the_active_vocabulary_matches_the_python_side() {
        // The pin: reachability._ACTIVE_STATES and session_truth.STALLED_AFTER_S
        // are the load-bearing vocabulary; this test fails when Python grows a
        // fourth state or moves the stall threshold, so the copy cannot rot
        // silently.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("cli/src/fno/agents/reachability.py");
        let Ok(text) = std::fs::read_to_string(&src) else {
            eprintln!(
                "reachability.py not found at {}; pin skipped (sdist build)",
                src.display()
            );
            return;
        };
        assert!(
            text.contains(r#"_ACTIVE_STATES = frozenset({"working", "watching", "your-move"})"#)
        );
    }

    // --- x-9958: stalled asks for progress, never for a lease ---------------
    //
    // The plan for x-9958 requires three tests: advancing evidence clears the
    // row, no evidence keeps it, and an UNEXPIRED spawn-handover lease under a
    // silent holder must never suppress it (the x-caf7 deadlocked worker held
    // a fresh lease; keying on the lease would have gone silent about exactly
    // that row). Each asserts the literal state word: a positive marker.

    fn handover_claim(node_id: &str) -> serde_json::Value {
        let mut claim = json!({
            "key": format!("node:{node_id}"),
            "state": "live",
            "holder": format!("spawn-handover:t-{node_id}-worker"),
        });
        // The lease field the claim scan strips before this layer; carried
        // here to prove the classification reads nothing from it.
        claim["expires_at"] = json!("9999-12-31T23:59:59Z");
        claim
    }

    #[test]
    fn an_advancing_holder_is_never_stalled_whatever_the_lease_says() {
        // Test A: a live handover claim whose holder's row shows
        // transcript-turn progress and a recent age. The claim names no agent
        // row of its own - the probe is the row's answer - and the node must
        // read active, not stalled.
        let node = json!({"id": "x-adv", "priority": "p1"});
        let mut claims = HashMap::new();
        claims.insert("x-adv".to_string(), handover_claim("x-adv"));
        let mut activity = HashMap::new();
        activity.insert("t-x-adv-worker".to_string(), probe("working", 30.0));
        assert_eq!(
            node_driver(&node, &claims, &activity, None, None, None).0,
            "active"
        );
    }

    #[test]
    fn a_holder_without_progress_evidence_reads_stalled() {
        // Test B: no agent row, no progress evidence. The queued-spawn window
        // has nothing to probe yet, and the rule stays honest about that:
        // stalled is the answer until transcript evidence exists. The probe
        // ANSWERED here - `unknown`/`not-found` - so this is the answered-no
        // shape, not the unmeasured shape the test below pins.
        let node = json!({"id": "x-quiet", "priority": "p1"});
        let mut claims = HashMap::new();
        claims.insert("x-quiet".to_string(), handover_claim("x-quiet"));
        let mut activity = HashMap::new();
        activity.insert("t-x-quiet-worker".to_string(), probe("unknown", 30.0));
        assert_eq!(
            node_driver(&node, &claims, &activity, None, None, None).0,
            "stalled"
        );
    }

    #[test]
    fn an_unexpired_handover_lease_never_suppresses_a_stalled_row() {
        // Test C, the regression guard: a fresh lease beside NO progress
        // evidence still reads stalled. The lease is not evidence of health;
        // this is the shape the lease-keyed fix would have silenced.
        let node = json!({"id": "x-wedge", "priority": "p1"});
        let mut claims = HashMap::new();
        claims.insert("x-wedge".to_string(), handover_claim("x-wedge"));
        let mut activity = HashMap::new();
        // A row exists and answers, but shows nothing recent: the deadlock
        // shape, not the missing-row shape.
        activity.insert(
            "t-x-wedge-worker".to_string(),
            probe("working", STALLED_AFTER_S + 1.0),
        );
        assert_eq!(
            node_driver(&node, &claims, &activity, None, None, None).0,
            "stalled"
        );
    }

    // --- x-dead: contained nodes and the no-claim arm ----------------------

    #[test]
    fn a_contained_node_never_reads_none() {
        // x-dead task 1.4b (the x-58a5 shape): `contained_in` set means an
        // owner exists by definition and the node never dispatches alone, so
        // `none` - the word that fills the board queues - is not an available
        // verdict for it, whatever its claim edge says.
        let node = json!({"id": "x-58a5", "priority": "p1", "contained_in": "x-b7f8"});
        let claims: HashMap<String, Value> = HashMap::new();
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        let (state, claim) =
            node_driver(&node, &claims, &activity, None, Some(&ok_worked(&[])), None);
        assert_eq!(state, "active");
        assert!(claim.is_none());
    }

    #[test]
    fn a_no_claim_node_reads_what_the_worked_feed_answers() {
        // x-dead task 1.4 (the PR 1747 shape): a live worker with no claim row
        // is not an undriven PR. The worked feed answering is the only path to
        // a positive none; an unreadable feed is unmeasured, never none.
        let node = json!({"id": "x-1747", "priority": "p1"});
        let claims: HashMap<String, Value> = HashMap::new();
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        let (state, claim) = node_driver(
            &node,
            &claims,
            &activity,
            None,
            Some(&ok_worked(&["x-1747"])),
            None,
        );
        assert_eq!(state, "active");
        assert!(claim.is_none());
        // The feed answered and found nothing: this is the one positive none.
        assert_eq!(
            node_driver(&node, &claims, &activity, None, Some(&ok_worked(&[])), None).0,
            "none"
        );
        // The feed could not answer: unmeasured, not none.
        assert_eq!(
            node_driver(
                &node,
                &claims,
                &activity,
                None,
                Some(&crate::king_board::SourceRead::err(
                    "worked: reader panicked"
                )),
                None,
            )
            .0,
            "unmeasured"
        );
    }

    fn drivers_read(rows: Value) -> crate::king_board::SourceRead {
        crate::king_board::SourceRead::ok(rows)
    }

    fn driver_row(node: &str, token: &str) -> Value {
        json!({"name": format!("t-{node}"), "node": node, "token": token})
    }

    #[test]
    fn a_live_roster_driver_with_a_free_claim_is_active() {
        // The measured shape that motivated the roster: claim free, driver
        // live mid-edit. The claim is a snapshot; the driver is a process.
        let node = json!({"id": "x-5baf", "priority": "p1", "pr_number": 1});
        let claims: HashMap<String, Value> = HashMap::new();
        let mut activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        activity.insert("uuid-5baf".to_string(), probe("working", 15.0));
        let drivers = drivers_read(json!([driver_row("x-5baf", "uuid-5baf")]));
        let (state, claim) = node_driver(
            &node,
            &claims,
            &activity,
            None,
            Some(&ok_worked(&[])),
            Some(&drivers),
        );
        assert_eq!(state, "active");
        assert!(claim.is_none());
    }

    #[test]
    fn a_roster_candidate_the_probe_cannot_measure_reads_unmeasured() {
        // The codex-transcript trap: a driver whose transcript lives outside
        // the claude project store is unmeasurable, and unmeasurable is not
        // absent.
        // Unmeasured is its own verdict, so the row is spared either way.
        let node = json!({"id": "x-cdx", "priority": "p1", "pr_number": 2});
        let claims: HashMap<String, Value> = HashMap::new();
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        let drivers = drivers_read(json!([driver_row("x-cdx", "codex-thread")]));
        assert_eq!(
            node_driver(
                &node,
                &claims,
                &activity,
                None,
                Some(&ok_worked(&[])),
                Some(&drivers)
            )
            .0,
            "unmeasured"
        );
    }

    #[test]
    fn a_failed_roster_read_is_unmeasured_never_none() {
        // Measured in the field: a degraded roster read suppressed a real
        // undriven PR. Coverage must not produce a healthy-looking zero.
        let node = json!({"id": "x-a792", "priority": "p1", "pr_number": 3});
        let claims: HashMap<String, Value> = HashMap::new();
        let activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        let drivers = crate::king_board::SourceRead::err("registry unreadable: boom");
        assert_eq!(
            node_driver(
                &node,
                &claims,
                &activity,
                None,
                Some(&ok_worked(&[])),
                Some(&drivers)
            )
            .0,
            "unmeasured"
        );
    }

    #[test]
    fn a_dead_roster_candidate_falls_through_to_the_claim() {
        // Positive death evidence is the one roster answer that steps aside:
        // the claim/worked verdict governs what the roster cannot see.
        let node = json!({"id": "x-dead", "priority": "p1", "pr_number": 4});
        let claims: HashMap<String, Value> = HashMap::new();
        let mut activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        activity.insert(
            "dead-uuid".to_string(),
            probe("working", 3.0 * 3600.0 + 60.0),
        );
        let drivers = drivers_read(json!([driver_row("x-dead", "dead-uuid")]));
        assert_eq!(
            node_driver(
                &node,
                &claims,
                &activity,
                None,
                Some(&ok_worked(&[])),
                Some(&drivers)
            )
            .0,
            "none"
        );
    }

    #[test]
    fn a_crown_scope_row_never_drives_the_scopes_leaf() {
        // The field specimen: a crown's registry row names its scope in the
        // node field, so the node-field join can never suppress a leaf's
        // genuinely driverless PR.
        let leaf = json!({"id": "x-leaf", "priority": "p1", "pr_number": 1545});
        let claims: HashMap<String, Value> = HashMap::new();
        let mut activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        activity.insert("king-uuid".to_string(), probe("working", 5.0));
        let drivers = drivers_read(json!([driver_row("x-scope", "king-uuid")]));
        assert_eq!(
            node_driver(
                &leaf,
                &claims,
                &activity,
                None,
                Some(&ok_worked(&[])),
                Some(&drivers)
            )
            .0,
            "none"
        );
    }

    #[test]
    fn a_live_roster_driver_outranks_a_stale_held_claim() {
        // A live driver outranks a stale HELD claim (a claim is a snapshot);
        // roster first, claim corroborates.
        let node = json!({"id": "x-held", "priority": "p1", "pr_number": 5});
        let mut claims: HashMap<String, Value> = HashMap::new();
        claims.insert(
            "x-held".to_string(),
            json!({"key": "node:x-held", "state": "live", "holder": "claude:stale-h"}),
        );
        let mut activity: HashMap<String, crate::truth_probe::TruthProbe> = HashMap::new();
        activity.insert("new-driver".to_string(), probe("working", 10.0));
        let drivers = drivers_read(json!([driver_row("x-held", "new-driver")]));
        assert_eq!(
            node_driver(
                &node,
                &claims,
                &activity,
                None,
                Some(&ok_worked(&[])),
                Some(&drivers)
            )
            .0,
            "active"
        );
    }
}
