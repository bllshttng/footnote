//! What did earlier fires record, and how does this fire record itself.

use super::*;

pub(super) fn make_fingerprint(
    head_sha: &str,
    pr_state: &str,
    ci_conclusion: &str,
    latest_ts: &str,
) -> String {
    // An absent latest-review time renders "none", the pre-read's form; two shapes reset the streak.
    let latest_ts = if latest_ts.is_empty() {
        "none"
    } else {
        latest_ts
    };
    format!("{head_sha}|{pr_state}|{ci_conclusion}|{latest_ts}")
}

/// Default debounce window: an unchanged fingerprint seen again inside this many
/// seconds is the SAME observation, not a new one. The streak counts independent
/// observations of an unchanged world, not stop-hook fires -- a session taking
/// short turns used to burn a 5-fire backstop in 109 seconds while its CI run
/// still had 7 minutes to go, which no external wait can outrun. The effective
/// floor becomes `(backstop_n - 1) * gap`: 10 minutes unattended, 20 attended.
/// Override with `FNO_LOOPCHECK_MIN_FIRE_GAP_SECS` (0 restores fire counting).
pub(super) const MIN_FIRE_GAP_SECS: i64 = 300;

/// Resolve the debounce window from the env seam, falling back to the default.
/// Mirrors the `FNO_LOOPCHECK_GH_BIN` / `_NO_NOTIFY` / `_NO_COMMENT` seams.
pub(super) fn min_fire_gap_secs() -> i64 {
    std::env::var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(MIN_FIRE_GAP_SECS)
}

/// Count prior loop_check events for this session_id in the project events file.
/// Returns (total_fires, consecutive_unchanged_count, last_fingerprint_in_log,
/// streak_window_secs).
///
/// `current_fp` is the fingerprint computed this fire (used for streak matching).
/// `last_fp` is the most recent fingerprint recorded in the events log for this
/// session -- used for carry-forward when the gh pre-read fails this fire.
/// `streak_window_secs` is the span from the oldest COUNTED fire to `now`; it is
/// what makes a streak count falsifiable from the events log.
///
/// The streak is debounced by `min_gap_secs`: walking backwards from `now`, a
/// matching fire closer than the gap to the last counted one is skipped
/// TRANSPARENTLY and does not advance the cursor, so a burst collapses to a
/// single observation. The asymmetry is deliberate and load-bearing: a CHANGED
/// fingerprint breaks the streak at any spacing, because real progress is real
/// progress at any speed -- only the *absence* of change needs time to be
/// credible.
pub(super) fn read_prior_fires(
    events_path: &Path,
    session_id: &str,
    current_fp: Option<&str>,
    now: DateTime<Utc>,
    min_gap_secs: i64,
) -> (u64, u64, Option<String>, i64) {
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return (0, 0, None, 0),
    };

    let mut total: u64 = 0;

    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        total += 1;
    }

    // Calculate consecutive streak from the end (how many recent fires share current_fp)
    // and capture the most recent fp recorded. `next_ts` is the cursor: it starts
    // at `now` and only moves to a fire that was COUNTED, which is what collapses
    // a rapid burst into one observation.
    let mut consecutive: u64 = 0;
    let mut last_fp: Option<String> = None;
    let mut next_ts = now;
    let mut oldest_counted_ts: Option<DateTime<Utc>> = None;
    for line in content.lines().rev() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        // US4: gh-errored fires are TRANSPARENT to the streak - they neither
        // advance nor reset the consecutive count (their recorded fp is just
        // a carry-forward, not an observation). After an outage clears, the
        // streak resumes from its pre-outage value (AC4-FR).
        if val
            .pointer("/data/fp_read_failed")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            continue;
        }
        let fp = val
            .pointer("/data/fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Capture the most recent fp (first match in reverse order)
        if last_fp.is_none() && !fp.is_empty() {
            last_fp = Some(fp.to_string());
        }
        // With no explicit reference, the streak counts against the NEWEST
        // recorded fingerprint: the journal IS the observation history when
        // this fire reads no PR state .
        let reference = match current_fp {
            Some(fp) => fp,
            None => last_fp.as_deref().unwrap_or(""),
        };
        // A CHANGED fingerprint breaks the streak at ANY spacing - progress is
        // never debounced. This check precedes the gap check on purpose.
        if fp != reference {
            break;
        }
        // Debounce. A fire we cannot place in time is skipped transparently
        // rather than counted: giving up on a parse error must fail AWAY from
        // an irreversible NoProgress, matching classify_bot_nudge's precedent.
        let Some(ts) = val
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        let gap = (next_ts - ts).num_seconds();
        // gap < 0 means clock skew (a fire stamped after `now`); count it rather
        // than invent a debounce from a bad clock - status quo, no crash.
        if gap < 0 || gap >= min_gap_secs {
            consecutive += 1;
            next_ts = ts;
            oldest_counted_ts = Some(ts);
        }
        // else: same observation seen twice; skip WITHOUT advancing next_ts.
    }

    let streak_window_secs = oldest_counted_ts
        .map(|t| (now - t).num_seconds().max(0))
        .unwrap_or(0);

    (total, consecutive, last_fp, streak_window_secs)
}

/// The newest recorded loop_check row's `pr_state`/`ci` components for this
/// session: the journal's copy of the last observed world, so a fire that
/// reads no PR state can still record comparable row fields .
pub(super) fn read_last_row_fields(events_path: &Path, session_id: &str) -> (String, String) {
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return ("none".to_string(), "none".to_string()),
    };
    for line in content.lines().rev() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        return (
            val.pointer("/data/pr_state")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
                .to_string(),
            val.pointer("/data/ci")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
                .to_string(),
        );
    }
    ("none".to_string(), "none".to_string())
}

/// Envelope struct for target-stream events. Field order ts,type,source,data is
/// preserved because serde_json serializes struct fields in declaration order.
/// Method is named `append_loop_event` (NOT .emit / .emit_fields) so the
/// production-emit scanner test in lib.rs does not capture it and force
/// registration in KNOWN_EVENT_KINDS (which is the Branch B / fno-agents
/// daemon stream, not the target stream that these events belong to).
#[derive(Debug, Serialize)]
pub(super) struct LoopEventEnvelope<'a> {
    pub(super) ts: String,
    #[serde(rename = "type")]
    pub(super) event_type: &'a str,
    pub(super) source: &'static str,
    pub(super) data: serde_json::Value,
}

// pub(crate): the `finalize` verb (step 6, ) reuses this so its
// `session_finalized` events carry the identical RFC3339 timestamp shape.
pub(crate) fn now_rfc3339_utc() -> String {
    // Millisecond precision prevents distinct same-second events from sharing
    // the content-derived id that makes a true byte-identical retry idempotent.
    let now = chrono::Utc::now();
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Append a target-stream event through the shared Branch-A mkdir mutex.
/// Failure is loud on stderr but never fatal to the decision.
pub(super) fn append_loop_event(path: &Path, event_type: &str, data: serde_json::Value) {
    let env = LoopEventEnvelope {
        ts: now_rfc3339_utc(),
        event_type,
        source: "hook",
        data,
    };
    let Ok(event) = serde_json::to_value(&env) else {
        eprintln!("loop-check: failed to serialize event {event_type}");
        return;
    };
    // One store commit is the acknowledgement: the journal lock-timeout and
    // maintenance retry legs retired with the mutex they served.
    if let Err(error) =
        crate::claims::append_event_line(path, &event, std::time::Duration::from_secs(2))
    {
        eprintln!(
            "loop-check: failed to write event {event_type} to {}: {error}",
            path.display()
        );
    }
}

/// Append to both project and global event logs; `finalize` ships its
/// session events through the same writer, so all envelopes stay identical.
pub(crate) fn emit_to_both(
    project_events: &Path,
    global_events: &Path,
    event_type: &str,
    data: serde_json::Value,
) {
    append_loop_event(project_events, event_type, data.clone());
    if project_events != global_events {
        append_loop_event(global_events, event_type, data);
    }
}

pub(crate) fn observe_shadow_transition(
    run_log: &Path,
    session_id: &str,
    event: crate::run_state::RunEvent,
    project_events: &Path,
    global_events: &Path,
) -> bool {
    if !is_full_run_id(session_id) {
        emit_transition_rejection(
            session_id,
            event,
            "invalid_run_id",
            "manifest carries no valid full run id".to_string(),
            None,
            run_log,
            project_events,
            global_events,
        );
        return false;
    }

    let Err(error) = crate::run_state::append_transition(run_log, session_id, event) else {
        return true;
    };
    let (kind, from) = match &error {
        crate::run_state::RunStateError::InvalidTransition(invalid) => (
            "invalid_transition",
            Some(serde_json::to_value(invalid.from).unwrap_or(serde_json::Value::Null)),
        ),
        _ => ("observer_io", None),
    };
    emit_transition_rejection(
        session_id,
        event,
        kind,
        error.to_string(),
        from,
        run_log,
        project_events,
        global_events,
    );
    false
}

pub(super) fn emit_transition_rejection(
    session_id: &str,
    event: crate::run_state::RunEvent,
    kind: &str,
    error: String,
    from: Option<serde_json::Value>,
    run_log: &Path,
    project_events: &Path,
    global_events: &Path,
) {
    let event_name = serde_json::to_value(event)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let mut data = serde_json::json!({
        "session_id": session_id,
        "kind": kind,
        "event": event_name,
        "error": error,
        "run_log": run_log.display().to_string(),
    });
    if let Some(from) = from {
        data["from"] = from;
    }
    emit_to_both(project_events, global_events, "transition_rejected", data);
}

pub(crate) fn is_full_run_id(value: &str) -> bool {
    static SESSION_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    SESSION_ID
        .get_or_init(|| {
            regex::Regex::new(
                r"^(?:\d{8}T\d{6}Z-[a-z]{0,2}\d+-[0-9a-f]{6}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$",
            )
            .expect("full run id regex is valid")
        })
        .is_match(value)
}
