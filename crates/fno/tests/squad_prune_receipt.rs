//! The `fno mux workspace prune` receipt must name what a run actually did,
//! and the scope flags must let one verb do either half alone.
//!
//! The headline assertions run against the real binary with a real server and
//! real panes where the behavior needs them: an apply run that closes a tab
//! used to print `nothing to prune` and to put `would close 0` beside the
//! number it had just acted on. Refusals and the dry-run mood need no server.

mod common;

use common::{spawn_server, FakeClient, Scratch};

fn now_stamp() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .expect("date");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// One grace-window squad at a real origin. `members_json` is the raw member
/// array: `[]` for the surplus-shell shape the tab arm meets, or one provably
/// dead member (the isolated home has no registry or roster) for the reap arm.
fn seed_store_with(scratch: &Scratch, members_json: &str) -> String {
    let agents_home = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&agents_home).unwrap();
    let origin = scratch.0.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    let origin = origin.to_str().expect("utf8 scratch path").to_string();
    std::fs::write(
        agents_home.join("squads.json"),
        format!(
            r#"{{"version":1,"squads":[
  {{"name":"","key":"seeded","origins":["{origin}"],"members":[{members}],"created_at":"{now}"}}
]}}"#,
            members = members_json,
            origin = origin,
            now = now_stamp(),
        ),
    )
    .unwrap();
    origin
}

fn seed_store(scratch: &Scratch) -> String {
    seed_store_with(scratch, "")
}

fn store_after(scratch: &Scratch) -> String {
    std::fs::read_to_string(scratch.0.join("iso-agents").join("squads.json")).unwrap_or_default()
}

#[test]
fn tab_close_without_a_target_names_the_bulk_verb() {
    let scratch = Scratch::new("receipt-tab-refusal");
    let out = scratch
        .command()
        .args(["mux", "tab", "close"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage refusal");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fno mux tab close: needs --tab"),
        "the refusal keeps naming the missing flag: {stderr}"
    );
    assert!(
        stderr.contains("fno mux workspace prune"),
        "the refusal must point at the bulk close verb: {stderr}"
    );
}

#[test]
fn contradictory_scope_flags_are_a_usage_error() {
    let scratch = Scratch::new("receipt-scope-xor");
    let out = scratch
        .command()
        .args(["mux", "workspace", "prune", "--tabs-only", "--dead-only"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage refusal");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tabs-only") && stderr.contains("--dead-only"),
        "the refusal names both flags: {stderr}"
    );
}

#[test]
fn dry_run_tabs_clause_says_would_close() {
    let scratch = Scratch::new("receipt-dry-mood");
    seed_store(&scratch);
    let out = scratch
        .command()
        .args(["mux", "workspace", "prune", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("tabs would close"),
        "the dry-run tabs clause carries its mood in the verb: {stdout}"
    );
    assert!(
        !stdout.contains("tabs closed"),
        "a dry-run never claims a close: {stdout}"
    );
}

/// An apply run with a real server, a live pane keeping the squad inside its
/// grace window, and one dead member record: the store arm reaps the member
/// while the squad survives and the tab arm folds nothing. That run is the
/// opposite of a no-op, so the receipt must name what it reaped and never
/// speak in the conditional - and `nothing to prune` must not print.
#[test]
fn an_apply_run_that_acted_never_prints_nothing_to_prune() {
    let scratch = Scratch::new("receipt-apply");
    let origin = seed_store_with(&scratch, r#"{"attach_id":"deadbeef","tombstone":false}"#);
    let sock = scratch.main_sock();
    let _server = spawn_server(&sock, &[("SHELL", "/bin/bash")]);
    let mut client = FakeClient::attach(&sock, 24, 80, &origin);
    client.wait_layout(10, "a pane to exist", |l| !l.panes.is_empty());
    client.wait_prompt(client.focus());

    let out = scratch
        .command()
        .args(["mux", "workspace", "prune"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("reaped 1 dead member"),
        "the acted-on count is named: {stdout}"
    );
    assert!(
        !stdout.contains("would"),
        "an apply receipt never speaks in the conditional: {stdout}"
    );
    assert!(
        !stdout.contains("nothing to prune"),
        "a run that reaped a member is not a no-op: {stdout}"
    );
}

/// Tabs-only runs the tab arm alone. With no surplus tab to fold it closes
/// nothing, and the dead member record the store arm would have reaped is
/// still exactly where the seed left it.
#[test]
fn tabs_only_scope_leaves_the_store_alone() {
    let scratch = Scratch::new("receipt-tabs-only");
    let origin = seed_store_with(&scratch, r#"{"attach_id":"deadbeef","tombstone":false}"#);
    let sock = scratch.main_sock();
    let _server = spawn_server(&sock, &[("SHELL", "/bin/bash")]);
    let mut client = FakeClient::attach(&sock, 24, 80, &origin);
    client.wait_layout(10, "a pane to exist", |l| !l.panes.is_empty());
    client.wait_prompt(client.focus());

    let out = scratch
        .command()
        .args(["mux", "workspace", "prune", "--tabs-only", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json emits JSON");
    assert_eq!(
        receipt["tabs_closed"], 0,
        "a workspace's last tab is never surplus: {receipt}"
    );
    assert_eq!(
        receipt["members_reaped"], 0,
        "tabs-only must not touch member records: {receipt}"
    );
    let after = store_after(&scratch);
    assert!(
        after.contains("deadbeef"),
        "the dead member record survives a tabs-only run: {after}"
    );
    assert!(after.contains(&origin), "the squad row survives: {after}");
}

/// Dead-only runs the store arm against a live server but touches no tab: the
/// seeded dead member is reaped from a squad row that survives.
#[test]
fn dead_only_scope_reaps_members_and_leaves_tabs_alone() {
    let scratch = Scratch::new("receipt-dead-only");
    let origin = seed_store_with(&scratch, r#"{"attach_id":"deadbeef","tombstone":false}"#);
    let sock = scratch.main_sock();
    let _server = spawn_server(&sock, &[("SHELL", "/bin/bash")]);

    let out = scratch
        .command()
        .args(["mux", "workspace", "prune", "--dead-only", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json emits JSON");
    assert_eq!(receipt["members_reaped"], 1, "{receipt}");
    assert_eq!(
        receipt["tabs_closed"], 0,
        "dead-only never closes tabs: {receipt}"
    );
    assert_eq!(
        receipt["pruned_count"], 0,
        "dead-only never removes a squad row: {receipt}"
    );
    let after = store_after(&scratch);
    assert!(
        !after.contains("deadbeef"),
        "the dead member was reaped: {after}"
    );
    assert!(after.contains(&origin), "the squad row survives: {after}");
}
