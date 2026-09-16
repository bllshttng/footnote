//! Lane journey tests: the backend dispatch table
//! resolves every supported (harness, substrate) combination through the
//! packaged capability contract and refuses unsupported ones by name.

use fno_agents::spawn_backends::{resolve_backend, ResolvedBackend};

#[test]
fn codex_thread_resolves_to_the_attach_with_server_lane() {
    let resolved = resolve_backend("codex", "thread");
    assert_eq!(resolved, ResolvedBackend::CodexThread);
}

#[test]
fn claude_pane_resolves_to_the_mux_hosted_backend() {
    assert_eq!(resolve_backend("claude", "pane"), ResolvedBackend::MuxPane);
}

#[test]
fn bg_and_headless_stay_client_one_shots() {
    assert_eq!(
        resolve_backend("claude", "headless"),
        ResolvedBackend::ClientOneShot
    );
    assert_eq!(
        resolve_backend("codex", "bg"),
        ResolvedBackend::ClientOneShot
    );
}

#[test]
fn self_hosted_thread_harness_refuses_by_name() {
    // A harness the contract says hosts its own detached thread client gets
    // the honest refusal, not a silent fallback.
    for harness in ["opencode", "agy"] {
        let resolved = resolve_backend(harness, "thread");
        match resolved {
            ResolvedBackend::CodexThread | ResolvedBackend::MuxPane => {
                panic!("{harness} thread must not route to another harness's lane")
            }
            ResolvedBackend::Refused(reason) => {
                assert!(
                    reason.contains(harness),
                    "refusal must name the harness: {reason}"
                );
            }
            ResolvedBackend::ClientOneShot => {
                panic!("{harness} thread must refuse, not degrade to a one-shot")
            }
            ResolvedBackend::ClaudeStream => {
                panic!("{harness} thread must not route to the claude stream lane")
            }
        }
    }
}

#[test]
fn unknown_substrate_refuses_by_name() {
    let resolved = resolve_backend("claude", "teleport");
    match resolved {
        ResolvedBackend::Refused(reason) => {
            assert!(reason.contains("teleport"), "names the substrate: {reason}");
        }
        other => panic!("unknown substrate must refuse, got {other:?}"),
    }
}
