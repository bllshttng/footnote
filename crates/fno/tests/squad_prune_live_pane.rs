//! The live-pane arm of `fno mux workspace prune`, proven with a REAL pane.
//!
//! A member-less squad is the ambiguous case: mid-recruit and long-finished look
//! identical, so a live pane under one of its origins is what keeps it. That arm
//! protects work a person is still doing, and a false prune there kills it, so a
//! fixture that merely omits a field is not enough evidence that it holds. This
//! spawns the real server, attaches a real client with a real cwd, and reads the
//! decision back out of the receipt.
//!
//! Both directory arms now expire on one clock. Within grace they keep for
//! DIFFERENT reasons, and the receipt separates them: a pane is positive
//! protection (`kept_protected`), a merely-surviving origin is an admission of
//! ignorance (`kept_unknown`). Survival alone therefore proves nothing here, and
//! the counter is the discriminator. Past grace the pane stops counting, which
//! is the assertion that changed.

mod common;

use common::{spawn_server, FakeClient, Scratch};

/// `YYYY-MM-DDThh:mm:ssZ` for now, so a fixture can land INSIDE the one-hour
/// grace window against a wall clock the CLI reads for itself. The verb takes no
/// clock injection, so the stamp has to be real.
fn now_stamp() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .expect("date");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Seed `squads.json` where the exec'd binary reads it. Returns the origin dir,
/// which is real: an origin that has vanished prunes for a different reason and
/// would hide the arm under test.
fn seed(scratch: &Scratch, created_at: &str) -> String {
    let agents_home = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&agents_home).unwrap();
    let origin = scratch.0.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    let origin = origin.to_str().expect("utf8 scratch path").to_string();
    std::fs::write(
        agents_home.join("squads.json"),
        format!(
            r#"{{"version":1,"squads":[
  {{"name":"","key":"seeded","origins":["{origin}"],"members":[],"created_at":"{created_at}"}}
]}}"#
        ),
    )
    .unwrap();
    origin
}

/// `(receipt_json, store_after)`.
fn prune(scratch: &Scratch) -> (serde_json::Value, String) {
    let out = scratch
        .command()
        .args(["mux", "workspace", "prune", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json emits JSON");
    let store = std::fs::read_to_string(scratch.0.join("iso-agents").join("squads.json"))
        .unwrap_or_default();
    (receipt, store)
}

/// A real server with a real client attached at `cwd`. That cwd is what
/// `live_pane_cwds` reads back over `PaneLs`, so this is the path a person's
/// attached session takes, not a synthesized cwd list.
fn attach_pane_at(scratch: &Scratch, cwd: &str) -> (common::ServerProc, FakeClient) {
    let sock = scratch.main_sock();
    let server = spawn_server(&sock, &[]);
    let mut client = FakeClient::attach(&sock, 24, 80, cwd);
    // Attaching only opens the socket. Without waiting for a pane to exist, a
    // prune that ran first would see none and this would turn on a race.
    client.wait_layout(10, "a pane to exist", |l| !l.panes.is_empty());
    (server, client)
}

#[test]
fn a_live_pane_protects_a_squad_inside_the_grace_window() {
    let scratch = Scratch::new("prune-live-pane");
    let origin = seed(&scratch, &now_stamp());
    let _pane = attach_pane_at(&scratch, &origin);

    let (receipt, store) = prune(&scratch);
    assert!(
        store.contains(&origin),
        "a squad with a live pane under its origin was pruned; store={store}"
    );
    // Assert on the ORIGIN and the counter, never the seeded key: the store
    // recomputes `key` on load, so a control keyed on it passes whether or not
    // the prune ran at all.
    assert_eq!(
        receipt["kept_protected"], 1,
        "the pane must be the reason it survived, not a surviving origin; {receipt}"
    );
    assert_eq!(receipt["kept_unknown"], 0, "{receipt}");
}

#[test]
fn the_same_squad_without_a_pane_is_only_kept_as_unknown() {
    // The control. Same fixture, same window, no pane. It survives too, so
    // survival is not the discriminator - the REASON is. Kept here on a
    // surviving origin alone, which the receipt is careful to call unknown
    // rather than protected.
    let scratch = Scratch::new("prune-no-pane");
    let origin = seed(&scratch, &now_stamp());

    let (receipt, store) = prune(&scratch);
    assert!(store.contains(&origin), "store={store}");
    assert_eq!(
        receipt["kept_unknown"], 1,
        "with no pane the keep must be an admission of ignorance; {receipt}"
    );
    assert_eq!(receipt["kept_protected"], 0, "{receipt}");
}

#[test]
fn past_the_grace_window_a_live_pane_stops_protecting() {
    // The behaviour this PR changed. A pane in a repo is a coincidence of
    // directory, not evidence about a squad nobody has recruited into for
    // weeks, so the pane arm expires on the same clock as the origin arm.
    let scratch = Scratch::new("prune-stale-pane");
    let origin = seed(&scratch, "2001-01-01T00:00:00Z");
    let _pane = attach_pane_at(&scratch, &origin);

    let (receipt, store) = prune(&scratch);
    assert!(
        !store.contains(&origin),
        "a live pane still held a squad decades past grace; store={store}"
    );
    assert_eq!(receipt["pruned_count"], 1, "{receipt}");
    // The removal names why, so a prune cannot quietly erase the evidence that
    // nothing ever recorded a member for this squad.
    assert!(
        receipt["pruned"][0]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("no member ever recorded"),
        "the receipt must name the reason; {receipt}"
    );
}
