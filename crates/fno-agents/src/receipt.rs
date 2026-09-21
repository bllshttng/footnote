//! Recovery receipts for a registry row that left the registry.
//!
//! Moved out of `daemon.rs` so the write choke point
//! (`state::update_registry`) can stage the same receipt for a row removed
//! through ANY door, not only the reap sweep. `daemon.rs` re-exports the
//! moved items, so existing reap-path references are unchanged.
//!
//! The durability ordering is load-bearing and shared by both callers:
//! receipt FIRST, then the event, so an auditor never sees a removal
//! announced with no recovery path beside it.

use serde_json::{json, Value};

use crate::paths::AgentsHome;
use crate::state;

/// The door that produced a receipt. The vocabulary is closed: a new
/// removal door extends this enum, and the compiler then refuses its
/// writer until it names itself through the required builder parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Writer {
    /// The scheduled retirement sweep and `fno-agents reap --apply`.
    GcSweep,
    /// The roster sweep for harness rows no fno row names.
    RosterReap,
    /// The merge reaper's tree/row cleanup.
    #[allow(dead_code)] // vocabulary sibling; the merge reaper emits
    // events, not receipts, until a door routes its receipts here
    MergeReaper,
    /// The registry choke point: the surface is whatever argv named.
    RegistryWrite,
}

impl Writer {
    pub fn surface(self) -> String {
        match self {
            Writer::GcSweep => "gc-sweep".to_string(),
            Writer::RosterReap => "roster-reap".to_string(),
            Writer::MergeReaper => "merge-reaper".to_string(),
            // One verb serves a human shell and a daemon loop, so the
            // surface is the whole bounded invocation, not the binary.
            Writer::RegistryWrite => crate::state::invocation_verb(),
        }
    }
    /// RegistryWrite reads argv0: the daemon binary is unattended, any
    /// other invocation is a session's call. The stem (not the whole
    /// name) matches, so `fno-agents-daemon.exe` still classifies.
    pub fn trigger(self) -> &'static str {
        match self {
            Writer::RegistryWrite => {
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.file_stem().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                if exe == crate::component_update::AGENTS_DAEMON {
                    "unattended"
                } else {
                    "session"
                }
            }
            _ => "unattended",
        }
    }
}

/// One reaped row's recovery record. Built from the registry row
/// itself - the fields present on every row - plus the harness-DECLARED
/// interactive resume form read from the capability table (the same single
/// source `fno whoami ledger` renders), and enriched from the ledger entry
/// when one exists. Written durably BEFORE the retain drops the row, so a
/// reaped row stays recoverable even when its ledger entry does not exist and
/// never will (kings, blueprint and rescue sessions never open a PR, so no
/// target run ever writes them one).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReapReceipt {
    pub row_name: String,
    pub short_id: String,
    pub harness: String,
    pub harness_session_id: String,
    pub cwd: String,
    pub log_path: Option<String>,
    pub created_at: String,
    pub reaped_at: String,
    /// The resume command, rendered from the capability table's
    /// `interactive_resume` form. Never hardcoded here.
    pub resume: String,
    /// Ledger enrichment (node / pr / plan) when the session resolves there.
    /// The ledger stays the richer source; this is the copy that survives
    /// when the row has no ledger entry at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger: Option<Value>,
    /// The SURFACE that took the row, never the process that ran it:
    /// `gc-sweep`, `roster-reap`, `merge-reaper`, or the argv verb for a
    /// removal that came through the registry choke point. Empty only on
    /// a receipt written before this field was required.
    #[serde(default)]
    pub removed_by: String,
    /// `unattended` for a sweep nobody asked for, `session` for a call a
    /// session made. `fno agents rm` is reachable from both, so the writer
    /// name alone cannot separate them. Empty on pre-stamp receipts.
    #[serde(default)]
    pub removal_trigger: String,
    /// Receipt schema version. v1 receipts predate the field and read as
    /// `None` (migration reads old receipts without inventing fields); every
    /// new write stamps `Some(2)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// The full native identity and store context: harness, session id,
    /// store root, project dir. `None` on a v1 receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<serde_json::Value>,
    /// Where the native history lives (transcript paths, index records), so
    /// resume can find the same session after every active surface is gone.
    /// `None` on a v1 receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_locator: Option<serde_json::Value>,
    /// Known model provenance: what was requested and/or observed for this
    /// session's lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provenance: Option<serde_json::Value>,
    /// The resume command as TOKENS with no shell quoting implied. The v1
    /// `resume` string stays for rendering; tokens are what a launcher runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume_argv: Vec<String>,
    /// Per-effect retirement progress, appended as effects land so a
    /// crash/retry continues instead of repeating (task 3). Empty on
    /// a v1 receipt and on every receipt until the first effect records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<EffectRecord>,
    /// Assignment evidence: the node links and completion basis this
    /// retirement was decided on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<serde_json::Value>,
    /// Set when the retention window expired the expendable detail (ledger
    /// enrichment, per-effect rows) but the identity-critical core was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_expired_at: Option<String>,
    /// The binary that applied the retirement, stamped at write time as
    /// provenance. Absent on v1 receipts. The pin `reap --verify` audits
    /// against is `retirement_contract`, not this stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_build: Option<String>,
    /// The retirement contract the writer promised
    /// (`gc_verify::retirement_contract`): the pin `reap --verify` audits
    /// against. Absent on receipts written before the stamp existed, and
    /// those skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement_contract: Option<String>,
}

/// One retirement effect's durable record: what ran, what answered, when.
/// The outcome vocabulary is the typed outcome set the lifecycle reports;
/// a partial failure is never an overall success because each effect names
/// its own.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EffectRecord {
    pub op: String,
    /// `confirmed-removed` | `confirmed-already-absent` | `kept` |
    /// `failed` | `not-applicable`
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at: String,
}

/// Load one receipt from disk. All v2 fields default, so a v1 file reads
/// with them absent rather than invented.
pub fn read_reap_receipt(path: &std::path::Path) -> std::io::Result<ReapReceipt> {
    let raw = std::fs::read(path)?;
    serde_json::from_slice(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Expire the expendable fields of one aged receipt, keeping the identity
/// core. Returns true when the file still exists (rewritten); false when it
/// was already gone. A receipt with nothing expendable left is left
/// byte-identical.
pub fn expire_receipt_details(receipt: &mut ReapReceipt) -> bool {
    let mut changed = false;
    if receipt.ledger.is_some() {
        receipt.ledger = None;
        changed = true;
    }
    if !receipt.effects.is_empty() {
        receipt.effects.clear();
        changed = true;
    }
    if receipt.log_path.take().is_some() {
        changed = true;
    }
    if changed && receipt.details_expired_at.is_none() {
        receipt.details_expired_at = Some(crate::daemon::now_rfc3339_like());
    }
    changed
}

/// Sanitize a receipt filename component: the session id comes from registry
/// rows and is not guaranteed filename-safe across harnesses.
pub fn receipt_filename_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where reap receipts live: `<agents home>/reap-receipts/`, one file per
/// reaped row keyed by `<harness>-<session id>`, the resume identity.
pub fn reap_receipt_path(home: &AgentsHome, receipt: &ReapReceipt) -> std::path::PathBuf {
    reap_receipt_path_for(home, &receipt.harness, &receipt.harness_session_id)
}

/// The receipt path for one resume identity, so a reader without a receipt
/// in hand (the verify gate's event-derived cohort) spells the key once.
pub fn reap_receipt_path_for(
    home: &AgentsHome,
    harness: &str,
    session_id: &str,
) -> std::path::PathBuf {
    home.root().join("reap-receipts").join(format!(
        "{}-{}.json",
        receipt_filename_part(harness),
        receipt_filename_part(session_id)
    ))
}

/// Persist one receipt durably. 0600 like the rest of the agents tree.
pub fn write_reap_receipt(home: &AgentsHome, receipt: &ReapReceipt) -> std::io::Result<()> {
    let path = reap_receipt_path(home, receipt);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(receipt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
    )?;
    let _ = crate::paths::set_file_mode_0600(&path);
    Ok(())
}

/// Build the receipt from the row, or say exactly why it cannot be built.
///
/// The gate's positive requirement: a resume command from the capability
/// table. A row with no session identity, an empty harness name, a harness
/// with no capability row, or a harness that declares no interactive resume
/// form has no record of how to come back - that is the Unknown case, and
/// unknown never reaps.
pub fn build_reap_receipt(
    e: &state::RegistryEntry,
    ledger: Option<&Value>,
    writer: Writer,
) -> Result<ReapReceipt, String> {
    let harness = e.harness_name();
    if harness.is_empty() {
        return Err("missing harness identity".to_string());
    }
    let sid = e
        .harness_session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing harness session identity".to_string())?;
    let contract = crate::harness_capabilities::HarnessContract::packaged()
        .map_err(|err| format!("capability table unreadable: {err}"))?;
    let argv = contract
        .render_session_argv(harness, "interactive_resume", Some(sid))
        .map_err(|err| format!("no interactive resume form declared: {err}"))?;
    // The native locator: every transcript candidate the harness's own store
    // holds for this session, discovered at receipt time. A store read that
    // fails leaves the locator absent-but-named rather than blocking the
    // receipt - the resume tokens above already carry the identity.
    let transcripts = {
        let mut index = crate::gc_inventory::HarnessStoreIndex::default();
        index.matches(e).unwrap_or_default()
    };
    let identity = serde_json::json!({
        "harness": harness,
        "session_id": sid,
        "short_id": e.short_id,
        "store_root": store_root_for(harness).map(|p| p.to_string_lossy().to_string()),
        "cwd": e.cwd,
    });
    Ok(ReapReceipt {
        row_name: e.name.clone(),
        short_id: e.short_id.clone(),
        harness: harness.to_string(),
        harness_session_id: sid.to_string(),
        cwd: e.cwd.clone(),
        log_path: e.log_path.clone(),
        created_at: e.created_at.clone(),
        reaped_at: crate::daemon::now_rfc3339_like(),
        resume: argv.join(" "),
        ledger: ledger.cloned(),
        removed_by: writer.surface(),
        removal_trigger: writer.trigger().to_string(),
        schema_version: Some(2),
        identity: Some(identity),
        native_locator: Some(serde_json::json!({ "transcripts": transcripts })),
        model_provenance: model_provenance_of(e),
        resume_argv: argv,
        effects: Vec::new(),
        assignment: None,
        details_expired_at: None,
        writer_build: Some(crate::gc_verify::current_build()),
        retirement_contract: Some(crate::gc_verify::retirement_contract()),
    })
}

/// The store root one harness's transcripts live under, for the receipt's
/// identity record. `None` for a harness with no transcript store fno reads.
fn store_root_for(harness: &str) -> Option<std::path::PathBuf> {
    match harness {
        "claude" => std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".claude").join("projects")),
        "codex" => crate::client_verbs::codex_home().map(|h| h.join("sessions")),
        _ => None,
    }
}

/// Requested/observed model provenance from the row's axis fields, in the
/// shape (a bare model is two facts; the basis travels beside it).
fn model_provenance_of(e: &state::RegistryEntry) -> Option<serde_json::Value> {
    let model = e.model.as_deref()?;
    let mut out = serde_json::json!({ "model": model });
    if let Some(basis) = e.model_basis.as_deref() {
        out["basis"] = serde_json::Value::String(basis.to_string());
    }
    Some(out)
}

/// Stage the removal accounting for one row a write path is about to drop:
/// the receipt first, then the `registry_row_removed` event naming
/// the row, the remover and the reason. A receipt that cannot be built or
/// persisted still announces the removal (`receipt_staged: false`, the build
/// error as `reason`, the attempted harness-side outcome as `active_surface`):
/// an unrecoverable removal that is announced is strictly better than a silent
/// one, and refusing the write would turn an audit gap into an outage.
/// Best-effort by contract - an emission failure never fails the write that
/// triggered it.
pub fn stage_removal_accounting(
    home: &AgentsHome,
    entry: &state::RegistryEntry,
    remover: &str,
    emitter: &crate::events::EventEmitter,
) {
    // The active-surface outcome outlives the receipt write on purpose. The
    // harness-side removal happens BEFORE that write, so a write that fails
    // leaves the harness row already gone with no receipt on disk - the event
    // is then the only durable record of it, and must name it. `None` means
    // no attempt was made, never "attempted, outcome unknown".
    let mut active_surface: Option<&'static str> = None;
    let (receipt_staged, reason) = match build_reap_receipt(entry, None, Writer::RegistryWrite) {
        Ok(mut receipt) => {
            // A receipt already on disk for this session was staged moments
            // ago by the reap sweep (or the watchdog) BEFORE it dropped the
            // rows - rewriting it would stamp `removed_by` onto a pure reap
            // receipt and change its shape. The record on disk is
            // already the recovery path; leave it byte-identical.
            if reap_receipt_path(home, &receipt).exists() {
                (true, "receipt already staged for this session".to_string())
            } else {
                // The door that dropped the row also removes the harness
                // side, and its receipt records the attempt. The sweep's
                // receipt already carries its own effects (left untouched
                // above), so the sweep path never attempts twice.
                let outcome = crate::gc_native::apply_active_surface_removal(entry);
                active_surface = Some(outcome.as_str());
                receipt
                    .effects
                    .push(outcome.effect_record("active-surface"));
                match write_reap_receipt(home, &receipt) {
                    Ok(()) => (
                        true,
                        format!("removed by an update_registry write ({})", outcome.as_str()),
                    ),
                    Err(err) => (false, format!("receipt did not persist: {err}")),
                }
            }
        }
        Err(err) => (false, err),
    };
    let _ = emitter.emit(
        "registry_row_removed",
        &json!({
            "name": entry.name,
            "short_id": entry.short_id,
            "harness": entry.harness_name(),
            "harness_session_id": entry.harness_session_id.clone().unwrap_or_default(),
            "remover": remover,
            "reason": reason,
            "receipt_staged": receipt_staged,
            "active_surface": active_surface,
            "pid": std::process::id(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(name: &str) -> state::RegistryEntry {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","short_id":"{name}-id","harness":"claude","harness_session_id":"{name}-session","cwd":"/tmp/x","log_path":"/tmp/x.log","created_at":"2026-09-01T00:00:00Z","status":"live"}}"#
        ))
        .unwrap()
    }

    /// Every receipt names its writer: two `Writer` values produce the same
    /// key set and different stamps. The old absence assertions are
    /// gone; a receipt that cannot name its writer is not constructible.
    #[test]
    fn every_receipt_names_its_writer_and_the_key_sets_match() {
        let e = sample_row("shape");
        let sweep = build_reap_receipt(&e, None, Writer::GcSweep).unwrap();
        let choke = build_reap_receipt(&e, None, Writer::RegistryWrite).unwrap();
        let sweep_json: serde_json::Value = serde_json::to_value(&sweep).unwrap();
        let choke_json: serde_json::Value = serde_json::to_value(&choke).unwrap();
        assert_eq!(sweep_json["removed_by"], "gc-sweep");
        assert_eq!(sweep_json["removal_trigger"], "unattended");
        assert!(
            choke_json["removed_by"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "the choke point names its surface: {choke_json}"
        );
        assert_eq!(
            choke_json["removal_trigger"], "session",
            "a cargo test binary is a session, not the daemon: {choke_json}"
        );
        assert_ne!(
            sweep_json["removed_by"], choke_json["removed_by"],
            "two doors never stamp the same surface"
        );
        // The two writers write the same file shape.
        let mut sweep_keys: Vec<&str> = sweep_json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        let mut choke_keys: Vec<&str> = choke_json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        sweep_keys.sort_unstable();
        choke_keys.sort_unstable();
        assert_eq!(sweep_keys, choke_keys);
    }

    /// A receipt write that fails leaves the harness-side removal already
    /// done and nothing on disk to say so. The event is then the only record,
    /// so it must still name the active-surface outcome. The row is
    /// `opencode`, whose cascade is `not-applicable` and shells out to
    /// nothing, so the assertion measures the reporting, not a harness.
    #[test]
    fn a_failed_receipt_write_still_names_the_active_surface_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agents");
        std::fs::create_dir_all(&root).unwrap();
        // A FILE where the receipt directory belongs: create_dir_all fails,
        // so write_reap_receipt returns Err on a path nothing else touches.
        std::fs::write(root.join("reap-receipts"), b"not a directory").unwrap();
        let home = AgentsHome::at(&root);

        let entry: state::RegistryEntry = serde_json::from_str(
            r#"{"name":"wkE","short_id":"wkE-id","harness":"opencode","harness_session_id":"wkE-session","cwd":"/tmp/x","log_path":"/tmp/x.log","created_at":"2026-09-01T00:00:00Z","status":"live"}"#,
        )
        .unwrap();
        let events = dir.path().join("events.jsonl");
        let emitter = crate::events::EventEmitter::new(&events, "test");

        stage_removal_accounting(&home, &entry, "test-remover", &emitter);

        let line = std::fs::read_to_string(&events).unwrap();
        let event: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let data = &event["data"];
        assert_eq!(event["type"], "registry_row_removed");
        assert_eq!(data["receipt_staged"], false, "the write was made to fail");
        assert_eq!(
            data["active_surface"], "not-applicable",
            "the removal that already happened is named even with no receipt"
        );
    }
}
