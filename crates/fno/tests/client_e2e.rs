//! Client e2e tests (task 1.3): the real `fno` client runs under a
//! portable-pty from this harness - a true TTY, so bare `fno` role-selects to
//! the client, spawns its server, attaches, and draws. The harness plays the
//! human: it types into the PTY master and reads the client's rendered output
//! through our own VT emulator (`fno::vt::Pane`), i.e. it asserts on exactly
//! the screen a person would see.

mod common;

use std::time::Duration;

use common::{
    line_ends_with_prompt, line_is_segment, screen_has_line, strip_prompts, ClientHarness, Scratch,
};

#[test]
fn client_e2e_prompt_appears_and_echo_roundtrips() {
    // AC1-HP + AC2-HP: bare `fno` on a TTY comes up with a shell, and typed
    // input round-trips to rendered output. (The 500ms latency target is not
    // asserted - CI wall-clock is not a fairness court; presence is.)
    let scratch = Scratch::new("echo");
    let mut h = ClientHarness::spawn(&scratch);
    // A prompt renders.
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"echo he\"ll\"o\r");
    // Only the OUTPUT line is bare "hello" (the typed line has quotes).
    h.wait_screen(15, |s| screen_has_line(s, "hello"));
    // The shell draws its NEXT prompt after that output line, so a frame taken
    // the instant `hello` lands can sit between the two and find no prompt row
    // at all. Wait for a `$` row BELOW the output; without this the assertions
    // below race and fail only under parallel load.
    h.wait_screen(15, |s| {
        let lines: Vec<&str> = s.lines().collect();
        lines
            .iter()
            .position(|l| line_is_segment(l, "hello"))
            .is_some_and(|i| lines[i + 1..].iter().any(|l| line_ends_with_prompt(l)))
    });
    // AC1-UI: the cursor is visible and sits on the fresh prompt row, where
    // the shell put it. That row is the last screen line ending with `PS1`
    // (`$ `) - NOT necessarily the last screen line, since the always-on
    // status row (US4) is chrome that renders below the content prompt.
    let frame = h.pane.frame();
    assert!(frame.cursor_visible, "cursor must be visible at the prompt");
    let text = h.screen();
    let lines: Vec<&str> = text.lines().collect();
    let prompt_row = lines
        .iter()
        .rposition(|l| line_ends_with_prompt(l))
        .unwrap_or_else(|| panic!("no prompt row found; screen:\n{text}"));
    assert_eq!(
        frame.cursor_row as usize, prompt_row,
        "cursor should sit on the prompt row; screen:\n{text}"
    );
}

#[test]
fn client_e2e_utf8_and_control_keys_pass_through() {
    // AC2-UI: UTF-8 renders; Ctrl-C interrupts a foreground command. Both are
    // raw-byte passthrough - nothing re-encodes the input.
    let scratch = Scratch::new("bytes");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes("echo caf\u{00e9}\r".as_bytes());
    h.wait_screen(15, |s| screen_has_line(s, "caf\u{00e9}"));
    // Ctrl-C a sleep; the shell survives and answers again. The start marker
    // proves sleep is actually FOREGROUND-RUNNING before the ^C (a bare delay
    // could let ^C hit the prompt and the test pass without exercising it).
    h.type_bytes(b"echo start-sleep; sleep 100\r");
    h.wait_screen(15, |s| screen_has_line(s, "start-sleep"));
    // ^C, then wait for the shell to regain the foreground before typing:
    // bytes sent while sleep is still dying can be flushed by the line
    // discipline.
    h.type_bytes(&[0x03]);
    h.wait_prompt(15);
    h.type_bytes(b"echo interrupted\r");
    h.wait_screen(15, |s| screen_has_line(s, "interrupted"));
}

#[test]
fn client_e2e_output_flood_keeps_the_real_client_responsive() {
    // AC2-EDGE through the REAL client (the server-side flood test uses a
    // fake client): a large burst must not wedge the client's compositor or
    // its input path - a command typed after the flood still round-trips.
    let scratch = Scratch::new("flood");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(
        b"i=0; while [ \"$i\" -lt 5000 ]; do echo y; i=$((i + 1)); done; echo E2E-FLOOD-DONE\r",
    );
    h.wait_screen(30, |s| {
        let mut saw_done = false;
        for line in s.lines() {
            if line_is_segment(line, "E2E-FLOOD-DONE") {
                saw_done = true;
            } else if saw_done && line_ends_with_prompt(line) {
                return true;
            }
        }
        false
    });
    h.type_bytes(b"echo client-alive\r");
    h.wait_screen(15, |s| screen_has_line(s, "client-alive"));
}

#[test]
fn client_e2e_ctrl_backslash_reaches_the_child_as_sigquit() {
    // AC5-UI second half (Phase 3): raw Ctrl-\ is no longer a detach - it
    // forwards to the pane and the CHILD observes the SIGQUIT. Proven by a
    // foreground job trapping QUIT; the client must still be attached after.
    let scratch = Scratch::new("sigquit");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    // The trap exits the job so the shell prompts again deterministically
    // (keeping it alive makes prompt-vs-trap output ordering racy).
    h.type_bytes(b"sh -c \"trap 'echo GOTQUIT#; exit 0' QUIT; while :; do sleep 0.2; done\"\r");
    std::thread::sleep(Duration::from_millis(400));
    h.type_bytes(&[0x1C]); // Ctrl-\ : plain byte now, SIGQUIT to the fg job
    h.wait_screen(15, |s| s.contains("GOTQUIT#"));
    assert!(
        h.child.try_wait().unwrap().is_none(),
        "Ctrl-\\ must NOT detach the client anymore (Locked 11)"
    );
    std::thread::sleep(Duration::from_millis(400));
    h.type_bytes(b"echo still-here#\r");
    h.wait_screen(15, |s| s.contains("still-here#"));
}

#[test]
fn client_e2e_detach_exits_client_and_leaves_server_running() {
    // The Ctrl-\ detach: client exits 0, and the session (server + shell)
    // stays alive - proven by a fresh client reattaching and seeing state
    // from before the detach. (Full persistence torture is task 1.4.)
    let scratch = Scratch::new("detach");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"BEFORE_DETACH=yes; echo detach-ready=$BEFORE_DETACH\r");
    h.wait_screen(15, |s| screen_has_line(s, "detach-ready=yes"));
    h.type_bytes(b"\x02d"); // prefix+d -> detach (Locked 11)
    let status = h.wait_exit(10);
    assert!(status.success(), "detach must exit 0, got {status:?}");
    drop(h);

    // Reattach with a new client on the same session.
    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| !s.trim().is_empty());
    h2.type_bytes(b"echo var=$BEFORE_DETACH\r");
    h2.wait_screen(15, |s| screen_has_line(s, "var=yes"));
}

#[test]
fn launcher_one_esc_closes_the_sheet() {
    // R1 (x-5026): one Esc press must close the composer. The chord
    // scanner already holds the lone byte and flushes it after the 40ms quiet
    // window, so a trailing lone ESC at the end of a launcher chunk is always
    // a bare Esc press - the rule pick_keys_from_read already applies.
    //
    // Every screen match here is `contains`, never line-exact: at 120 columns
    // the sideline is visible and paints the pane title onto the same rows as
    // the shell output, salting every line. And each marker is spelled so the
    // tty ECHO of the typed command cannot contain it - only the pane's
    // OUTPUT can - so `contains` still proves a round trip.
    let scratch = Scratch::new("esc-sheet");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    h.wait_screen(15, |s| !s.trim().is_empty());
    // The input path must forward bytes before the composer chord means
    // anything. `printf 'read%s' y-marker` prints `ready-marker`; the echoed
    // command carries neither half joined.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        h.type_bytes(b"printf 'read%s' y-marker\r");
        let attempt = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < attempt {
            if h.screen().contains("ready-marker") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if h.screen().contains("ready-marker") {
            break;
        }
    }
    assert!(
        h.screen().contains("ready-marker"),
        "client input never became ready\n{}",
        h.diagnostics()
    );
    // prefix+i opens the composer. The open marker is the sheet title:
    // it paints only while the sheet is up.
    let sheet_open = |s: &str| s.contains("new agent");
    h.type_bytes(b"\x02i");
    h.wait_screen(15, |s| sheet_open(s));
    let before = h.screen();
    // The PR's rendered evidence: R1_DUMP=1 prints the opened sheet's screen
    // (the after render; the before render is the recorded main failure) and
    // its type-to-filter body. The sheet has no intermediate popover layer,
    // so the lone-Esc proof below stays one Esc whatever the body shows.
    if std::env::var("R1_DUMP").is_ok() {
        eprintln!("--- x-5026 sheet render (open, after) ---\n{before}");
        // The catalog read is bounded at 30s; a short settle keeps the
        // dump's rows real instead of the `reading...` placeholder.
        std::thread::sleep(Duration::from_secs(2));
        h.type_bytes(b"co");
        std::thread::sleep(Duration::from_millis(500));
        eprintln!("--- x-5026 sheet body (filter: co) ---\n{}", h.screen());
    }
    // Exactly one Esc byte, then silence.
    h.type_bytes(&[0x1b]);
    std::thread::sleep(Duration::from_millis(500));
    let after = h.screen();
    assert!(
        !sheet_open(&after),
        "one Esc must close the sheet; screen still shows it:\n{after}\n--- screen before Esc ---\n{before}"
    );
    // The next key reaches the shell, not the composer (which would swallow
    // it as its close key). Quoting splits the marker in the echo.
    h.type_bytes(b"echo after-\"esc\"\r");
    h.wait_screen(15, |s| s.contains("after-esc"));
}

#[test]
fn output_line_matcher_survives_a_late_prompt() {
    // The 2026-09-10 screen (main run 34438652586, persistence_kill_nine):
    // CR nudges queued while the shell was still starting printed their
    // prompts after the next command's echo, so the output rendered as
    // `$ set-ok` and the exact-trim predicate waited out its deadline on a
    // screen that was already correct.
    let fixture = "$ $ $ $ $\nSURVIVED=kill9; echo set-ok\n$ set-ok\n$";
    assert!(screen_has_line(fixture, "set-ok"));
    // A command ECHO is never the output line, prompts stripped or not.
    assert!(!screen_has_line("$ echo he\"ll\"o", "hello"));
    assert!(!screen_has_line("$ echo var=$SURVIVED", "var=kill9"));
    // macOS bash-as-sh prompts strip too; a bare prompt row strips to "".
    assert_eq!(strip_prompts("sh-3.2$ hello"), "hello");
    assert_eq!(strip_prompts("$"), "");
}

#[test]
fn output_line_guard_finds_no_exact_trim_match() {
    // Retires the x-cd8d class: a pane/screen row compared for exact
    // trim-equality against a literal misses output that already rendered
    // when a queued CR nudge prints its prompt onto the line. Port any hit
    // to common::screen_has_line or common::strip_prompts. The needles are
    // concat!'d so this file never holds the joined text and cannot match
    // itself.
    let needle_a: &str = concat!(".trim()", " ==");
    let needle_b: &str = concat!(".trim_end()", " ==");
    // Positive control: each needle must match a line built at runtime, so
    // a mistyped needle cannot green an empty scan.
    for needle in [needle_a, needle_b] {
        let line = format!("if l{} \"x\" {{", needle);
        assert!(
            line.contains(needle),
            "needle {needle:?} failed its own positive control"
        );
    }
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files_read = 0;
    let mut hits: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).expect("tests dir must be readable") {
        let path = entry.expect("dir entry readable").path();
        if path.extension() != Some(std::ffi::OsStr::new("rs")) || !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("test source readable");
        files_read += 1;
        for (i, line) in text.lines().enumerate() {
            if line.contains(needle_a) || line.contains(needle_b) {
                hits.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        files_read >= 20,
        "guard read only {files_read} files under {}; a partial scan must not pass",
        tests_dir.display()
    );
    assert!(
        hits.is_empty(),
        "exact trim-equality pane/screen matches must move to \
         common::screen_has_line / common::strip_prompts; hits:\n{}",
        hits.join("\n")
    );
}

/// The shared R-shape: one lone ESC byte after an open overlay, then quiet
/// past the 40ms flush window, then the overlay's marker gone and the next
/// command reaching the shell.
fn assert_overlay_closes_on_lone_esc(h: &mut ClientHarness, chord: &[u8], marker: &'static str) {
    h.type_bytes(chord);
    h.wait_screen(15, |s| s.contains(marker));
    let before = h.screen();
    h.type_bytes(&[0x1b]);
    std::thread::sleep(Duration::from_millis(500));
    let after = h.screen();
    assert!(
        !after.contains(marker),
        "one Esc must close it; {marker:?} still on screen:\n{after}\n--- screen before Esc ---\n{before}"
    );
    // The keyboard returned: the next command runs in the shell.
    h.type_bytes(b"echo after-\"esc\"\r");
    h.wait_screen(15, |s| s.contains("after-esc"));
}

/// Input readiness at 24x120. `wait_input_ready` matches the round-trip
/// line exactly, and at this width the sideline paints its border glyph
/// onto the pane's rows, salting every line (the launcher test's own
/// caveat). The marker is spelled split so the tty ECHO of the typed
/// command carries neither half joined; only the pane's OUTPUT does.
fn wait_ready_split_marker(h: &mut ClientHarness) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        h.type_bytes(b"printf 'fno-input-%s' ready\r");
        let attempt = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < attempt {
            if h.screen().contains("fno-input-ready") {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if h.screen().contains("fno-input-ready") {
            return;
        }
    }
    panic!("client input never became ready\n{}", h.diagnostics());
}

#[test]
fn a_lone_esc_closes_the_which_key_table() {
    // R1: a raw-fed overlay stalls on a lone Esc until the next key
    // on main; the client's quiet-window flush releases it. The marker is
    // the `a` binding's label: the global (no prefix) section pushed the
    // `?` row below the 24-row fold at scroll 0, so the old marker
    // ("this key table") no longer renders at this height.
    let scratch = Scratch::new("esc-which-key");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    h.wait_screen(15, |s| !s.trim().is_empty());
    wait_ready_split_marker(&mut h);
    assert_overlay_closes_on_lone_esc(&mut h, b"\x02?", "answer queue");
}

#[test]
fn a_lone_esc_closes_the_search_input() {
    // R2: the search input line (` /_`) is gone after one Esc and
    // one quiet window, with no second key.
    let scratch = Scratch::new("esc-search");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    h.wait_screen(15, |s| !s.trim().is_empty());
    wait_ready_split_marker(&mut h);
    assert_overlay_closes_on_lone_esc(&mut h, b"\x02/", " /_");
}

#[test]
fn a_lone_esc_closes_the_navigator() {
    // R3: the navigator's `find` line is gone after one Esc and
    // one quiet window.
    let scratch = Scratch::new("esc-navigator");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    h.wait_screen(15, |s| !s.trim().is_empty());
    wait_ready_split_marker(&mut h);
    assert_overlay_closes_on_lone_esc(&mut h, b"\x02f", " find \u{203a} ");
}

#[test]
fn a_lone_esc_closes_the_row_selector() {
    // R4: the selector paints no text of its own, so the proof is
    // behavioral. On the stalled build the lone ESC sits in the selector's
    // carry: the next `q` resolves it (Esc close, q swallowed) and nothing
    // reaches the shell. With the flush the selector closed in the quiet
    // window, so `q` runs in the shell and sh answers "not found".
    let scratch = Scratch::new("esc-selector");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    h.wait_screen(15, |s| !s.trim().is_empty());
    wait_ready_split_marker(&mut h);
    h.type_bytes(b"\x02w");
    std::thread::sleep(Duration::from_millis(200));
    h.type_bytes(&[0x1b]);
    std::thread::sleep(Duration::from_millis(500));
    h.type_bytes(b"q\r");
    h.wait_screen(15, |s| s.contains("not found"));
}
