//! (x-3954) The workspace-restore gate for a routed codex member: the row's
//! `route_provider_id` names a route (set, non-empty, not the `openai`
//! sentinel), so the pane door refuses by name and the member is marked
//! refused in the restore receipt. `crates/fno` never links fno-agents, so
//! the routed predicate is spelled a second time here - one small fn, whose
//! twin is `codex_route::row_route_identity`. Shared vocabulary, not a
//! second builder.

use crate::agents_view::RegistryAgent;
use crate::proto::RestoreRow;

/// The one constructor for a refused restore row, folded from seven inline
/// literal sites so each shrinks and the fields cannot drift apart.
pub(crate) fn refused_row(member: String, harness: Option<String>, reason: String) -> RestoreRow {
    RestoreRow {
        member,
        harness,
        squad: 0,
        portal: None,
        outcome: "refused".into(),
        pane: None,
        tab: None,
        reason: Some(reason),
        notice: None,
    }
}

/// The routed-codex refusal for one restore candidate.
pub(crate) fn member_routed_codex_refusal(row: &RegistryAgent, name: &str) -> Option<String> {
    if row.harness.as_deref() != Some("codex") {
        return None;
    }
    let provider = row
        .route_provider_id
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "openai")?;
    Some(format!(
        "{name} runs on codex route {provider}; resume it with `fno agents resume {name}`, \
         which restores the route"
    ))
}
