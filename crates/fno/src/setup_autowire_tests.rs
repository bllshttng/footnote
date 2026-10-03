//! Tests for `fno config setup auto-wire`. Unit-shaped: classify strictness,
//! outcome wording, and the pure receipt parsers. Anything that shells to a
//! real harness CLI belongs to a manual pass.

use std::ffi::OsString;

use crate::setup_autowire::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn classify_claims_exactly_config_setup_auto_wire() {
    assert_eq!(classify(&os(&["config", "setup", "auto-wire"])), Some(()));
}

#[test]
fn classify_forwards_everything_else() {
    assert_eq!(classify(&os(&["config", "setup"])), None);
    assert_eq!(classify(&os(&["config", "setup", "wizard"])), None);
    assert_eq!(
        classify(&os(&["config", "setup", "auto-wire", "--json"])),
        None
    );
    assert_eq!(classify(&os(&["mux", "ls"])), None);
    assert_eq!(classify(&os(&["config", "get"])), None);
}

#[test]
fn outcome_lines_carry_the_wizard_wording() {
    assert_eq!(
        outcome_line("Claude Code", &Outcome::Installed(String::new())),
        "  Claude Code: installed"
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
    assert_eq!(
        outcome_line("pi", &Outcome::Failed("no door".into())),
        "  pi: FAILED (no door)"
    );
}

#[test]
fn claude_plugin_list_ids_decide_installed() {
    assert!(claude_list_has_fno(
        r#"[{"id": "fno@footnote"}, {"id": "x@y"}]"#
    ));
    assert!(claude_list_has_fno(r#"[{"id": "fno@skills-dir"}]"#));
    assert!(!claude_list_has_fno(r#"[{"id": "x@footnote"}]"#));
    assert!(!claude_list_has_fno("not json"));
    assert!(!claude_list_has_fno("[]"));
}

#[test]
fn claude_skills_dir_manifest_answers_before_any_spawn() {
    let home = std::env::temp_dir().join(format!("fno-autowire-{}", std::process::id()));
    let dest = home.join(".claude").join("skills").join(SKILLS_DROP);
    std::fs::create_dir_all(dest.join(".claude-plugin")).unwrap();
    std::fs::write(dest.join(".claude-plugin").join("plugin.json"), "{}").unwrap();
    let outcome = claude_wire(&home, &|argv| {
        let _ = argv;
        Err("spawned during test".into())
    });
    assert_eq!(outcome, Outcome::Already("skills-dir".into()));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn claude_marketplace_failure_falls_back_to_skills_dir() {
    let home = std::env::temp_dir().join(format!("fno-autowire-clone-{}", std::process::id()));
    let run = |argv: &[&str]| {
        if argv.contains(&"clone") {
            Ok(String::new())
        } else {
            Err("claude plugin: boom".into())
        }
    };
    let outcome = claude_wire(&home, &run);
    assert_eq!(
        outcome,
        Outcome::Installed("skills-dir; no `claude plugin update`".into())
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn codex_receipt_maps_to_outcomes() {
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
}

#[test]
fn opencode_receipt_decides_by_status() {
    let out = r#"{"status": "installed", "written": 3, "version": "0.4.0", "config_dir": "/cfg"}"#;
    assert_eq!(
        parse_opencode_receipt(out),
        Outcome::Installed("3 file(s) (footnote 0.4.0) -> /cfg".into())
    );
    assert_eq!(
        parse_opencode_receipt(r#"{"status": "failed"}"#),
        Outcome::Failed("0 file(s) (footnote ?) -> ?".into())
    );
}

#[test]
fn agy_paths_parse_three_lines_or_refuse() {
    assert_eq!(
        parse_agy_paths("/a/stop.sh\n/crown.sh\n/guard.sh"),
        Some([
            "/a/stop.sh".to_string(),
            "/crown.sh".to_string(),
            "/guard.sh".to_string()
        ])
    );
    assert_eq!(parse_agy_paths(""), None);
    assert_eq!(parse_agy_paths("only\ntwo"), None);
}

#[test]
fn agy_absent_adapters_come_back_empty_not_missing() {
    assert_eq!(
        parse_agy_paths("adapter.sh\n-\n-"),
        Some(["adapter.sh".to_string(), String::new(), String::new()])
    );
}
