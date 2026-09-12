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
/// (x-c672, AC7). Nothing decides on this word anymore - retirement reads the
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
    // The probe's own absence splits by the batch outcome (x-6d16): a handle
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
    if let Some(session_id) = entry.harness_session_id.as_deref() {
        return session_id.to_string();
    }
    if !entry.short_id.is_empty() {
        entry.short_id.clone()
    } else {
        entry.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The SAME probe fixtures the parent's tests read: these assertions moved
    // here to sit beside the functions they test, and a second copy of the
    // fixture is how two case tables start disagreeing.
    use crate::daemon::tests::{probe, probe_with_age, probe_with_verdict};
    use crate::truth_probe::BatchOutcome::{self, Measured};
    use serde_json::json;

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
            "a probe that answered nothing reads unknown, never orphaned (x-c672)"
        );
    }

    #[test]
    fn no_probe_at_all_reads_unknown_even_for_a_live_row() {
        // A live pid is a fact about the PROCESS, not about served activity,
        // so it is not an input to the STATUS word (x-c672): the Python list
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

    /// x-6d16's positive control, since the live fleet-wide timeout does not
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
    // The provider-refusal arm (x-e594). One case table, copied verbatim from
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
}
