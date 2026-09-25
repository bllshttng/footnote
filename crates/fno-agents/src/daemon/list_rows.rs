//! The `agent.list` entry point and its attention ordering, extracted from
//! the parent under the file-budget gate: the list projection's change
//! (the `substrate` key) moved with the code it touched.

use super::*;

pub(super) fn handle_list(ctx: &Ctx, req: &Request) -> Response {
    handle_list_with_truth(
        ctx,
        req,
        crate::truth_probe::family1_truth_probe_many_measured,
    )
}

/// The reaped and retired sessions `--all` shows: every receipt in the
/// reap-receipt store, newest first, with the newest recorded cause from the
/// event store joined by session id. A reaped row leaves the registry, so a
/// wrongful reap is invisible without this lane; the receipt is the record
/// that the row existed, and the event names WHY it left. A receipt the
/// event store cannot explain still shows, with the cause honestly absent.
pub(super) fn retired_rows(home: &AgentsHome) -> Vec<Value> {
    let mut out = Vec::new();
    let dir = home.root().join("reap-receipts");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut receipts: Vec<(std::path::PathBuf, crate::receipt::ReapReceipt)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| crate::receipt::read_reap_receipt(&p).ok().map(|r| (p, r)))
        .collect();
    receipts.sort_by(|a, b| b.1.reaped_at.cmp(&a.1.reaped_at));
    let causes = newest_reap_causes(home);
    for (path, receipt) in receipts {
        let cause = causes.get(&receipt.harness_session_id);
        let ledger_node = receipt
            .ledger
            .as_ref()
            .and_then(|l| l.get("graph_node_id").or_else(|| l.get("node")))
            .and_then(Value::as_str)
            .map(String::from);
        out.push(json!({
            "name": receipt.row_name,
            "harness": receipt.harness,
            "session_id": receipt.harness_session_id,
            "short_id": receipt.short_id,
            "cwd": receipt.cwd,
            "state": "reaped",
            "reaped_at": receipt.reaped_at,
            "cause": cause.map(|c| c.0).unwrap_or("not recorded"),
            "cause_at": cause.map(|c| c.1).unwrap_or("not recorded"),
            "basis": cause.map(|c| c.2).unwrap_or("not recorded"),
            "node": ledger_node,
            "resume": receipt.resume,
            "receipt": path.display().to_string(),
        }));
    }
    out
}

/// The newest reaped/removed/vacated event per session id: `(type, ts,
/// basis)`. Commit order means the last line for a sid is the newest; a
/// later removal over an earlier one is the reading that names why the row
/// is gone NOW.
fn newest_reap_causes(
    home: &AgentsHome,
) -> std::collections::HashMap<String, (String, String, String)> {
    let mut out: std::collections::HashMap<String, (String, String, String)> =
        std::collections::HashMap::new();
    let raw = match crate::event_store::journal_text_checked(
        &home.events_jsonl(),
        &crate::event_store::EventQuery::of_types(&[
            "agent_row_reaped",
            "registry_row_removed",
            "agent_crown_vacated",
        ]),
    ) {
        Ok(raw) => raw,
        Err(_) => return out,
    };
    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = event.get("ts").and_then(Value::as_str).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        let sid = data
            .get("harness_session_id")
            .or_else(|| data.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if sid.is_empty() {
            continue;
        }
        let basis = data.get("basis").and_then(Value::as_str).unwrap_or("");
        out.insert(
            sid.to_string(),
            (kind.to_string(), ts.to_string(), basis.to_string()),
        );
    }
    out
}

/// One row's list-lane attention key: evidence tier, then longest-silent
/// first, then name so consecutive lists never shuffle equal rows. Only
/// fields that carry their evidence with them (`basis`,
/// `last_activity_age_s`) - never `status`, never a bare verdict. A row with
/// no probe answer (all three null) lands in the neutral tier with age 0:
/// absence of a reading is not urgency.
/// `to_bits` is order-preserving for non-negative f64 (and an age is a
/// duration, always non-negative), which is what lets a float age ride an
/// `Ord` tuple key.
pub(super) fn attention_sort_key(row: &Value) -> (u8, std::cmp::Reverse<u64>, String) {
    let basis = row.get("basis").and_then(|v| v.as_str());
    let age = row
        .get("last_activity_age_s")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let tier = if matches!(basis, Some("process-gone") | Some("pane-gone"))
        || row.get("reachability").and_then(|v| v.as_str()) == Some("unreachable")
    {
        5
    } else if basis == Some("transcript") && age >= STALE_ATTENTION_S {
        0
    } else if basis == Some("silent") {
        1
    } else if basis == Some("no-evidence") {
        2
    } else {
        4
    };
    (
        tier,
        std::cmp::Reverse(age.to_bits()),
        row.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

/// The STATUS word `list` renders: SERVED ACTIVITY, never a `live` token
/// (x-aaaa, AC7). Nothing decides on this word anymore - retirement reads the
/// reverse join, the lanes read their own probes - so the column answers the
/// operator's actual question, what is this session doing: `writing` (the
/// transcript moved inside `STALE_ATTENTION_S`), `quiet` (older), `parked`
/// (the tail closed a promise). A row whose last assistant turn is a provider
/// refusal reads `refused`: that error record is the newest transcript entry,
/// so without this arm a corpse reads `writing`. A positively falsified row
/// reads `orphaned`, and a probe that did not answer reads `unknown`. A confirmed-live pid does
/// NOT lift an unanswered age to `quiet`: the word is activity, and a process
/// being up says nothing about when it last wrote - the same row must render
/// the same word through the Python list lane, which has no pid census.
pub(super) fn rendered_status_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
) -> &'static str {
    if probe.and_then(|p| p.reachability.as_deref()) == Some("unreachable") {
        return "orphaned";
    }
    if probe.is_some_and(|p| p.provider_refusal.is_some()) {
        return "refused";
    }
    match probe.map(|p| p.state.as_str()) {
        Some("done") => "parked",
        Some(_) => match probe.and_then(|p| p.last_activity_age_s) {
            Some(age) if age < STALE_ATTENTION_S => "writing",
            Some(_) => "quiet",
            None => "unknown",
        },
        None => "unknown",
    }
}

/// True for a Claude model id or tier alias. Mirrors
/// `fno.agents.model_routing.is_anthropic_model` (cli/src/fno/agents/model_routing.py) --
/// duplicated rather than shelled out to because the daemon already pays one
/// probe per row and a second process spawn per row would multiply that cost
/// for a four-branch string check.
fn is_anthropic_model(model: &str) -> bool {
    let name = model.trim().to_ascii_lowercase();
    name.starts_with("claude-") || matches!(name.as_str(), "opus" | "sonnet" | "haiku" | "fable")
}

/// Structural refusal predicate (Locked Decision 3). Mirrors
/// `fno.agents.reachability._is_refused` -- never reads the transcript's
/// prose, so a reworded refusal message cannot break it. Fails OPEN: a
/// recorded `route_settings_path` records the INTENDED route, so a
/// foreign-routed worker answering as a foreign model is healthy, not refused.
fn is_refused(observed_model: &Value, harness: &str, route_settings_path: Option<&str>) -> bool {
    if harness != "claude" {
        return false;
    }
    if route_settings_path.is_some() {
        return false;
    }
    if observed_model.get("kind").and_then(Value::as_str) != Some("observed") {
        return false;
    }
    match observed_model.get("model").and_then(Value::as_str) {
        Some(model) => !is_anthropic_model(model),
        None => false,
    }
}

/// Map a truth probe onto the progress axis `list` renders, mirroring Python's
/// `classify_progress` (`fno/agents/reachability.py`). Reads the SAME probe
/// `rendered_status_from_truth` reads, plus `harness` and
/// `route_settings_path` off the registry entry -- no second probe is paid.
///
/// Precedence matches the Python classifier exactly: a falsified/unresolved
/// row first (`reachability` absent or `unreachable` -- AC12-FR, the
/// compatibility-fallback case included, since an unmeasured row has no
/// progress state to report either), then the refusal predicate, then the
/// truth-state arms plus the measured transcript age. A written `working`
/// state is not progress evidence when its transcript stopped advancing.
pub(crate) fn progress_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
    batch: crate::truth_probe::BatchOutcome,
    harness: &str,
    route_settings_path: Option<&str>,
) -> (&'static str, &'static str) {
    // The probe's own absence splits by the batch outcome: a handle
    // missing from a batch that ran clean is the measured `no-evidence`
    // verdict; a handle missing because the batch itself timed out is
    // `unmeasured`, a different fact the row may not publish as a verdict. A
    // probe that ANSWERED renders exactly as before whatever the batch did -
    // this handle was measured.
    match probe {
        None => {
            return match batch {
                crate::truth_probe::BatchOutcome::Measured => ("unknown", "no-evidence"),
                crate::truth_probe::BatchOutcome::NotMeasured => ("unknown", "unmeasured"),
            }
        }
        Some(p) => match p.reachability.as_deref() {
            Some("unreachable") | None => return ("unknown", "no-evidence"),
            _ => {}
        },
    }
    let observed_model = probe.map(|p| &p.observed_model);
    if observed_model.is_some_and(|om| is_refused(om, harness, route_settings_path)) {
        return ("refused", "model-refused");
    }
    // Below the structural arm, mirroring Python: `model-refused` is read off
    // observed-model evidence, this one off the transcript's own refusal text.
    if probe.is_some_and(|p| p.provider_refusal.is_some()) {
        return ("refused", "provider-refused");
    }
    match probe.map(|p| p.state.as_str()) {
        Some("working" | "watching") => match probe.and_then(|p| p.last_activity_age_s) {
            None => ("unknown", "no-evidence"),
            Some(age) if age >= STALE_ATTENTION_S => ("unknown", "silent"),
            Some(_) => ("advancing", "transcript-turn"),
        },
        Some("your-move") => ("awaiting-operator", "operator-turn"),
        Some("done") => ("parked", "promise"),
        Some("stalled") => ("unknown", "silent"),
        _ => ("unknown", "no-evidence"),
    }
}

pub(crate) fn registry_truth_handle(entry: &RegistryEntry) -> String {
    if let Some(session_id) = entry
        .harness_session_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        return session_id.to_string();
    }
    if !entry.short_id.is_empty() {
        entry.short_id.clone()
    } else {
        entry.name.clone()
    }
}

/// The `basis` leg beside `reachability`, worded by the batch
/// outcome when the probe is absent: a handle the batch never measured reads
/// `unmeasured`, a handle a clean batch resolved nothing for keeps the null
/// an absent reading has always rendered (the `no-evidence` verdict lives on
/// `progress_basis`, where [`progress_from_truth`] words it the same way). A
/// probe that answered carries its own basis.
pub(super) fn basis_word_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
    batch: crate::truth_probe::BatchOutcome,
) -> Value {
    match probe.and_then(|t| t.basis.clone()) {
        Some(basis) => json!(basis),
        None if probe.is_none() && batch == crate::truth_probe::BatchOutcome::NotMeasured => {
            json!("unmeasured")
        }
        None => Value::Null,
    }
}

/// The instrument the activity age came from, lifted off the same probe:
/// `last-entry` | `mtime` | `opencode-db` when the probe answered, the
/// resolver's reason word (`not-found` | `no-records` | `resolver-error`)
/// when it could not resolve the handle, and `unmeasured` when the batch
/// never ran for this page. The three unknown-reason words are the
/// difference between "this worker has no transcript" and "the resolver
/// crashed"; both rendered as the same blank before, and a null
/// never stands alone.
pub(super) fn activity_basis_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
    batch: crate::truth_probe::BatchOutcome,
) -> Value {
    match probe.and_then(|t| t.last_activity_basis.clone()) {
        Some(basis) => json!(basis),
        None if probe.is_none() && batch == crate::truth_probe::BatchOutcome::NotMeasured => {
            json!("unmeasured")
        }
        None => Value::Null,
    }
}

/// Parse one row timestamp into `(value, basis)`-compatible `Option`s for the
/// contradiction rules: absent and unreadable both read `None`.
fn row_timestamp(value: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    let value = value?;
    if let Some(raw) = value.as_str() {
        return chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|parsed| parsed.with_timezone(&chrono::Utc));
    }
    let micros = value.as_u64()?;
    if micros <= 1_000_000_000_000 {
        return None;
    }
    chrono::DateTime::from_timestamp_micros(micros as i64)
}

/// Refuse a row-level verdict when the same emitted row carries fresher
/// evidence against it. This is deliberately pure and shared by the fixture
/// test with Python; the caller supplies all fields before the row is written.
/// `now` is injected so the fixture's fixed clock and production's wall clock
/// assert the same rules.
pub(super) fn apply_row_contradiction(
    row: &mut Map<String, Value>,
    exited_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) {
    // The falsifier as it ARRIVED, snapshotted before any rule below rewrites
    // `basis`. Python's `_supervisor_contradicted` reads the input mapping, so
    // reading the mutated map here would name a different falsifier than the
    // twin for the same row, and the shared fixture has no case where two
    // rules fire together to catch it.
    let incoming_basis = row
        .get("basis")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // `status` for the same reason, and the reason generalises: Python reads
    // the input `row` and writes a SEPARATE `projected` dict, so every rule
    // there sees the original. This twin mutates `row` in place, so any rule
    // reading `status` after an earlier one rewrote it diverges from Python
    // for that row. Today the two rules are mutually exclusive - `terminal`
    // is `orphaned`/`exited` and this one needs `spawning` - so nothing
    // changes; snapshot anyway, because relying on that exclusion is a rule
    // no test states and the next rule added here will not know it.
    let incoming_status = row
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let event_at = row_timestamp(row.get("last_event_at"));
    let reconciled_at = row_timestamp(row.get("last_reconciled_at"));
    let terminal = matches!(
        row.get("status").and_then(Value::as_str),
        Some("orphaned" | "exited")
    );
    // An exit-recorded verdict dates from the exit stamp. The reconcile stamp
    // is rewritten every sweep tick, so a later event could never beat it.
    let verdict_at = if incoming_basis == "exit-recorded" {
        exited_at
            .and_then(|s| row_timestamp(Some(&json!(s))))
            .or(reconciled_at)
    } else {
        reconciled_at
    };
    if terminal && event_at.is_some() && verdict_at.is_some() && event_at > verdict_at {
        row.insert("status".into(), json!("unknown"));
        row.insert("basis".into(), json!("stale-verdict-fresher-event"));
    }

    let message_at = row_timestamp(row.get("last_message_at"));
    let message_is_too_new = match (message_at, event_at) {
        (Some(message), Some(event)) => message - event > chrono::Duration::seconds(2),
        _ => false,
    };
    if message_is_too_new {
        row.insert("last_message_at".into(), Value::Null);
        row.insert(
            "last_message_at_basis".into(),
            json!("refused-newer-than-transcript"),
        );
    }

    // A stored `spawning` token a live pid has outlived: the token
    // stopped being a measurement. Fires only on POSITIVE liveness (the
    // caller measured a live pid and injected `pid_alive: true`); unknown
    // keeps the token, and a missing `created_at` is absent age evidence,
    // not staleness. Mirrors `_spawning_outlived_by_a_live_pid` in Python;
    // rows read `spawning` for 3-16 hours while alive.
    if incoming_status == "spawning"
        && row.get("pid_alive") == Some(&Value::Bool(true))
        // `> Duration::seconds(600)`, not `num_seconds() > 600`: num_seconds
        // truncates, so a 600.5s-old row read `spawning` here and `live` in
        // Python, whose timedelta compare keeps the fraction.
        && row_timestamp(row.get("created_at"))
            .is_some_and(|created_at| now - created_at > chrono::Duration::seconds(600))
    {
        row.insert("status".into(), json!("quiet"));
        row.insert("basis".into(), json!("stale-spawning-live-pid"));
    }
    row.remove("pid_alive");

    // Both keys ALWAYS ride the row, as `reachability`/`basis` and
    // `progress`/`progress_basis` already do on this same row. A conditional
    // key cannot be told apart from a producer that forgot to set one, and the
    // list-row contract in schemas/agents-list-row.json is an exact key set.
    let (origin, origin_basis) = liveness_origin(row);
    row.insert("liveness_origin".into(), origin);
    row.insert(
        "liveness_origin_basis".into(),
        origin_basis.map_or(Value::Null, |basis| json!(basis)),
    );
    // A superseded supervisor claim beside the falsifier
    // that beat it: `superseded_live_status` is a caller-injected input (like
    // `pid`), popped here; only the basis key survives. PRESENCE is the
    // caller's assertion that a supersession happened; which words claim
    // nothing lives in the gate that stamps this input (read.py's {idle,
    // done} admission set) - a second word list here once denied a
    // supersession the gate had stamped. Mirrors `_supervisor_contradicted`
    // in Python.
    let superseded = row
        .get("superseded_live_status")
        .and_then(Value::as_str)
        .is_some_and(|word| !word.is_empty());
    let contradicted = superseded
        && row.get("reachability").and_then(Value::as_str) == Some("unreachable")
        && !incoming_basis.is_empty();
    row.insert(
        "live_status_basis".into(),
        if contradicted {
            json!(format!("contradicted-by-{incoming_basis}"))
        } else {
            // This projection never runs the claude live-status
            // probe, so null here would read as "measured, nothing found".
            // The lane that did not ask says so: a blank is the
            // one thing this pair may not be); the Python list surface, which
            // does run it, keeps its own words.
            json!("not-probed")
        },
    );
    row.remove("superseded_live_status");
}

/// Parse one row timestamp into `(value, basis)`, separating absent from
/// unreadable. Mirrors `_read_field` in cli/src/fno/agents/row_contradiction.py.
///
/// Folding the two together is what let `liveness_origin: null` mean five
/// different things at once, so a reader holding one null could not tell
/// "nothing was recorded" from "something this parser cannot read".
fn row_field_with_basis(
    row: &Map<String, Value>,
    key: &str,
    label: &str,
) -> (Option<chrono::DateTime<chrono::Utc>>, Option<String>) {
    match row.get(key) {
        None | Some(Value::Null) => (None, Some(format!("{label}-absent"))),
        raw => match row_timestamp(raw) {
            Some(parsed) => (Some(parsed), None),
            None => (None, Some(format!("{label}-unreadable"))),
        },
    }
}

/// Return `(liveness_origin, basis)` for one row. Mirrors `_liveness_origin`
/// in cli/src/fno/agents/row_contradiction.py, and the shared fixture at
/// schemas/agents-row-contradiction.json drives both.
///
/// THE PID GATE COMES FIRST. This producer already checked it and the Python
/// one did not, so a pidless row read `survivor` there and null here: one
/// field, two reachable implementations, one guard. A non-null origin carries
/// no basis, because the value is its own evidence.
fn liveness_origin(row: &Map<String, Value>) -> (Value, Option<String>) {
    if !row.get("pid").is_some_and(|value| !value.is_null()) {
        return (Value::Null, Some("pid-absent".to_string()));
    }
    let (created_at, basis) = row_field_with_basis(row, "created_at", "created-at");
    let Some(created_at) = created_at else {
        return (Value::Null, basis);
    };
    let (pid_started_at, basis) = row_field_with_basis(row, "pid_start_time", "pid-start");
    let Some(pid_started_at) = pid_started_at else {
        return (Value::Null, basis);
    };
    if (pid_started_at - created_at).num_seconds() > 600 {
        (json!("resumed"), None)
    } else {
        (json!("survivor"), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The SAME probe fixtures the parent's tests read: these assertions moved
    // here to sit beside the functions they test, and a second copy of the
    // fixture is how two case tables start disagreeing.
    use crate::daemon::tests::{
        probe, probe_with_age, probe_with_verdict, seed_stream_row, short_home, test_ctx,
    };
    use crate::truth_probe::BatchOutcome::{self, Measured};
    use serde_json::json;

    fn registry_entry(name: &str, session_id: Option<&str>, short_id: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            harness_session_id: session_id.map(str::to_string),
            short_id: short_id.to_string(),
            ..RegistryEntry::default()
        }
    }

    #[test]
    fn empty_session_id_falls_back_to_short_id_then_name() {
        assert_eq!(
            registry_truth_handle(&registry_entry("worker", Some(""), "abc12345")),
            "abc12345"
        );
        assert_eq!(
            registry_truth_handle(&registry_entry("worker", Some(""), "")),
            "worker"
        );
    }

    /// The verdict OUTRANKS the transcript state, which is the whole point of
    /// putting it on the wire: a session whose process died forty minutes ago
    /// still reads `working` from its transcript, and mapping that state is how
    /// `list` reported a dead worker live. Same input state, opposite render.
    #[test]
    fn the_reachability_verdict_outranks_the_transcript_state() {
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("working", "unreachable").as_ref()),
            "orphaned",
            "a falsified row must not render writing merely because its transcript is recent"
        );
        // A verdict that did not resolve falls through to the ACTIVITY read:
        // a 12s-old transcript is writing whatever the state word says.
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("stalled", "unknown").as_ref()),
            "writing"
        );
        assert_eq!(
            rendered_status_from_truth(probe("stalled").as_ref()),
            "unknown",
            "a probe that answered nothing reads unknown, never orphaned "
        );
    }

    #[test]
    fn no_probe_at_all_reads_unknown_even_for_a_live_row() {
        // A live pid is a fact about the PROCESS, not about served activity,
        // so it is not an input to the STATUS word: the Python list
        // lane has no pid census, and an unanswered activity age must read
        // the same word on both lanes.
        assert_eq!(
            rendered_status_from_truth(None),
            "unknown",
            "no probe at all reads unknown, never quiet"
        );
    }

    /// A verdict-carrying probe with an explicit `observed_model`, for the
    /// progress-axis tests below (`probe_with_verdict` above always carries
    /// `Value::Null`, which is `no-transcript` and can never refuse).
    fn probe_observed(
        state: &str,
        reachability: &str,
        observed_model: Value,
    ) -> Option<crate::truth_probe::TruthProbe> {
        Some(crate::truth_probe::TruthProbe {
            state: state.into(),
            reachability: Some(reachability.into()),
            basis: Some("transcript".into()),
            last_activity_age_s: Some(12.0),
            last_activity_basis: None,
            last_event_at: None,
            last_message: None,
            observed_model,
            provider_refusal: None,
            harness_title: None,
        })
    }

    #[test]
    fn progress_ac1_ac2_done_is_parked_working_is_advancing() {
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("done", "reachable").as_ref(),
                Measured,
                "claude",
                None
            ),
            ("parked", "promise")
        );
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("working", "reachable").as_ref(),
                Measured,
                "claude",
                None
            ),
            ("advancing", "transcript-turn")
        );
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("your-move", "reachable").as_ref(),
                Measured,
                "claude",
                None
            ),
            ("awaiting-operator", "operator-turn")
        );
    }

    #[test]
    fn progress_ac3_ac4_refusal_outranks_working_but_never_a_routed_worker() {
        let refused_model = json!({"kind": "observed", "model": "glm-5.2[1m]"});
        assert_eq!(
            progress_from_truth(
                probe_observed("working", "reachable", refused_model.clone()).as_ref(),
                Measured,
                "claude",
                None
            ),
            ("refused", "model-refused"),
            "the refusal must outrank the active working truth state"
        );
        assert_eq!(
            progress_from_truth(
                probe_observed("working", "reachable", refused_model).as_ref(),
                Measured,
                "claude",
                Some("/x/route-settings/ab12.json")
            ),
            ("advancing", "transcript-turn"),
            "a deliberately routed worker must never be condemned"
        );
    }

    #[test]
    fn progress_ac5_unmeasured_observed_model_kinds_never_refuse() {
        for kind in [
            "no-transcript",
            "not-file-backed",
            "no-model-yet",
            "unreadable",
        ] {
            let (verdict, _) = progress_from_truth(
                probe_observed("working", "reachable", json!({"kind": kind})).as_ref(),
                Measured,
                "claude",
                None,
            );
            assert_ne!(verdict, "refused", "kind={kind} must never refuse");
        }
    }

    #[test]
    fn progress_ac6_stalled_is_unknown_silent_never_parked() {
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("stalled", "reachable").as_ref(),
                Measured,
                "claude",
                None
            ),
            ("unknown", "silent")
        );
    }

    #[test]
    fn progress_deliberately_wedged_open_turn_is_quiet_but_not_advancing() {
        let probe = probe_with_age("working", "reachable", Some(STALE_ATTENTION_S + 1.0));
        assert_eq!(
            rendered_status_from_truth(probe.as_ref()),
            "quiet",
            "the process and reachability axes still say present; the transcript has not moved"
        );
        assert_eq!(
            progress_from_truth(probe.as_ref(), Measured, "claude", None),
            ("unknown", "silent"),
            "an open turn with no transcript advance past the window is not progressing"
        );
    }

    #[test]
    fn progress_unreadable_activity_age_is_unknown_never_advancing() {
        assert_eq!(
            progress_from_truth(
                probe_with_age("working", "reachable", None).as_ref(),
                Measured,
                "claude",
                None,
            ),
            ("unknown", "no-evidence")
        );
    }

    #[test]
    fn progress_ac7_unreachable_is_unknown_no_evidence_regardless_of_state() {
        for state in ["working", "done", "your-move", "stalled"] {
            assert_eq!(
                progress_from_truth(
                    probe_with_verdict(state, "unreachable").as_ref(),
                    Measured,
                    "claude",
                    None
                ),
                ("unknown", "no-evidence"),
                "state={state}"
            );
        }
    }

    #[test]
    fn progress_ac12_fr_a_probe_with_no_reachability_verdict_is_unknown_no_evidence() {
        // The compatibility fallback (a `fno` too old to emit the verdict):
        // `probe()` carries `reachability: None`. An unmeasured row has no
        // progress state to report, so this must never panic and must never
        // read a stale `state` as an active truth-state arm.
        assert_eq!(
            progress_from_truth(probe("working").as_ref(), Measured, "claude", None),
            ("unknown", "no-evidence")
        );
        assert_eq!(
            progress_from_truth(None, Measured, "claude", None),
            ("unknown", "no-evidence")
        );
    }

    /// The positive control, since the live fleet-wide timeout does not
    /// reproduce on demand: the SAME missing-handle input, worded by the batch
    /// outcome. `unmeasured` says the instrument did not run; `no-evidence`
    /// stays the verdict a clean batch that resolved nothing earns. Assert the
    /// word, never the absence of the other one.
    #[test]
    fn a_missing_handle_splits_by_the_batch_outcome() {
        assert_eq!(
            progress_from_truth(None, BatchOutcome::NotMeasured, "claude", None),
            ("unknown", "unmeasured"),
            "the instrument did not run: never publish that as the no-evidence verdict"
        );
        assert_eq!(
            progress_from_truth(None, BatchOutcome::Measured, "claude", None),
            ("unknown", "no-evidence"),
            "a clean batch that resolved nothing keeps its verdict"
        );
        // A probe that ANSWERED is measured whatever the page-level outcome:
        // the batch timing out must not re-word the handles it did reach.
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("working", "reachable").as_ref(),
                BatchOutcome::NotMeasured,
                "claude",
                None
            ),
            ("advancing", "transcript-turn")
        );
    }

    // ------------------------------------------------------------------
    // The provider-refusal arm. One case table, copied verbatim from
    // the Python lane's `test_reachability.py`, because two lanes rendering
    // the same row differently is the defect this field exists to close.
    // ------------------------------------------------------------------

    fn refused_probe(provider_refusal: Option<&str>) -> Option<crate::truth_probe::TruthProbe> {
        let mut probe = probe_with_verdict("working", "reachable").unwrap();
        probe.last_activity_age_s = Some(469.0);
        probe.provider_refusal = provider_refusal.map(str::to_owned);
        Some(probe)
    }

    /// AC3-HP. The measured specimen: t-1666-guard-retry, dead on a usage-limit
    /// 429 for 469 s, rendered `writing` on both lanes because the error record
    /// is the newest transcript entry.
    #[test]
    fn ac3_a_provider_refused_row_renders_refused_on_both_axes() {
        let probe = refused_probe(Some("provider_4xx_quota"));
        assert_eq!(rendered_status_from_truth(probe.as_ref()), "refused");
        assert_eq!(
            progress_from_truth(probe.as_ref(), Measured, "claude", None),
            ("refused", "provider-refused")
        );
    }

    /// AC4-ERR. Reachability keeps precedence: a falsified row is `orphaned`,
    /// never `refused`. A gone process has no refusal state to report.
    #[test]
    fn ac4_reachability_outranks_the_provider_refusal() {
        let mut probe = refused_probe(Some("provider_4xx_quota")).unwrap();
        probe.reachability = Some("unreachable".into());
        assert_eq!(rendered_status_from_truth(Some(&probe)), "orphaned");
        assert_eq!(
            progress_from_truth(Some(&probe), Measured, "claude", None),
            ("unknown", "no-evidence")
        );
    }

    /// AC4-ERR. No refusal at the same state and age renders exactly as before.
    #[test]
    fn ac4_a_healthy_row_at_the_same_age_still_reads_writing() {
        let probe = refused_probe(None);
        assert_eq!(rendered_status_from_truth(probe.as_ref()), "writing");
        assert_eq!(
            progress_from_truth(probe.as_ref(), Measured, "claude", None),
            ("advancing", "transcript-turn")
        );
    }

    /// The structural `model-refused` arm keeps precedence over this one, the
    /// same order Python's `classify_progress` uses.
    #[test]
    fn the_structural_refusal_arm_still_outranks_the_transcript_text_arm() {
        let mut probe = refused_probe(Some("provider_4xx_quota")).unwrap();
        probe.observed_model = json!({"kind": "observed", "model": "glm-5.2[1m]"});
        assert_eq!(
            progress_from_truth(Some(&probe), Measured, "claude", None),
            ("refused", "model-refused")
        );
    }

    /// The refusal predicate is claude-only by construction: a codex worker
    /// answering as glm is its normal lane, not a refusal (Python twin
    /// `test_non_claude_harness_is_never_refused`).
    #[test]
    fn a_non_claude_harness_is_never_refused() {
        let refused_model = json!({"kind": "observed", "model": "glm-5.2[1m]"});
        for harness in ["codex", "opencode"] {
            assert_eq!(
                progress_from_truth(
                    probe_observed("working", "reachable", refused_model.clone()).as_ref(),
                    Measured,
                    harness,
                    None
                ),
                ("advancing", "transcript-turn"),
                "harness={harness}"
            );
        }
    }

    /// The shared row-contradiction truth table (schemas/agents-row-
    /// contradiction.json), asserted against the Rust projection here; the
    /// Python twin asserts the same cases against `project_row`. Moved beside
    /// `apply_row_contradiction` under the file-budget gate.
    #[test]
    fn row_contradiction_fixture_matches_python_projection() {
        const FIXTURE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/agents-row-contradiction.json"
        ));
        let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
        let now = chrono::DateTime::parse_from_rfc3339(
            fixture["now"].as_str().expect("fixture now is a string"),
        )
        .expect("fixture now is a timestamp")
        .with_timezone(&chrono::Utc);
        for case in fixture["cases"].as_array().expect("cases is an array") {
            let mut row = case["row"].as_object().expect("row is an object").clone();
            let exited_at = row.remove("exited_at");
            apply_row_contradiction(&mut row, exited_at.as_ref().and_then(Value::as_str), now);
            assert!(!row.contains_key("exited_at"), "case={}", case["name"]);
            // Where the two lanes legitimately differ - the Rust
            // projection never runs the claude live-status probe, so its
            // no-contradiction basis reads `not-probed` where Python renders
            // null - the case carries an `expected_rust` override. Absent,
            // `expected` binds both lanes.
            let expected = case
                .get("expected_rust")
                .unwrap_or(&case["expected"])
                .as_object()
                .expect("expected is an object");
            for (key, expected) in expected {
                assert_eq!(row.get(key), Some(expected), "case={}", case["name"]);
            }
        }
    }

    /// End to end through the row projection: when the batch seam
    /// reports the page was never measured, every row words it (`unmeasured`)
    /// instead of rendering the `no-evidence` verdict the old lossy seam
    /// published for a run that did not run. The measured-path twin is the
    /// daemon test `list_row_the_batch_did_not_answer_renders_exactly_as_an_
    /// unanswered_row`: same missing handle, clean batch, verdict unchanged.
    #[test]
    fn list_rows_word_a_page_the_batch_never_measured() {
        let home = short_home("listbatchunmeasured");
        seed_stream_row(&home, "never-probed", "aaaaaaaa");
        let ctx = test_ctx(home.clone(), std::path::PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"all": true}));

        let response = handle_list_with_truth(&ctx, &req, |_handles: &[String]| {
            // The timeout shape: the page comes back with NO answers
            // (truth_probe.rs BoundedRun::NoOutput -> an empty map), not with
            // answers plus a flag.
            (
                std::collections::HashMap::new(),
                crate::truth_probe::BatchOutcome::NotMeasured,
            )
        });

        let rows = response.result().unwrap()["agents"].as_array().unwrap();
        assert!(!rows.is_empty(), "the seeded row must render");
        for row in rows {
            assert_eq!(row["basis"], "unmeasured", "row {}", row["name"]);
            assert_eq!(row["progress_basis"], "unmeasured", "row {}", row["name"]);
            assert_eq!(
                row["last_activity_basis"], "unmeasured",
                "row {}",
                row["name"]
            );
            assert!(row["reachability"].is_null(), "row {}", row["name"]);
            assert!(row["last_activity_age_s"].is_null(), "row {}", row["name"]);
            assert_eq!(
                row["live_status_basis"], "not-probed",
                "row {}",
                row["name"]
            );
            assert_eq!(row["status"], "unknown", "row {}", row["name"]);
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The `--all` provenance lane: a reaped row the registry dropped still
    /// shows, newest first, carrying the resume command and the newest
    /// recorded cause from the event journal. The event store is absent in
    /// this staged world, so the journal text is the source and the join
    /// runs on the same code path.
    #[test]
    fn the_all_lane_joins_the_newest_recorded_cause() {
        let temp = tempfile::tempdir().unwrap();
        let home = AgentsHome::at(temp.path().join("agents"));
        let receipts = home.root().join("reap-receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        std::fs::write(
            receipts.join("claude-11111111-2222-3333-4444-555555555555.json"),
            json!({
                "row_name": "warden",
                "short_id": "11111111",
                "harness": "claude",
                "harness_session_id": "11111111-2222-3333-4444-555555555555",
                "cwd": "/tmp/wt",
                "log_path": null,
                "created_at": "2026-09-24T10:00:00Z",
                "reaped_at": "2026-09-25T16:19:49Z",
                "resume": "claude --bg --resume 11111111-2222-3333-4444-555555555555 --model opus --effort high",
                "ledger": {"graph_node_id": "x-node", "pr_number": 1943}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            home.events_jsonl(),
            concat!(
                r#"{"ts":"2026-09-25T16:19:48Z","type":"registry_row_removed","source":"daemon","data":{"session_id":"11111111-2222-3333-4444-555555555555"}}"#,
                "\n",
                r#"{"ts":"2026-09-25T16:19:49Z","type":"agent_row_reaped","source":"daemon","data":{"harness_session_id":"11111111-2222-3333-4444-555555555555","basis":"every named node done: x-node"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let rows = retired_rows(&home);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row["name"], "warden");
        assert_eq!(row["state"], "reaped");
        assert_eq!(row["node"], "x-node");
        assert_eq!(row["cause"], "agent_row_reaped", "the newest event wins");
        assert_eq!(row["basis"], "every named node done: x-node");
        assert!(
            row["resume"]
                .as_str()
                .unwrap()
                .contains("--model opus --effort high"),
            "the resume command rides the row verbatim"
        );
        // A receipt whose cause never journal'd still shows, honestly absent.
        std::fs::write(
            receipts.join("claude-99999999-2222-3333-4444-555555555555.json"),
            json!({
                "row_name": "ghost",
                "short_id": "99999999",
                "harness": "claude",
                "harness_session_id": "99999999-2222-3333-4444-555555555555",
                "cwd": "/tmp/other",
                "log_path": null,
                "created_at": "2026-09-24T10:00:00Z",
                "reaped_at": "2026-09-25T15:00:00Z",
                "resume": "claude --bg --resume 99999999-2222-3333-4444-555555555555"
            })
            .to_string(),
        )
        .unwrap();
        let rows = retired_rows(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "warden", "newest reaped_at first");
        assert_eq!(rows[1]["cause"], "not recorded");
        std::fs::remove_dir_all(home.root()).ok();
    }
}
