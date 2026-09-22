//! The two event_store copies refuse to drift apart.
//!
//! The store ships as one module inside each binary: fno never links
//! fno-agents, so a copy in each crate is the only way both answer the
//! same storage verbs. This guard makes the copy loud - edit one side
//! and the other refuses to build its tests until it matches.

#[test]
fn event_store_copies_stay_byte_identical_across_the_two_binaries() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let (mine, sibling) = if manifest.ends_with("fno-agents") {
        ("src/event_store.rs", "../fno/src/event_store.rs")
    } else {
        ("src/event_store.rs", "../fno-agents/src/event_store.rs")
    };
    let mine_text = std::fs::read_to_string(format!("{manifest}/{mine}")).unwrap();
    let sibling_text = std::fs::read_to_string(format!("{manifest}/{sibling}")).unwrap();
    assert_eq!(
        mine_text, sibling_text,
        "the two event_store copies drifted"
    );
}
