//! How an adopt resolves a session's identity: the plan precedence
//! (registry row, target manifest, harness stores) and what a reap
//! receipt restores onto a freshly minted row. Split from client_verbs
//! under the file budget: the adopt cluster is one question.

use crate::lifecycle_child::heal_token;
use crate::manifest_lookup::{find_manifest_for_session, ManifestIdentity};
use crate::paths::AgentsHome;
use crate::receipt::{read_reap_receipt, reap_receipt_path_for, ReapReceipt};
use serde_json::Value;

/// Where an adoption's evidence came from (the receipt line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdoptSource {
    Registry,
    Manifest,
    HarnessStore,
}

impl AdoptSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            AdoptSource::Registry => "registry",
            AdoptSource::Manifest => "target manifest",
            AdoptSource::HarnessStore => "harness store",
        }
    }
}

/// Why an adoption did not complete.
#[derive(Debug)]
pub(crate) enum AdoptError {
    /// No evidence in any source.
    NoEvidence,
    /// A registry read/write or harness-store consultation failed.
    Io(String),
}

pub(crate) fn persist_manifest_identity(
    id: &ManifestIdentity,
    home: &AgentsHome,
) -> Result<Value, AdoptError> {
    let mut entry =
        crate::client_verbs::mint_synthesized_entry(id, &crate::daemon::now_rfc3339_like());
    entry.last_message_at = crate::claude_adopt::transcript_stamp(id.canonical_session_id());
    // same missing-model closure as the roster adopt - the claude
    // transcript states the model; the provider comes only from the
    // route-settings match and otherwise records None.
    if let Some(model) = crate::claude_adopt::transcript_model(id.canonical_session_id()) {
        entry.provider = crate::claude_adopt::provider_from_route_settings(Some(&model));
        entry.model = Some(model);
        entry.model_basis = Some("verified".to_string());
    }
    // The receipt is the only record that survives the reap, so the minted
    // row takes the identity it kept: the birth name, the node the ledger
    // resolved, and a `revived` origin naming this adoption as a comeback.
    let receipt = restore_reaped_identity(&mut entry, home);
    crate::client_verbs::upsert_synthesized_row(&home.registry_json(), entry.clone())
        .map_err(|error| AdoptError::Io(error.to_string()))?;
    if let Some(receipt) = receipt {
        journal_agent_revived(home, "adopt", &receipt, &entry);
    }
    serde_json::to_value(&entry).map_err(|error| AdoptError::Io(error.to_string()))
}

/// Fold one reap receipt's kept identity back onto a freshly minted row.
/// Returns the receipt when one existed (the caller journals the revive);
/// `None` when this session was never reaped and the mint stands as-is.
fn restore_reaped_identity(
    entry: &mut crate::state::RegistryEntry,
    home: &AgentsHome,
) -> Option<ReapReceipt> {
    let harness = entry.harness_name();
    let sid = entry.harness_session_id.as_deref()?.trim();
    if harness.is_empty() || sid.is_empty() {
        return None;
    }
    let path = reap_receipt_path_for(home, harness, sid);
    let receipt = read_reap_receipt(&path).ok()?;
    if !receipt.row_name.trim().is_empty() {
        entry.name = receipt.row_name.clone();
    }
    if entry.node.is_none() {
        entry.node = receipt
            .ledger
            .as_ref()
            .and_then(|l| l.get("node"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    entry.origin = Some("revived".into());
    Some(receipt)
}

/// The one `agent_revived` journal writer: names the verb that brought the
/// session back, the actor session that ran it, the name the row was born
/// with, and the session id. Best-effort - a journal fault never fails an
/// adoption that already persisted.
fn journal_agent_revived(
    home: &AgentsHome,
    verb: &str,
    receipt: &ReapReceipt,
    entry: &crate::state::RegistryEntry,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("verb".into(), serde_json::Value::String(verb.into()));
    fields.insert(
        "prior_name".into(),
        serde_json::Value::String(receipt.row_name.clone()),
    );
    fields.insert("name".into(), serde_json::Value::String(entry.name.clone()));
    fields.insert(
        "harness".into(),
        serde_json::Value::String(receipt.harness.clone()),
    );
    fields.insert(
        "harness_session_id".into(),
        serde_json::Value::String(receipt.harness_session_id.clone()),
    );
    if let Some(actor) = entry
        .spawned_by_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        fields.insert(
            "actor_session".into(),
            serde_json::Value::String(actor.to_string()),
        );
    }
    let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "agents");
    let _ = emitter.emit_fields("agent_revived", fields);
}

/// Resolve `session_id` to one registry row, minting one if needed, through the
/// plan precedence: an existing registry row; a `.fno/target-state.md` whose
/// session id matches; then the harness session stores (the heal-token shellout,
/// which adopts best-effort). Identity only. Returns the row (as JSON), any
/// `fno_id` carried, and the source.
pub(crate) fn synthesize_and_adopt(
    session_id: &str,
    home: &AgentsHome,
    cross_project: bool,
) -> Result<(Value, Option<String>, AdoptSource), AdoptError> {
    let registry_path = home.registry_json();
    let entries =
        crate::client_verbs::read_registry_entries(&registry_path).map_err(AdoptError::Io)?;
    // 1. Already registered (name / full id / short resolution, no store heal yet).
    if let Ok(e) = crate::client_verbs::find_agent_entry(&entries, session_id) {
        let fno_id = e
            .get("fno_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok((e.clone(), fno_id, AdoptSource::Registry));
    }
    // 2. Target manifest.
    if let Ok(Some(id)) = find_manifest_for_session(session_id) {
        // The manifest run id is not the row's id: the registry write mints
        // the row its own, so no fno_id evidence rides the receipt.
        let value = persist_manifest_identity(&id, home)?;
        return Ok((value, None, AdoptSource::Manifest));
    }
    // 3. Harness session stores (heal-token adopts best-effort and writes the row).
    match heal_token(session_id, &registry_path, cross_project, None) {
        Ok(Some(row)) => Ok((row, None, AdoptSource::HarnessStore)),
        Ok(None) => Err(AdoptError::NoEvidence),
        Err(msg) => Err(AdoptError::Io(msg)),
    }
}

/// Manifest-only adoption used as the `resume` fallback: `resolve_entry_with_heal`
/// already consulted the registry + harness stores, so this is just the manifest
/// path. Returns the minted row (already upserted), `None` when no manifest
/// matches, or the actual registry/serialization failure.
pub(crate) fn adopt_from_manifest(
    session_id: &str,
    home: &AgentsHome,
) -> Result<Option<Value>, AdoptError> {
    let Ok(Some(id)) = find_manifest_for_session(session_id) else {
        return Ok(None);
    };
    persist_manifest_identity(&id, home).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adopt_tmpdir() -> tempfile::TempDir {
        let td = tempfile::TempDir::new().unwrap();
        crate::paths::pin_test_claims_root(td.path().join("claims-root").as_path());
        td
    }

    #[test]
    fn adopt_restores_the_identity_a_reap_receipt_kept() {
        let dir = adopt_tmpdir();
        let home = AgentsHome::at(dir.path());
        let sid = "979e1acc-e240-4af5-9998-0a74ec6c0683";
        let receipt = crate::receipt::ReapReceipt {
            row_name: "t-old-name".into(),
            short_id: "979e1acc".into(),
            harness: "claude".into(),
            harness_session_id: sid.into(),
            cwd: "/w".into(),
            log_path: None,
            created_at: "t0".into(),
            reaped_at: "t1".into(),
            resume: "claude --resume <id>".into(),
            removed_by: "gc-sweep".into(),
            removal_trigger: "unattended".into(),
            schema_version: Some(2),
            identity: None,
            native_locator: None,
            model_provenance: None,
            resume_argv: vec![],
            effects: vec![],
            assignment: None,
            details_expired_at: None,
            writer_build: None,
            retirement_contract: None,
            ledger: Some(serde_json::json!({ "node": "x-old" })),
        };
        crate::receipt::write_reap_receipt(&home, &receipt).unwrap();

        let id = crate::manifest_lookup::ManifestIdentity {
            harness: "claude".into(),
            harness_session_id: sid.into(),
            ..Default::default()
        };
        let value = crate::adopt_identity::persist_manifest_identity(&id, &home).unwrap();
        // The receipt is the only record that survived the reap: the minted
        // row takes its birth name and node back, and names itself revived.
        assert_eq!(value["name"], "t-old-name");
        assert_eq!(value["node"], "x-old");
        assert_eq!(value["origin"], "revived");

        let rows = crate::event_store::query_events(
            &home.events_jsonl(),
            &crate::event_store::EventQuery::of_types(&["agent_revived"]),
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "the revive journals one event");
        assert!(rows[0].line.contains("t-old-name"), "{}", rows[0].line);

        // A session never reaped mints under the synthesized name, keeps the
        // adopted origin, and journals nothing.
        let fresh = crate::manifest_lookup::ManifestIdentity {
            harness: "claude".into(),
            harness_session_id: "aaaa1111-2222-4333-8444-555566667777".into(),
            ..Default::default()
        };
        let fresh_value = crate::adopt_identity::persist_manifest_identity(&fresh, &home).unwrap();
        assert_eq!(fresh_value["origin"], "adopted");
        let rows = crate::event_store::query_events(
            &home.events_jsonl(),
            &crate::event_store::EventQuery::of_types(&["agent_revived"]),
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "only the revived session journals");
    }
}
