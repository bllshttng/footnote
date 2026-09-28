//! x-61f5 end-to-end: the OSC background takeover's observable wire bytes.
//! The set sequence rides the client's first output; the restore rides the
//! exit path; the kill switch removes both.

mod common;

use common::{ClientHarness, Scratch};
use std::time::Duration;

fn settle(_h: &mut ClientHarness, ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[test]
fn ground_set_and_restore_ride_the_launch_and_exit() {
    let scratch = Scratch::new("ground-set-restore");
    let mut h = ClientHarness::spawn_sized_with(&scratch, 30, 100, &[("COLORFGBG", "15;0")]);
    h.wait_prompt(20);
    settle(&mut h, 1500);

    // The superscript ground is set on launch: base, stamp fg, 16 slots.
    let raw = h.raw_output();
    assert!(
        raw.contains("\x1b]11;#141414\x1b\\"),
        "OSC 11 sets the theme base"
    );
    assert!(
        raw.contains("\x1b]10;#e8e8e8\x1b\\"),
        "OSC 10 sets the stamp fg"
    );
    assert_eq!(
        raw.matches("\x1b]4;").count(),
        16,
        "OSC 4 carries all 16 palette slots"
    );

    // The restore rides the exit path (Drop runs when the client ends).
    h.type_bytes(b"\x02d");
    let _ = h.wait_exit(15);
    settle(&mut h, 500);
    let raw = h.raw_output();
    assert!(
        raw.contains("\x1b]111\x1b\\\x1b]110\x1b\\\x1b]104\x1b\\"),
        "exit restores background, foreground and palette"
    );
}

#[test]
fn the_kill_switch_removes_both_directions() {
    let scratch = Scratch::new("ground-kill-switch");
    let config = scratch.0.join("kill-config.toml");
    std::fs::write(&config, "[mux]\npaint_background = \"false\"\n").unwrap();
    let cfg = config.to_string_lossy().to_string();
    let mut h = ClientHarness::spawn_sized_with(
        &scratch,
        30,
        100,
        &[("COLORFGBG", "15;0"), ("FNO_CONFIG", &cfg)],
    );
    h.wait_prompt(20);
    settle(&mut h, 1500);

    let raw = h.raw_output();
    assert!(
        !raw.contains("\x1b]11;"),
        "kill switch: no OSC 11 on launch"
    );
    assert!(!raw.contains("\x1b]4;"), "kill switch: no OSC 4 palette");

    h.type_bytes(b"\x02d");
    let _ = h.wait_exit(15);
    settle(&mut h, 500);
    let raw = h.raw_output();
    assert!(
        !raw.contains("\x1b]111\x1b\\"),
        "kill switch: no color restore"
    );
}
