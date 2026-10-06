//! The attended-session guard: a typed user turn inside the
//! window holds a session against every reap and stop. The age instrument
//! can read a live transcript quiet (an account store the probe misses, a
//! resumed session id); the turn record itself is the witness that cannot
//! miss, classified by the same provenance rules `fno-agents intel` folds
//! with. A done ledger never overrides an attended session.

use crate::provenance::{
    classify_turn, is_user_turn, turn_ts_epoch, BusIndex, HarnessKind, Provenance,
};
use crate::state::RegistryEntry;
use serde_json::Value;
use std::io::BufRead;
use std::path::PathBuf;

/// How recent a typed turn must be to hold a session: 15 minutes.
pub(crate) const WINDOW_SECS: i64 = 15 * 60;

/// A person spoke: the witnessed operator turn, the unbound typed turn,
/// and a hand-run slash command (the operator acting) all count; keepalive
/// pings, mail and relay envelopes, and every synthetic harness shape do
/// not.
fn typed(p: Provenance) -> bool {
    matches!(
        p,
        Provenance::Operator
            | Provenance::Unknown
            | Provenance::Harness(HarnessKind::CommandInvocation)
    )
}

/// The bus index both arms classify with, read off the state root the way
/// `liveness_sweep` reads it.
fn bus_for(home: &crate::paths::AgentsHome) -> BusIndex {
    BusIndex::load(&crate::intel::bus_log_path(
        home.root().parent().unwrap_or(home.root()),
    ))
}

/// Age in seconds of the newest typed user turn across `paths`, `None`
/// when no transcript holds one. A row without a parseable timestamp
/// cannot testify; a future stamp reads as now.
pub(crate) fn newest_typed_turn_age_s(
    paths: &[PathBuf],
    bus: &BusIndex,
    session: &str,
    now: i64,
) -> Option<i64> {
    let mut newest: Option<i64> = None;
    for path in paths {
        // Transcripts are append-only JSONL: a file quiet past the window
        // cannot hold an in-window typed turn, so the full read is skipped.
        if let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) {
            let m = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0) as i64;
            if now - m > WINDOW_SECS {
                continue;
            }
        }
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).split(b'\n') {
            let Ok(line) = line else { continue };
            let Ok(obj) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if !is_user_turn(&obj) || !typed(classify_turn(&obj, bus, session)) {
                continue;
            }
            let Some(ts) = turn_ts_epoch(&obj) else {
                continue;
            };
            let age = (now - ts as i64).max(0);
            newest = Some(newest.map_or(age, |n: i64| n.min(age)));
        }
    }
    newest
}

/// The sweep's one-row gate: `Some(hold)` when the row's transcript
/// carries a typed turn inside the window. A row the age seam already
/// reads fresh (`quiet`) skips the read - `kept_active` holds it.
pub(crate) fn sweep_hold(
    id: &str,
    home: &crate::paths::AgentsHome,
    hits: Option<Vec<PathBuf>>,
    sid: &str,
    quiet: bool,
    now: i64,
    hold_age_s: Option<i64>,
    hold_age_basis: &'static str,
) -> Option<crate::gc_sweep::Hold> {
    if quiet {
        return None;
    }
    let hits = hits?;
    let typed_age = newest_typed_turn_age_s(&hits, &bus_for(home), sid, now)?;
    (typed_age <= WINDOW_SECS).then(|| crate::gc_sweep::Hold {
        id: id.to_string(),
        reason: "attended",
        detail: format!("typed user turn {typed_age}s ago"),
        age_s: hold_age_s,
        age_basis: hold_age_basis,
        escalated: false,
    })
}

/// The stop-verb refusal: `Some(why)` when the row's store-resolved
/// transcript carries a typed turn inside the window. The bus join keys on
/// the row's harness session id, the same key `fno-agents intel` joins on.
/// The caller names its own error shape.
pub(crate) fn stop_refusal(home: &crate::paths::AgentsHome, e: &RegistryEntry) -> Option<String> {
    let mut index = crate::gc_inventory::HarnessStoreIndex::default();
    let hits = index.matches(e)?;
    let now = crate::daemon::now_epoch_secs();
    let sid = e.harness_session_id.as_deref().unwrap_or("");
    let bus = bus_for(home);
    let typed_age = newest_typed_turn_age_s(&hits, &bus, sid, now)?;
    (typed_age <= WINDOW_SECS).then(|| format!("typed user turn {typed_age}s ago"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_turn_age_reads_the_person_and_skips_the_machines() {
        let dir = tempfile::tempdir().unwrap();
        let now = crate::daemon::now_epoch_secs();
        let ts = |age: i64| {
            chrono::DateTime::from_timestamp(now - age, 0)
                .unwrap()
                .to_rfc3339()
        };
        let path = dir.path().join("s1.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"user","timestamp":ts(60),"message":{"role":"user","content":"[cache-keepalive] Ping 1/4"}}),
                json!({"type":"user","timestamp":ts(30),"message":{"role":"user","content":"<fno_mail from=\"a\" to=\"b\" id=\"m\">run the sweep</fno_mail>"}}),
                json!({"type":"user","timestamp":ts(120),"message":{"role":"user","content":"keep the notes coming"}}),
            ),
        )
        .unwrap();
        let bus = BusIndex::empty();
        assert_eq!(
            newest_typed_turn_age_s(&[path.clone()], &bus, "s1", now),
            Some(120),
            "the newest TYPED turn decides; the keepalive and the mail never hold"
        );
        assert_eq!(newest_typed_turn_age_s(&[], &bus, "s1", now), None);
        let machine_only = dir.path().join("s2.jsonl");
        std::fs::write(
            &machine_only,
            format!(
                "{}\n",
                json!({"type":"user","timestamp":ts(10),"message":{"role":"user","content":"[cache-keepalive] Ping 2/4"}}),
            ),
        )
        .unwrap();
        assert_eq!(
            newest_typed_turn_age_s(&[machine_only], &bus, "s2", now),
            None,
            "machine-only transcript: nothing testifies"
        );
        // The append-only short-circuit: a file quiet past the window is
        // skipped even when its rows carry an in-window stamp.
        let aged = dir.path().join("s3.jsonl");
        std::fs::write(
            &aged,
            format!(
                "{}\n",
                json!({"type":"user","timestamp":ts(120),"message":{"role":"user","content":"typing now"}})
            ),
        )
        .unwrap();
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(WINDOW_SECS as u64 + 600);
        std::fs::File::options()
            .write(true)
            .open(&aged)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        assert_eq!(
            newest_typed_turn_age_s(&[aged], &bus, "s3", now),
            None,
            "a file quiet past the window is skipped"
        );
    }
}
