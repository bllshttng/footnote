use super::*;
use crate::graph_store;

fn one(cmd: &str, cwd: &str) -> Launch {
    let got = parse_command(cmd, cwd);
    assert_eq!(got.len(), 1, "{cmd:?} -> {got:?}");
    got.into_iter().next().unwrap()
}

#[test]
fn a_claude_bg_launch_names_its_worker_and_node() {
    let got = one(
        r#"claude --bg -n t-x-ab12-glm --model glm "/fno:target x-ab12" > /tmp/l 2>&1 &"#,
        "/repo",
    );
    assert_eq!(got.harness, "claude");
    assert_eq!(got.name.as_deref(), Some("t-x-ab12-glm"));
    assert_eq!(got.node.as_deref(), Some("x-ab12"));
    assert_eq!(got.cwd, "/repo");
}

#[test]
fn every_harness_launch_in_one_command_is_seen() {
    let got = parse_command(
        r#"cd /w && codex exec "fix it"; opencode run "x" && pi -p "y" | agy -p "$fno:review x-cd34"; fno agents spawn --name t-x-ef56 -H codex --node x-ef56 '/fno:target x-ef56'"#,
        "/repo",
    );
    let harnesses: Vec<&str> = got.iter().map(|l| l.harness.as_str()).collect();
    assert_eq!(harnesses, ["codex", "opencode", "pi", "agy", "codex"]);
    assert_eq!(got[0].cwd, "/w");
    assert_eq!(got[3].node.as_deref(), Some("x-cd34"));
    // A spawn takes its name, harness and node from its own flags.
    assert_eq!(got[4].name.as_deref(), Some("t-x-ef56"));
    assert_eq!(got[4].node.as_deref(), Some("x-ef56"));
}

#[test]
fn reads_and_help_are_not_launches() {
    for cmd in [
        "claude --version",
        "claude --bg --help",
        "grep -n 'claude --bg' notes.md",
        "codex exec resume abc",
        "codex --help",
        "opencode",
        "echo pi",
    ] {
        assert!(parse_command(cmd, "/repo").is_empty(), "{cmd}");
    }
}

fn child(sid: &str, harness: &str, cwd: &str, name: Option<&str>, at: i64) -> Child {
    Child {
        harness: harness.into(),
        session_id: sid.into(),
        cwd: cwd.into(),
        name: name.map(str::to_string),
        started_at_ms: at,
    }
}

#[test]
fn the_named_child_claims_its_record_once() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().to_string_lossy().into_owned();
    let n = record_command(
        home.path(),
        "claude --bg -n w1 hi && claude --bg -n w2 hi",
        &cwd,
        Some("lead-1"),
        Some("claude"),
        1_000_000,
    )
    .unwrap();
    assert_eq!(n, 2);
    let got = claim(
        home.path(),
        &child("c-2", "claude", &cwd, Some("w2"), 1_003_000),
    )
    .unwrap();
    assert_eq!(got.launch.name.as_deref(), Some("w2"));
    assert_eq!(got.launcher_session, "lead-1");
    // A second claim by the same child returns the same record; another
    // child named w2 finds nothing left.
    assert_eq!(
        claim(
            home.path(),
            &child("c-2", "claude", &cwd, Some("w2"), 1_004_000)
        )
        .unwrap(),
        got
    );
    assert!(claim(
        home.path(),
        &child("c-9", "claude", &cwd, Some("w2"), 1_004_000)
    )
    .is_none());
    assert_eq!(
        claimed_edge(home.path(), "c-2").unwrap().launcher_session,
        "lead-1"
    );
}

#[test]
fn ambiguous_unattributed_and_stale_records_claim_nothing() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().to_string_lossy().into_owned();
    record_command(
        home.path(),
        "codex exec a",
        &cwd,
        Some("lead-1"),
        None,
        1_000_000,
    )
    .unwrap();
    record_command(
        home.path(),
        "codex exec b",
        &cwd,
        Some("lead-1"),
        None,
        1_000_001,
    )
    .unwrap();
    assert!(claim(home.path(), &child("c-1", "codex", &cwd, None, 1_002_000)).is_none());
    assert_eq!(
        std::fs::read_dir(home.path().join("launches/pending"))
            .unwrap()
            .count(),
        2
    );
    // No launcher session writes nothing; a record past the window is
    // never claimed.
    assert_eq!(
        record_command(home.path(), "claude --bg x", &cwd, None, None, 1).unwrap(),
        0
    );
    record_command(
        home.path(),
        "claude --bg -n w x",
        &cwd,
        Some("l"),
        None,
        1_000_000,
    )
    .unwrap();
    let late = 1_000_000 + WINDOW_MS + 1;
    assert!(claim(home.path(), &child("c", "claude", &cwd, Some("w"), late)).is_none());
}

#[test]
fn the_first_launch_edge_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let rows = serde_json::json!([
        {"id": "x-ab12", "slug": "ab", "title": "AB", "type": "feature", "status": "idea",
         "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00"}
    ]);
    graph_store::seed_rows(&graph, rows.as_array().unwrap()).unwrap();
    assert_eq!(
        stamp_node(&graph, "x-ab12", "lead-1", Some("claude"), Some("/repo")).unwrap(),
        Stamp::Wrote
    );
    assert_eq!(
        stamp_node(&graph, "x-ab12", "lead-2", Some("codex"), None).unwrap(),
        Stamp::Kept("lead-1".into())
    );
    assert_eq!(
        stamp_node(&graph, "x-none", "lead-1", None, None).unwrap(),
        Stamp::NoNode
    );
    let back = graph_store::read_rows(&graph).unwrap();
    let row = back.iter().find(|r| r["id"] == "x-ab12").unwrap();
    assert_eq!(row["spawned_by_session"], "lead-1");
    assert_eq!(row["spawned_by_cwd"], "/repo");
}
