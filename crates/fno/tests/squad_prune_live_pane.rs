//! The live-pane arm of `fno mux workspace prune`, proven with a REAL pane.
//!
//! A member-less squad is the ambiguous case: mid-recruit and long-finished look
//! identical, so a live pane under one of its origins is what keeps it. That arm
//! protects work a person is still doing, and a false prune there kills it, so a
//! fixture that merely omits a field is not enough evidence that it holds. This
//! spawns the real server, attaches a real client with a real cwd, and asserts
//! the squad survives.
//!
//! The discriminator is the pane and nothing else. Both squads below are
//! member-less, both carry an origin that exists, and both are stamped far past
//! the grace window, so the only remaining reason to keep one is a pane sitting
//! under its origin. The no-pane case is the positive control: it must be
//! PRUNED. Without it, a survival assertion would pass just as happily against a
//! prune that never ran.

mod common;

use common::{spawn_server, FakeClient, Scratch};

/// A member-less squad whose origin exists and whose stamp is long past the
/// one-hour grace window. Keeps only if a live pane sits under `origin`.
fn store_json(origin: &str) -> String {
    format!(
        r#"{{"version":1,"squads":[
  {{"name":"","key":"past-grace","origins":["{origin}"],"members":[],"created_at":"2001-01-01T00:00:00Z"}}
]}}"#
    )
}

/// Seed `squads.json` where the exec'd binary reads it, and return the origin
/// dir it points at. The origin is a real directory: an origin that has vanished
/// prunes for a different reason, which would hide the arm under test.
fn seed(scratch: &Scratch) -> String {
    let agents_home = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&agents_home).unwrap();
    let origin = scratch.0.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    let origin = origin.to_str().expect("utf8 scratch path").to_string();
    std::fs::write(agents_home.join("squads.json"), store_json(&origin)).unwrap();
    origin
}

fn prune(scratch: &Scratch) -> String {
    let out = scratch
        .command()
        .args(["mux", "workspace", "prune"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(scratch.0.join("iso-agents").join("squads.json")).unwrap_or_default()
}

#[test]
fn a_squad_under_a_live_pane_survives_the_prune() {
    let scratch = Scratch::new("prune-live-pane");
    let origin = seed(&scratch);

    // A real server, and a real client attached with its cwd AT the origin. The
    // cwd is what `live_pane_cwds` reads back over `PaneLs`, so this is the same
    // path a person's attached session takes, not a synthesized cwd list.
    let sock = scratch.main_sock();
    let _server = spawn_server(&sock, &[]);
    let mut client = FakeClient::attach(&sock, 24, 80, &origin);
    // Wait for a pane to actually exist. Attaching only opens the socket; if the
    // prune ran before the server built the pane, PaneLs would report none and
    // this test would pass or fail on a race rather than on the predicate.
    client.wait_layout(10, "a pane to exist", |l| !l.panes.is_empty());

    // Assert on the ORIGIN, not the key. The store recomputes `key` on load, so
    // a seeded key is absent from the written file either way, and a control
    // keyed on it would pass whether or not the prune ran.
    let store = prune(&scratch);
    assert!(
        store.contains(&origin),
        "a squad with a live pane under its origin was pruned; store={store}"
    );
}

#[test]
fn the_same_squad_prunes_with_no_live_pane() {
    // The positive control for the test above. Same store, same past-grace
    // stamp, same existing origin, no pane. It must prune, or the survival
    // assertion proves nothing about the pane.
    let scratch = Scratch::new("prune-no-pane");
    let origin = seed(&scratch);

    let store = prune(&scratch);
    assert!(
        !store.contains(&origin),
        "the no-pane control did not prune, so the live-pane assertion is not \
         discriminating; store={store}"
    );
}
