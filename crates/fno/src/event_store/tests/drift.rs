//! The two event_store copies refuse to drift apart.
//!
//! The store ships as one module inside each binary: fno never links
//! fno-agents, so a copy in each crate is the only way both answer the
//! same storage verbs. This guard makes the copy loud - edit one side
//! and the other refuses to build its tests until it matches. Every
//! twin file the store ships (the module, its observation submodule,
//! and the observation test module) is compared, so a new file cannot
//! quietly escape the guard the way a line edit cannot.

#[test]
fn event_store_copies_stay_byte_identical_across_the_two_binaries() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let sibling_root = if manifest.ends_with("fno-agents") {
        "../fno"
    } else {
        "../fno-agents"
    };
    const TWIN_FILES: &[&str] = &[
        "src/event_store.rs",
        "src/event_store/observation.rs",
        "src/event_store/tests/observation.rs",
    ];
    for file in TWIN_FILES {
        let mine_text = std::fs::read_to_string(format!("{manifest}/{file}")).unwrap();
        let sibling_text = std::fs::read_to_string(format!("{sibling_root}/{file}")).unwrap();
        assert_eq!(
            mine_text, sibling_text,
            "the two event_store copies drifted: {file}"
        );
    }
}
