//! `config.mux.prefix` + `[mux.keys]` end to end: a real config.toml, the real
//! `fno` binary, a real PTY.
//!
//! The unit tests in `keys.rs` cover the resolver, which is pure. They cannot
//! cover the part that actually breaks: whether a key written in a config file
//! reaches the scanner at all. That chain runs config.toml -> `digest_overlay`
//! -> `keys::install` -> `Scanner`, entirely inside the spawned binary, so only
//! a launched client can prove it - and a rebind that resolves perfectly but is
//! installed after the first keystroke is indistinguishable from no rebind.
//!
//! Detach is the observable: it is the one chord whose effect is a process
//! exit, which needs no screen scraping to see.

mod common;

use std::time::Duration;

use common::{ClientHarness, Scratch};

/// Write a config.toml inside the scratch and return the path, for `FNO_CONFIG`.
///
/// Pinned explicitly rather than dropped beside the isolated global settings:
/// the reader checks `<cwd>/.fno/config.toml` FIRST, and the cwd here is the
/// developer's checkout, which on a set-up worktree has one. Without the pin
/// this test would read whoever ran it.
fn write_config(scratch: &Scratch, body: &str) -> String {
    let path = scratch.0.join("keymap-config.toml");
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn keymap_e2e_config_moves_the_prefix_and_the_chord() {
    let scratch = Scratch::new("keymap");
    let cfg = write_config(
        &scratch,
        "[mux]\nprefix = \"C-a\"\n\n[mux.keys]\ndetach = \"Q\"\n",
    );
    let mut h = ClientHarness::spawn_with(&scratch, &[("FNO_CONFIG", cfg.as_str())]);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.wait_input_ready(20);

    // The OLD chord is dead. Ctrl-b is now an ordinary byte, so `\x02d` reaches
    // the shell as input instead of detaching - the `d` echoes onto the prompt
    // line, which is also proof the bytes were forwarded rather than swallowed.
    h.type_bytes(b"\x02d");
    // Scan every line, not the last few: the prompt sits near the TOP of a
    // fresh session with blank rows and the status row below it.
    h.wait_screen(15, |s| s.lines().any(|l| l.trim().ends_with("$ d")));
    assert!(
        h.child.try_wait().unwrap().is_none(),
        "prefix+d must NOT detach once the prefix has moved to C-a"
    );

    // Clear the half-typed line so the shell is not left mid-command.
    h.type_bytes(&[0x15]); // Ctrl-U
    std::thread::sleep(Duration::from_millis(200));

    // The NEW chord works: C-a then the rebound detach key.
    h.type_bytes(b"\x01Q");
    let status = h.wait_exit(10);
    assert!(
        status.success(),
        "C-a + Q must detach and exit 0, got {status:?}"
    );
}

#[test]
fn keymap_e2e_a_refused_rebind_keeps_the_shipped_key() {
    // Fail-safe, the property that matters most here: a config that names a key
    // already taken must leave a WORKING keyboard, not a half-applied one. `c`
    // is already new-tab, so `detach = "c"` is refused and prefix+d still
    // detaches. A rebind that silently shadowed new-tab would be worse than one
    // that did nothing.
    let scratch = Scratch::new("keymap-refuse");
    let cfg = write_config(&scratch, "[mux.keys]\ndetach = \"c\"\n");
    let mut h = ClientHarness::spawn_with(&scratch, &[("FNO_CONFIG", cfg.as_str())]);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.wait_input_ready(20);
    h.type_bytes(b"\x02d");
    let status = h.wait_exit(10);
    assert!(
        status.success(),
        "the shipped prefix+d must survive a refused rebind, got {status:?}"
    );
}
