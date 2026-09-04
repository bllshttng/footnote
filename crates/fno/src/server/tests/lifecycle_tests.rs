//! x-1ab9 lifecycle resolution (AC2-EDGE): a sideline lifecycle keypress that
//! carries the row's harness session id resolves by that identity, never by
//! the label. The handler then hands the subprocess the row's CURRENT label
//! (`resolve_lifecycle_target(..).map(|a| a.name.clone())`), so a
//! harness-side rename between capture and keypress still reaches the right
//! session. Mounted as a child of server.rs's `mod tests` (use super::*), so
//! server.rs itself only pays two lines for it.

use super::*;

#[test]
fn lifecycle_resolution_prefers_identity_over_label() {
    let mut core = empty_core();
    let mut row = exited_claude_row("label", Some("11111111-1111-4111-8111-111111111111"));
    row.harness_session_id = Some("11111111-1111-4111-8111-111111111111".into());
    core.agents = vec![row];
    // The captured label is stale ("stale"); the carried id answers the row.
    let resolved =
        core.resolve_lifecycle_target("stale", Some("11111111-1111-4111-8111-111111111111"));
    assert_eq!(
        resolved.map(|a| a.name.clone()).as_deref(),
        Ok("label"),
        "identity resolves the row; the handler gets its current label"
    );
    // The same keypress without the id falls back to the label and refuses.
    assert!(
        core.resolve_lifecycle_target("stale", None).is_err(),
        "label-only resolution keeps today's fail-closed refusal"
    );
}

#[test]
fn lifecycle_identity_never_resolves_an_external_row() {
    // An external row is managed from its own session; identity resolution
    // must not let a keypress bypass the external gate.
    let mut core = empty_core();
    let mut row = exited_claude_row("ext", Some("22222222-2222-4222-8222-222222222222"));
    row.external = true;
    row.harness_session_id = Some("22222222-2222-4222-8222-222222222222".into());
    core.agents = vec![row];
    assert!(
        core.resolve_lifecycle_target("ext", Some("22222222-2222-4222-8222-222222222222"))
            .is_err(),
        "an external row is refused even by exact session id"
    );
}

#[test]
fn lifecycle_identity_that_misses_falls_back_to_the_label() {
    let mut core = empty_core();
    core.agents = vec![exited_claude_row(
        "label",
        Some("33333333-3333-4333-8333-333333333333"),
    )];
    // The carried id names no row (stale snapshot): the label still works.
    let resolved =
        core.resolve_lifecycle_target("label", Some("44444444-4444-4444-8444-444444444444"));
    assert_eq!(
        resolved.map(|a| a.name.clone()).as_deref(),
        Ok("label"),
        "a missed id falls back to the label"
    );
}
