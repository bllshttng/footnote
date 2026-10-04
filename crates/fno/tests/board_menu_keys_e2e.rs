//! end to end: with the backlog board docked in the sideline, the
//! prefix-chord menu keys still reach their surfaces. The real `fno` client
//! runs on a portable-pty and the harness plays the human (see
//! `client_e2e.rs`); the assertion is exactly the screen a person sees.
//!
//! Before the fix the board's key router swallowed every raw byte: `^B V`
//! never cycled the sideline view and `^B ?` armed the board's own keys
//! overlay instead of the global keybinds, so the org fold could not be
//! collapsed while the board held the sideline.

mod common;

use common::{ClientHarness, Scratch};

const PREFIX: u8 = 0x02; // Ctrl-B, the default mux prefix

#[test]
fn board_menu_keys_reach_their_surfaces_while_the_board_is_docked() {
    let scratch = Scratch::new("boardmenukeys");
    // The experimental pref so `^B O` opens the board directly.
    let iso_agents = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&iso_agents).unwrap();
    std::fs::write(
        iso_agents.join("mux-view.json"),
        r#"{"experimental_backlog_view": true}"#,
    )
    .unwrap();

    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.wait_input_ready(20);
    // The org fold is open before the board arrives, the shape the bug was
    // seen live in: the fold's expanded state must not pin anything the
    // board paints.
    h.type_bytes(&[PREFIX, b'C']);
    let _ = h.screen();

    // The board docks into the sideline column.
    h.type_bytes(&[PREFIX, b'O']);
    h.wait_screen(15, |s| s.contains("filters"));

    // leg 1: `^B V` (cycle sideline view) still resolves while the
    // board holds the keyboard. Org is the next view from the board.
    h.type_bytes(&[PREFIX, b'V']);
    eprintln!("STAGE leg1 org-after-V");
    h.wait_screen(15, |s| s.contains("Tree │ Table │ Graph"));

    h.type_bytes(&[PREFIX, b'?']);
    eprintln!("STAGE org global-keybinds");
    let screen = h.wait_screen(15, |s| s.contains("Keybindings"));
    assert!(
        !screen.contains("Org keys"),
        "Org's own keys must not answer the prefix chord:\n{screen}"
    );
    h.type_bytes(&[27]);
    h.wait_screen(15, |s| {
        s.contains("Tree │ Table │ Graph") && !s.contains("Keybindings")
    });

    h.type_bytes(&[PREFIX, b'V']);
    eprintln!("STAGE leg1 agents-after-second-V");
    h.wait_screen(15, |s| s.contains("+ new workspace"));

    h.type_bytes(&[PREFIX, b'?']);
    eprintln!("STAGE agents global-keybinds");
    h.wait_screen(15, |s| s.contains("Keybindings"));
    h.type_bytes(&[27]);
    h.wait_screen(15, |s| {
        s.contains("+ new workspace") && !s.contains("Keybindings")
    });

    // The cycle now runs through Messages (the new view after Agents) ...
    h.type_bytes(&[PREFIX, b'V']);
    eprintln!("STAGE roundtrip messages-after-third-V");
    h.wait_screen(15, |s| s.contains("Partners"));

    // ... and the chord round-trips the view back to the board.
    h.type_bytes(&[PREFIX, b'V']);
    eprintln!("STAGE roundtrip board-again");
    h.wait_screen(15, |s| s.contains("filters"));

    // leg 2: `^B ?` opens the GLOBAL keybinds menu, not the board's
    // own keys overlay (its `?` verb, reachable with the bare key).
    h.type_bytes(&[PREFIX, b'?']);
    eprintln!("STAGE leg2 keybinds-after-^B?");
    let screen = h.wait_screen(15, |s| s.contains("Keybindings"));
    assert!(
        !screen.contains("backlog keys"),
        "the board's keys overlay must not answer the prefix chord:\n{screen}"
    );
}
