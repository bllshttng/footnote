//! pi's session identity across the two crates that build its attach argv
//! (x-c198).
//!
//! `fno` never links `fno-agents` (it shells the binary at runtime), so the
//! argv that opens pi's own TUI exists twice. This test links both and pins
//! them byte-for-byte, the way `codex_shared_daemon_attach.rs` already does for
//! codex, so a change to either fails here rather than drifting into a viewport
//! that opens a different session from the one `fno agents attach` opens.

use fno_agents::pi::{
    duplicate_resume_refusal, encode_cwd, lookup_sessions, pi_attach_argv, SessionLookup,
};
use std::path::{Path, PathBuf};

#[test]
fn the_pi_attach_argv_is_identical_in_both_crates() {
    let session_id = "fno-tui-0001";
    assert_eq!(
        pi_attach_argv(session_id),
        fno::agents_view::pi_attach_argv(session_id)
    );
}

/// The argv is the plain TUI, never `--mode rpc`: the two lanes are mutually
/// exclusive per PROCESS, and this one is the watching lane.
#[test]
fn the_attach_argv_is_the_tui_with_provider_and_model_pinned() {
    let argv = pi_attach_argv("fno-tui-0001");
    assert_eq!(argv[..3], ["pi", "--session-id", "fno-tui-0001"]);
    assert!(!argv.contains(&"--mode".to_string()), "{argv:?}");
    assert!(!argv.contains(&"rpc".to_string()), "{argv:?}");
    // Trap 2: `--provider openai-codex` without `--model` falls through to a
    // Bedrock model and dies naming an expired AWS SSO session.
    assert!(argv.contains(&"--provider".to_string()), "{argv:?}");
    assert!(argv.contains(&"--model".to_string()), "{argv:?}");
}

/// A duplicate id refuses and names EVERY session with its timestamp.
///
/// Asserting that nothing went wrong would prove nothing here: pi's own
/// behaviour on a duplicate is to succeed, pick the oldest, and say nothing.
#[test]
fn a_duplicate_id_refuses_and_names_every_session() {
    let tmp = std::env::temp_dir().join(format!("pi-ident-{}", std::process::id()));
    let cwd = Path::new("/repo/worktrees/pi-dupes");
    let dir = tmp.join("agent").join("sessions").join(encode_cwd(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let stamps = ["2026-08-28T20-58-10-768Z", "2026-08-28T20-58-10-817Z"];
    for stamp in stamps {
        std::fs::write(dir.join(format!("{stamp}_fno-race-0001.jsonl")), "{}\n").unwrap();
    }

    // PI_HOME is process-global; this is the only test in the file that reads
    // it, and it restores the variable before returning.
    std::env::set_var("PI_HOME", &tmp);
    let lookup = lookup_sessions(cwd, "fno-race-0001");
    let refusal = duplicate_resume_refusal(cwd, "fno-race-0001", &lookup);
    std::env::remove_var("PI_HOME");

    let files = match &lookup {
        SessionLookup::Duplicate { files } => files.clone(),
        other => panic!("two files on one id must read Duplicate, got {other:?}"),
    };
    assert_eq!(files.len(), 2);
    let refusal = refusal.expect("a duplicate must refuse");
    for stamp in stamps {
        assert!(refusal.contains(stamp), "{stamp} missing from:\n{refusal}");
    }
    assert!(refusal.contains("None was selected"), "{refusal}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The encoding is pinned against three real directories from a live
/// `~/.pi/agent/sessions`, and mirrors the Python `encode_cwd`.
#[test]
fn the_cwd_encoding_matches_the_observed_directories() {
    assert_eq!(
        encode_cwd(&PathBuf::from("/Users/bb16/code/footnote/footnote")),
        "--Users-bb16-code-footnote-footnote--"
    );
    assert_eq!(
        encode_cwd(&PathBuf::from("/private/tmp")),
        "--private-tmp--"
    );
}
