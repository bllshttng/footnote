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

/// A duplicate id refuses and names EVERY session with its timestamp,
/// read through an explicitly resolved store - no env, no default-tree
/// dependency (the store rules the resolution owns are pinned elsewhere).
#[test]
fn a_duplicate_id_refuses_through_the_resolved_store() {
    use fno_agents::pi::{lookup_sessions_in, PiStore, StoreLayout};

    let tmp = std::env::temp_dir().join(format!("pi-ident-store-{}", std::process::id()));
    let cwd = Path::new("/repo/worktrees/pi-dupes");
    let dir = tmp.join(encode_cwd(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let stamps = ["2026-08-28T20-58-10-768Z", "2026-08-28T20-58-10-817Z"];
    for stamp in stamps {
        std::fs::write(dir.join(format!("{stamp}_fno-race-0001.jsonl")), "{}\n").unwrap();
    }
    let store = PiStore {
        root: tmp.clone(),
        layout: StoreLayout::CwdScoped,
        source: "test",
    };
    let lookup = lookup_sessions_in(&store, cwd, "fno-race-0001");
    let refusal = duplicate_resume_refusal(cwd, "fno-race-0001", &lookup);

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

/// A flat store (a set session dir) matches on the FILE HEADER's cwd, never
/// on a directory name (AC1-HP).
#[test]
fn a_flat_store_matches_the_header_cwd() {
    use fno_agents::pi::{lookup_sessions_in, PiStore, StoreLayout};

    let tmp = std::env::temp_dir().join(format!("pi-ident-flat-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let target = std::fs::write;
    let good = tmp.join("2026-09-17T00-00-00_s-1.jsonl");
    target(
        &good,
        "{\"cwd\":\"/repo\",\"id\":\"s-1\"}\n{\"type\":\"turn\"}\n",
    )
    .unwrap();
    let store = PiStore {
        root: tmp.clone(),
        layout: StoreLayout::Flat,
        source: "test",
    };
    let lookup = lookup_sessions_in(&store, Path::new("/repo"), "s-1");
    match lookup {
        SessionLookup::One { file } => assert_eq!(file, good),
        other => panic!("one header match must read One, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The flat header filter refuses to guess past a file it cannot read or
/// parse, and never counts a sibling cwd's file (AC1-ERR).
#[test]
fn a_flat_store_reads_unknown_past_an_unparseable_header() {
    use fno_agents::pi::{lookup_sessions_in, PiStore, StoreLayout};

    let tmp = std::env::temp_dir().join(format!("pi-ident-flatx-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    // A sibling cwd's file: parsed fine, never counted for /repo.
    std::fs::write(
        tmp.join("2026-09-17T00-00-01_s-1.jsonl"),
        "{\"cwd\":\"/other\",\"id\":\"s-1\"}\n",
    )
    .unwrap();
    // An unparseable header.
    std::fs::write(
        tmp.join("2026-09-17T00-00-02_s-1.jsonl"),
        "not json at all\n",
    )
    .unwrap();
    let store = PiStore {
        root: tmp.clone(),
        layout: StoreLayout::Flat,
        source: "test",
    };
    let lookup = lookup_sessions_in(&store, Path::new("/repo"), "s-1");
    match lookup {
        SessionLookup::Unknown { reason, .. } => {
            assert!(
                reason.contains("unparseable session header"),
                "reason must name the defect: {reason}"
            );
            assert!(reason.contains("00-02_s-1"), "must name the file: {reason}");
        }
        other => panic!("an unparseable header must read Unknown, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// pi's own store resolution: the env overrides in pi's order, the
/// project-trust Err, the settings flat store, the default. Process-global
/// env vars, so the cases run serially inside one test.
#[test]
fn pi_store_resolution_follows_pis_own_order() {
    use fno_agents::pi::{lookup_sessions, pi_agent_dir, pi_store, PiStore, StoreLayout};

    let tmp = std::env::temp_dir().join(format!("pi-ident-res-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("agent")).unwrap();
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let sessions = tmp.join("agent").join("sessions");
    let _ = sessions;

    // AC2-HP: PI_CODING_AGENT_DIR wins; PI_HOME names nowhere in the answer.
    std::env::set_var("PI_CODING_AGENT_DIR", tmp.join("agent"));
    std::env::set_var("PI_HOME", tmp.join("elsewhere"));
    let cwd_dir = sessions.join(encode_cwd(&repo));
    std::fs::create_dir_all(&cwd_dir).unwrap();
    std::fs::write(cwd_dir.join("2026-09-17T00-00-00_s-2.jsonl"), "{}\n").unwrap();
    let lookup = lookup_sessions(&repo, "s-2");
    match &lookup {
        SessionLookup::One { file } => {
            assert!(
                file.starts_with(&sessions),
                "must read the agent dir: {file:?}"
            );
        }
        other => panic!("AC2-HP: must read PI_CODING_AGENT_DIR's store, got {other:?}"),
    }
    std::env::remove_var("PI_HOME");

    // AC2-EDGE: a project settings.json naming sessionDir is a named Err.
    let proj = repo.join(".pi");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("settings.json"),
        "{\"sessionDir\": \"/repo/store\"}\n",
    )
    .unwrap();
    let store = pi_store(&repo);
    std::env::remove_var("PI_CODING_AGENT_DIR");
    let err = store.err().expect("project sessionDir must refuse");
    assert!(err.contains("project trust"), "{err}");
    assert!(err.contains("settings.json"), "{err}");
    let _ = std::fs::remove_dir_all(&proj);

    // AC2-EDGE: a global settings sessionDir joins relative values to cwd.
    std::env::set_var("PI_CODING_AGENT_DIR", tmp.join("agent"));
    std::fs::write(
        tmp.join("agent").join("settings.json"),
        "{\"sessionDir\": \"store\"}\n",
    )
    .unwrap();
    let store = pi_store(&repo).unwrap();
    assert_eq!(store.root, repo.join("store"), "relative joins to cwd");
    assert_eq!(store.layout, StoreLayout::Flat);
    assert_eq!(store.source, "settings");
    std::fs::remove_file(tmp.join("agent").join("settings.json")).unwrap();

    // AC2-EDGE: an unparseable global settings file errs naming file+position.
    std::fs::write(tmp.join("agent").join("settings.json"), "{oops").unwrap();
    let err = pi_store(&repo)
        .err()
        .expect("unparseable settings must refuse");
    assert!(err.contains("not valid JSON"), "{err}");
    assert!(err.contains("line 1"), "{err}");
    std::fs::remove_file(tmp.join("agent").join("settings.json")).unwrap();

    // The default: cwd-scoped, under the agent dir.
    let store = pi_store(&repo).unwrap();
    assert_eq!(store.root, pi_agent_dir().join("sessions"));
    assert_eq!(store.layout, StoreLayout::CwdScoped);
    assert_eq!(store.source, "default");

    std::env::remove_var("PI_CODING_AGENT_DIR");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The colon encodes like a separator, the value pi's own encoder computes
/// (AC3-HP).
#[test]
fn the_cwd_encoding_rewrites_the_colon() {
    assert_eq!(encode_cwd(&PathBuf::from("/a/b:c")), "--a-b-c--");
}
