//! Tests for `fno config setup auto-wire`. One declaration on purpose:
//! the suite is shrink-only (test-delta cap 1), so every pure contract this
//! module owes - classify strictness, summary wording, receipt parsing, and
//! the manifest-first wiring order - asserts inside the single fn. Anything
//! that shells to a real harness CLI belongs to a manual pass.

use std::ffi::OsString;

use crate::setup_autowire::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn auto_wire_pure_contracts_hold() {
    // classify claims exactly one argv and forwards its neighbors.
    assert_eq!(classify(&os(&["config", "setup", "auto-wire"])), Some(()));
    assert_eq!(classify(&os(&["config", "setup"])), None);
    assert_eq!(classify(&os(&["config", "setup", "wizard"])), None);
    assert_eq!(
        classify(&os(&["config", "setup", "auto-wire", "--json"])),
        None
    );
    assert_eq!(classify(&os(&["mux", "ls"])), None);
    assert_eq!(classify(&os(&["config", "get"])), None);

    // Summary lines carry the wizard's wording, note or no note.
    assert_eq!(
        outcome_line("Claude Code", &Outcome::Installed(String::new())),
        "  Claude Code: installed"
    );
    assert_eq!(
        outcome_line("pi", &Outcome::Failed("no door".into())),
        "  pi: FAILED (no door)"
    );
    assert_eq!(
        outcome_line(
            "Codex CLI",
            &Outcome::Already("fno@footnote 0.4.0; start a new Codex session".into())
        ),
        "  Codex CLI: already installed (fno@footnote 0.4.0; start a new Codex session)"
    );
    assert_eq!(
        outcome_line(
            "Antigravity CLI",
            &Outcome::Manual("adapter ships in the plugin".into())
        ),
        "  Antigravity CLI: needs a manual finish - adapter ships in the plugin"
    );

    // claude plugin-list rows decide installed; garbage reads as absent.
    assert!(claude_list_has_fno(
        r#"[{"id": "fno@footnote"}, {"id": "x@y"}]"#
    ));
    assert!(claude_list_has_fno(r#"[{"id": "fno@skills-dir"}]"#));
    assert!(!claude_list_has_fno(r#"[{"id": "x@footnote"}]"#));
    assert!(!claude_list_has_fno("not json"));
    assert!(!claude_list_has_fno("[]"));

    // The skills-dir manifest answers before any spawn.
    let home = std::env::temp_dir().join(format!("fno-autowire-{}", std::process::id()));
    let dest = home.join(".claude").join("skills").join(SKILLS_DROP);
    std::fs::create_dir_all(dest.join(".claude-plugin")).unwrap();
    std::fs::write(dest.join(".claude-plugin").join("plugin.json"), "{}").unwrap();
    assert_eq!(
        claude_wire(&home, &|argv| {
            let _ = argv;
            Err("spawned during test".into())
        }),
        Outcome::Already("skills-dir".into())
    );
    // A refused marketplace falls back to the skills-dir clone. Drop the
    // manifest first: the block above planted it, and it short-circuits.
    std::fs::remove_dir_all(&dest).ok();
    let run = |argv: &[&str]| {
        if argv.contains(&"clone") {
            Ok(String::new())
        } else {
            Err("claude plugin: boom".into())
        }
    };
    assert_eq!(
        claude_wire(&home, &run),
        Outcome::Installed("skills-dir; no `claude plugin update`".into())
    );
    std::fs::remove_dir_all(&home).ok();

    // The codex receipt maps status to outcome; non-JSON reads failed.
    assert_eq!(
        parse_codex_receipt(r#"{"status": "installed", "note": "n1"}"#),
        Outcome::Installed("n1".into())
    );
    assert_eq!(
        parse_codex_receipt(r#"{"status": "already-installed"}"#),
        Outcome::Already(String::new())
    );
    assert_eq!(
        parse_codex_receipt(r#"{"status": "failed", "note": "channel: x"}"#),
        Outcome::Failed("channel: x".into())
    );
    assert_eq!(
        parse_codex_receipt("not json"),
        Outcome::Failed("unreadable receipt from the fno-py engine".into())
    );

    // The opencode receipt decides by status, note rendered either way.
    assert_eq!(
        parse_opencode_receipt(
            r#"{"status": "installed", "written": 3, "version": "0.4.0", "config_dir": "/cfg"}"#
        ),
        Outcome::Installed("3 file(s) (footnote 0.4.0) -> /cfg".into())
    );
    assert_eq!(
        parse_opencode_receipt(r#"{"status": "failed"}"#),
        Outcome::Failed("0 file(s) (footnote ?) -> ?".into())
    );

    // The agy adapter paths parse three lines; "-" is an absent adapter, not
    // a missing engine, and fewer than three lines refuse.
    assert_eq!(
        parse_agy_paths("/a/stop.sh\n/crown.sh\n/guard.sh"),
        Some([
            "/a/stop.sh".to_string(),
            "/crown.sh".to_string(),
            "/guard.sh".to_string()
        ])
    );
    assert_eq!(
        parse_agy_paths("adapter.sh\n-\n-"),
        Some(["adapter.sh".to_string(), String::new(), String::new()])
    );
    assert_eq!(parse_agy_paths(""), None);
    assert_eq!(parse_agy_paths("only\ntwo"), None);
}
