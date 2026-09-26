#![cfg(test)]

//! `argv_facts` tests: the wrapper-token provenance reads.

use super::*;

#[test]
fn node_from_argv_reads_the_wrapper_token() {
    // env(1) wrapper prefix: `env FNO_AGENT_SELF=... FNO_NODE=x-66e8 ... claude`.
    let argv: Vec<String> = [
        "env",
        "FNO_AGENT_SELF=peer",
        "FNO_NODE=x-66e8",
        "FNO_SLUG=some-slug",
        "claude",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(node_from_argv(&argv), Some("x-66e8".to_string()));
}

#[test]
fn node_from_argv_is_none_for_ad_hoc_pane() {
    let ad_hoc = |a: &[&str]| node_from_argv(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    // A plain `pane run htop` (no wrapper) has no provenance.
    assert_eq!(ad_hoc(&["htop"]), None);
    // An empty-valued token is treated as absent (no empty-string exports).
    assert_eq!(ad_hoc(&["env", "FNO_NODE=", "sh"]), None);
    // A command that merely MENTIONS FNO_NODE= in its own args is not
    // provenance: scanning stops at the command (first non-`NAME=` token).
    assert_eq!(
        ad_hoc(&["env", "FOO=1", "grep", "FNO_NODE=x", "file"]),
        None
    );
    // No `env` wrapper at all -> never scanned, even with a bare token.
    assert_eq!(ad_hoc(&["grep", "FNO_NODE=x", "file"]), None);
}

#[test]
fn portal_hold_from_argv_reads_the_held_row_past_the_env_wrapper() {
    let argv: Vec<String> = ["env", "FNO_PORTAL_HELD=deadbee1", "/bin/zsh"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(portal_hold_from_argv(&argv), Some("deadbee1".to_string()));
    // A plain shell carries no held-portal provenance.
    assert_eq!(portal_hold_from_argv(&["/bin/zsh".to_string()]), None);
    // The marker must sit in the wrapper's assignment run, not in a
    // command's own args.
    let mention: Vec<String> = ["grep", "FNO_PORTAL_HELD=x", "file"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(portal_hold_from_argv(&mention), None);
}
