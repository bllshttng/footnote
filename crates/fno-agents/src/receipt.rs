//! Recovery receipts for a registry row that left the registry.
//!
//! Moved out of `daemon.rs` (x-a879) so the write choke point
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

/// One reaped row's recovery record (x-b150). Built from the registry row
/// itself - the fields present on every row - plus the harness-DECLARED
/// interactive resume form read from the capability table (the same single
/// source `fno whoami ledger` renders), and enriched from the ledger entry
/// when one exists. Written durably BEFORE the retain drops the row, so a
/// reaped row stays recoverable even when its ledger entry does not exist and
/// never will (kings, blueprint and rescue sessions never open a PR, so no
/// target run ever writes them one).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
    /// Who took the row, when the removal came through a NON-reap door
    /// (x-a879). A reap receipt stays byte-identical in shape to before this
    /// field existed: the key is skipped when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_by: Option<String>,
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
    home.root().join("reap-receipts").join(format!(
        "{}-{}.json",
        receipt_filename_part(&receipt.harness),
        receipt_filename_part(&receipt.harness_session_id)
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
        removed_by: None,
    })
}

/// Stage the removal accounting for one row a write path is about to drop
/// (x-a879): the receipt first, then the `registry_row_removed` event naming
/// the row, the remover and the reason. A receipt that cannot be built or
/// persisted still announces the removal (`receipt_staged: false`, the build
/// error as `reason`): an unrecoverable removal that is announced is strictly
/// better than a silent one, and refusing the write would turn an audit gap
/// into an outage. Best-effort by contract - an emission failure never fails
/// the write that triggered it.
pub fn stage_removal_accounting(
    home: &AgentsHome,
    entry: &state::RegistryEntry,
    remover: &str,
    emitter: &crate::events::EventEmitter,
) {
    let (receipt_staged, reason) = match build_reap_receipt(entry, None) {
        Ok(mut receipt) => {
            receipt.removed_by = Some(remover.to_string());
            match write_reap_receipt(home, &receipt) {
                Ok(()) => (true, "removed by an update_registry write".to_string()),
                Err(err) => (false, format!("receipt did not persist: {err}")),
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

    /// The reap receipt's on-disk shape is unchanged: `removed_by` is absent
    /// for a reap, present for a non-reap removal (x-a879 change 2).
    #[test]
    fn reap_receipt_omits_removed_by_and_a_removal_receipt_sets_it() {
        let e = sample_row("shape");
        let reap = build_reap_receipt(&e, None).unwrap();
        let reap_json: serde_json::Value = serde_json::to_value(&reap).unwrap();
        assert!(reap_json.get("removed_by").is_none());

        let mut removal = build_reap_receipt(&e, None).unwrap();
        removal.removed_by = Some("fno-agents-daemon".into());
        let removal_json: serde_json::Value = serde_json::to_value(&removal).unwrap();
        assert_eq!(
            removal_json["removed_by"],
            "fno-agents-daemon",
            "a non-reap removal says who took the row"
        );
        // Every other key is identical between the two shapes.
        let mut reap_keys: Vec<&str> =
            reap_json.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let mut removal_keys: Vec<&str> =
            removal_json.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        reap_keys.sort_unstable();
        removal_keys.sort_unstable();
        let mut expected = reap_keys.clone();
        expected.push("removed_by");
        expected.sort_unstable();
        assert_eq!(removal_keys, expected);
    }
}
