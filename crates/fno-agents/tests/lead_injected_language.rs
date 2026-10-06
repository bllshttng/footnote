use fno_agents::team_names::PendingSuccession;
use std::path::Path;

fn capture_tree(path: &Path, text: &mut String) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            capture_tree(&path, text);
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            text.push_str(&std::fs::read_to_string(path).unwrap());
        }
    }
}

#[test]
fn every_lead_injection_template_and_rendered_succession_uses_role_vocabulary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut captured = String::new();
    for path in ["skills/lead", "skills/using-fno"] {
        capture_tree(&root.join(path), &mut captured);
    }
    for path in [
        "hooks/context-nudge.sh",
        "hooks/session-start.sh",
        "hooks/lead-postcompact-reinject.sh",
        "cli/src/fno/config/_lead.py",
        "crates/fno-agents/src/lead_checkin.rs",
        "crates/fno-agents/src/loopcheck.rs",
        "crates/fno-agents/src/team_settle.rs",
        "crates/fno-agents/src/succession_txn.rs",
    ] {
        captured.push_str(&std::fs::read_to_string(root.join(path)).unwrap());
    }
    let announcement = fno_agents::succession_txn::render_announcement(
        "scope",
        &PendingSuccession {
            successor_name: "Avery".into(),
            successor_session: Some("new-session".into()),
            predecessor_name: "Rowan".into(),
            predecessor_session: Some("old-session".into()),
            ts: "2026-01-01T00:00:00Z".into(),
        },
    );
    assert!(announcement.contains("Rowan"));
    assert!(announcement.contains("Avery"));
    assert!(announcement.contains("new-session"));
    captured.push_str(&announcement);
    assert!(!fno_agents::role_migration::contains_legacy_vocabulary(
        &captured
    ));
}
