//! What a row's measured truth says right now: the liveness word the
//! daemon serves, the identity its truth reads key on, the batched probe,
//! the title diff and its locked apply, and the one positive death proof.
//!
//! Split from daemon.rs: the file is over the line budget and shrink-only.

use crate::state;

/// The one POSITIVE death proof the sweep holds itself: a recorded pid whose
/// start time no longer matches provably ended. Folded into the answer type
/// so the reapers read one vocabulary; the ladder never answers `Dead` from
/// absence.
pub(crate) fn fold_positive_death(
    e: &state::RegistryEntry,
) -> Option<crate::client_verbs::RowLiveness> {
    e.pid
        .map(|p| !crate::daemon::pid_is_ours(p, e.pid_start_time))
        .unwrap_or(false)
        .then_some(crate::client_verbs::RowLiveness::Dead)
}

/// The claude-uuid candidate handles for the sweep's ONE truth batch: the
/// ladder never launches a serial per-row `fno agents truth` subprocess
/// inside the sweep - N rows would otherwise hold the GC worker for roughly
/// N probe timeouts. Every row qualifies, not only stamped ones: the
/// ladder's `is_live` vote (x-91f3) reads the truth rung for unstamped rows
/// too, and a stamped-only batch leaves the transcript - the one marker a
/// pid-less, unstamped claude row can carry - permanently silent for that
/// vote. An empty candidate set spends nothing.
pub(crate) fn row_truth_handles(entries: &[state::RegistryEntry]) -> Vec<String> {
    entries.iter().filter_map(row_truth_handle).collect()
}

/// The identity one row's truth reads key on: the dedicated claude uuid
/// when the row carries one, else the harness session id. Measured 2026-09-08:
/// 35 of 35 claude rows had a null uuid, which left this handle empty and
/// darkened the truth batch, the ladder's truth rung and the title detector
/// in one stroke - the fallback is the field every claude row carries.
pub(crate) fn row_truth_handle(e: &state::RegistryEntry) -> Option<String> {
    e.claude_session_uuid
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(String::from)
        .or_else(|| {
            e.harness_session_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

/// The batch over [`row_truth_handles`] as the reconcile sweep runs it,
/// returning the FULL probes, not a lowered state string: one batch feeds
/// both the liveness ladder and the title detector, and a second subprocess
/// for titles would be the same cold start paid twice per sweep.
pub(crate) fn batched_row_probes(
    entries: &[state::RegistryEntry],
    truth_tail_probes: &dyn Fn(
        &[String],
    )
        -> std::collections::HashMap<String, crate::truth_probe::TruthProbe>,
) -> std::collections::HashMap<String, crate::truth_probe::TruthProbe> {
    let handles = row_truth_handles(entries);
    if handles.is_empty() {
        return std::collections::HashMap::new();
    }
    truth_tail_probes(&handles)
}

/// The title diff the sweep's `agent_renamed` emits are built from:
/// one entry per row whose last-seen `harness_title` differs from the batch's
/// reading. The tuple is `(name, harness_session_id, from, to)` - the event
/// payload's shape, with `from` `None` on first observation. Rows without a
/// harness session id are skipped: the event names identity, and an
/// identity-less rename has no addressee. The row's
/// `name` is never written from any of this: the label is fno's, the title
/// is the harness's.
pub(crate) fn title_changes(
    entries: &[state::RegistryEntry],
    titles: &std::collections::HashMap<String, Option<String>>,
) -> Vec<(String, Option<String>, Option<String>, String)> {
    entries
        .iter()
        .filter_map(|e| {
            let handle = row_truth_handle(e)?;
            let sid = e.harness_session_id.clone().filter(|s| !s.is_empty())?;
            let new_title = titles.get(&handle)?.clone()?;
            let from = e.harness_title.clone();
            if from.as_deref() == Some(new_title.as_str()) {
                return None;
            }
            Some((e.name.clone(), Some(sid), from, new_title))
        })
        .collect()
}

/// Apply the batch's title readings to the registry under the
/// caller's lock. Keyed by identity read off the snapshot
/// the batch planned from, so a row replaced under the same label between
/// snapshot and locked write cannot receive the first row's title. The
/// stored value is the DIFF BASELINE the next sweep compares against; every
/// reader is served the probe's fresh reading with this as fallback.
pub(crate) fn apply_title_changes(
    r: &mut state::Registry,
    entries: &[state::RegistryEntry],
    titles: &std::collections::HashMap<String, Option<String>>,
) {
    for (uuid, new_title) in titles {
        let Some(new_title) = new_title else {
            continue;
        };
        let Some(e0) = entries
            .iter()
            .find(|e| row_truth_handle(e).as_deref() == Some(uuid.as_str()))
        else {
            continue;
        };
        let (harness, sid) = state::registry_write_key(e0);
        let keyed = sid
            .as_deref()
            .and_then(|sid| r.find_by_session_mut(&harness, sid));
        let target = match keyed {
            Some(e) => Some(e),
            None => r.find_mut(&e0.name),
        };
        if let Some(e) = target {
            e.harness_title = Some(new_title.clone());
        }
    }
}

/// A liveness word this binary measures is served only while it is fresh:
/// past the shared window (crates/fno/src/served_liveness.rs) it no longer
/// answers "is this row alive NOW", and republishing it as the served word
/// made a 24-hour-old `dead` read as current. Past the window the word is
/// withheld (the reader falls back to the status ladder); the stamp is
/// served unchanged so the age stays honest.
pub(crate) fn served_fresh_liveness<'a>(
    word: Option<&'a str>,
    measured_at: Option<&str>,
) -> Option<&'a str> {
    let stamp = measured_at.and_then(crate::state::rfc3339_like_to_secs)? as u64;
    crate::served_liveness::served_liveness_word(
        word,
        Some(stamp),
        crate::daemon::now_epoch_secs() as u64,
    )
}

/// Why the row's served `liveness` reads the way it does: `fresh` inside
/// the shared window, `stale` once the window passed (the word is
/// withheld), `never-measured` with no word or no stamp. Sits beside
/// `liveness` on every list row (law d-d6cb1827: a field with a basis
/// shows the basis, never blank).
pub(crate) fn served_liveness_basis(word: Option<&str>, measured_at: Option<&str>) -> &'static str {
    let stamp = measured_at
        .and_then(crate::state::rfc3339_like_to_secs)
        .map(|s| s as u64);
    crate::served_liveness::served_liveness_basis(
        word,
        stamp,
        crate::daemon::now_epoch_secs() as u64,
    )
}
