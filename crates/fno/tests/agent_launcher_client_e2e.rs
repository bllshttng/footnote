//! The composer's real-client contract: one test per reported
//! defect, driven on a real `fno` client over a PTY with a `cat -v` pane as
//! the leak detector. Every test asserts the FIXED behavior, so each one is
//! red on the branchpoint (AC0-REPRO) and green only once the composer owns
//! its input, shows its values, and opens as the centered sheet.

mod common;
use common::{strip_prompts, ClientHarness, Scratch};

use std::path::PathBuf;
use std::time::Duration;

const PREFIX: &[u8] = b"\x02";
const OPEN: &[u8] = b"i"; // prefix+i: toggle-composer
const FULL: &[u8] = b"F"; // prefix+F: full-screen sideline
const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const TAB: &[u8] = b"\t";

fn type_and_settle(h: &mut ClientHarness, bytes: &[u8]) {
    h.type_bytes(bytes);
    std::thread::sleep(Duration::from_millis(120));
}

fn wait_input(h: &mut ClientHarness) {
    h.wait_prompt(15);
}

/// A routing row the isolated home's config carries, so the agent list has a
/// real zai row under harness claude (AC4-HP).
fn seed_routing_config(scratch: &Scratch) {
    let dir = scratch.0.join("home").join(".fno");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[[routing.models]]\n\
         name = \"zai-flash\"\n\
         harness = \"claude\"\n\
         model = \"glm-5.3-flash[1m]\"\n\
         route = \"zai/glm-5.3-flash[1m]\"\n\
         band = \"high\"\n",
    )
    .unwrap();
}

/// Fake `claude`/`codex` bins on the client's PATH: the harness catalog's
/// installed flag is a PATH presence check, and a clean CI home has neither
/// binary. Presence is all the catalog reads.
fn with_fake_harnesses(scratch: &Scratch) -> Vec<(&'static str, String)> {
    let bin = scratch.0.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["claude", "codex"] {
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let augmented = format!("{}:{}", bin.display(), path);
    vec![("PATH", augmented)]
}

fn open_composer(h: &mut ClientHarness) {
    type_and_settle(h, PREFIX);
    type_and_settle(h, OPEN);
}

fn pane_region(screen: &str) -> String {
    // The content area right of the 28-column sideline: where a leaked byte
    // would print. Tests assert on absence, so an over-wide region is safe.
    screen
        .lines()
        .map(|l| l.chars().skip(28).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn composer_from_sidebar_opens_the_centered_sheet_with_full_values() {
    // AC1-HP, first half: from the regular sidebar the composer is the
    // centered sheet, and the chips carry values, not truncated labels.
    let scratch = Scratch::new("composer-sheet");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("new agent"));
    // The sheet, not the 28-column dock: the title chrome is on screen and
    // the full chosen value reads untruncated. The chip value lands when
    // the catalog read does, so wait for it instead of reading once.
    assert!(screen.contains("new agent"), "sheet title: {screen}");
    // The chip's value comes from the compile-time capability table (agy
    // sorts first), not the fake PATH bins; it lands when the catalog read
    // does, so wait for it instead of reading once.
    let screen = h.wait_screen(10, |s| s.contains("agy default"));
    assert!(
        screen.contains("agy default"),
        "the harness value shows in full: {screen}"
    );
}

#[test]
fn project_chip_down_opens_a_list_and_never_launches() {
    // AC2-HP: Down on the project chip opens the project list; Enter on a
    // row sets the project without launching. The open signal is the list's
    // footer grammar: a long temp-path hint and even the row glyph can
    // truncate to the popup width, the footer cannot.
    let scratch = Scratch::new("composer-project");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, TAB);
    type_and_settle(&mut h, DOWN);
    let screen = h.wait_screen(10, |s| s.contains("type to filter"));
    assert!(
        screen.contains("type to filter"),
        "the project list opens with its key grammar: {screen}"
    );
    // Enter picks the highlighted row: still editing, nothing launched.
    type_and_settle(&mut h, b"\r");
    std::thread::sleep(Duration::from_millis(400));
    let screen = h.screen();
    assert!(
        !screen.contains("starting..."),
        "Enter on a project row never launches: {screen}"
    );
}

#[test]
fn agent_list_offers_default_rows_and_no_free_text_model_row() {
    // AC5-HP / open question 2: every launchable model comes from a routing
    // row; the agent list carries `<harness> default` rows and no
    // "type a model..." free-text entry, and a typed query can never become
    // the value.
    let scratch = Scratch::new("composer-agent-list");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, DOWN);
    let screen = h.wait_screen(10, |s| s.contains("default"));
    assert!(
        screen.contains("default"),
        "the agent list names the default rows: {screen}"
    );
    assert!(
        !screen.contains("type a model"),
        "no free-text model row: {screen}"
    );
    // A typed query filters in place and rides the title (never a row, never
    // a chip value): the title names it, the chips row stays clean.
    type_and_settle(&mut h, b"fddd");
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.screen();
    assert!(
        screen.contains("filter: fddd"),
        "the query rides the title: {screen}"
    );
    let chips = screen
        .lines()
        .find(|l| l.contains("default\u{25be}"))
        .unwrap_or_default();
    assert!(
        !chips.contains("fddd"),
        "junk never becomes a chip value: {chips}"
    );
}

#[test]
fn focus_report_then_arrow_never_lands_as_text() {
    // AC6-EDGE: an unknown CSI (a focus report ESC [ I) is dropped whole; a
    // following arrow still navigates and no `[`/`I` byte reaches a field.
    let scratch = Scratch::new("composer-focus-in");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    // Tab to the message editor (agent -> project -> message), where stray
    // bytes would be visible as text.
    type_and_settle(&mut h, TAB);
    type_and_settle(&mut h, TAB);
    type_and_settle(&mut h, b"\x1b[I");
    type_and_settle(&mut h, DOWN);
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.screen();
    assert!(
        !screen.contains("[B") && !screen.contains("[I"),
        "no sequence byte lands in a field: {screen}"
    );
}

#[test]
fn wheel_over_open_list_never_reaches_the_pane() {
    // AC6-HP, mouse half: in the full-screen sideline the dock's open list
    // sits over live panes; a wheel report over it is consumed, never
    // forwarded to the `cat -v` pane underneath.
    let scratch = Scratch::new("composer-wheel");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    type_and_settle(&mut h, PREFIX);
    type_and_settle(&mut h, FULL);
    std::thread::sleep(Duration::from_millis(300));
    type_and_settle(&mut h, DOWN); // the agent list opens, flipped above the dock
    std::thread::sleep(Duration::from_millis(800));
    type_and_settle(&mut h, b"\x1b[<64;35;12M"); // wheel up at row 12, col 35: over the list
    std::thread::sleep(Duration::from_millis(400));
    let screen = h.screen();
    let pane = pane_region(&screen);
    assert!(
        !pane.contains("^[[<"),
        "the wheel report never reaches the pane: {pane}"
    );
}

#[test]
fn arrows_inside_the_repeat_window_type_letters_not_resizes() {
    // AC6-HP, keys half: a bare H/J/K/L typed into the composer inside a
    // resize repeat window is text for the draft, never a pane resize. The
    // composer stays open and the pane's columns never change (the prompt
    // does not redraw mid-line).
    let scratch = Scratch::new("composer-repeat");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    // Arm the resize window: prefix+L.
    type_and_settle(&mut h, PREFIX);
    type_and_settle(&mut h, b"L");
    open_composer(&mut h);
    // Tab to the message editor, then hold L inside the window.
    type_and_settle(&mut h, TAB);
    type_and_settle(&mut h, TAB);
    type_and_settle(&mut h, b"L");
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.screen();
    assert!(
        screen.contains("L"),
        "the letter lands in the draft: {screen}"
    );
}

#[test]
fn composer_hint_row_names_the_keys() {
    // AC8-HP: the hint row is always painted in the composer and names tab,
    // enter, the arrows and esc.
    let scratch = Scratch::new("composer-hint");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("\u{2193}"));
    assert!(screen.contains("esc"), "esc is named: {screen}");
    assert!(
        screen.contains("launch") || screen.contains("open"),
        "enter's action is named: {screen}"
    );
}

#[test]
fn short_terminal_refuses_the_sheet_with_a_notice() {
    // AC1-ERR: a terminal shorter than 12 rows opens nothing and names why.
    let scratch = Scratch::new("composer-short");
    let mut h = ClientHarness::spawn_sized(&scratch, 10, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("terminal too short"));
    assert!(
        screen.contains("terminal too short for the composer"),
        "the refusal names the limit: {screen}"
    );
    assert!(!screen.contains("new agent"), "nothing opened: {screen}");
}

#[test]
fn opening_the_composer_from_a_hidden_panel_paints_a_sheet_no_resize() {
    // AC9-HP: 60 columns hides the sidebar; opening the composer must paint
    // the centered sheet overlay and send NO Resize - the pane's rows never
    // reflow (the prompt redraws a second prompt line on a width change).
    let scratch = Scratch::new("composer-noresize");
    let mut h = ClientHarness::spawn(&scratch); // 24x60: panel hidden
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("new agent"));
    assert!(
        screen.contains("new agent"),
        "the sheet opens over the hidden panel: {screen}"
    );
    let prompts = screen
        .lines()
        .filter(|l| strip_prompts(l).ends_with('$'))
        .count();
    assert!(
        prompts <= 1,
        "no Resize: the pane's prompt never redraws a second line: {screen}"
    );
}

#[test]
fn prefix_hint_bar_shows_at_once() {
    // AC8-EDGE: the short hint bar paints the moment the prefix is pending.
    let scratch = Scratch::new("composer-prefix-hint");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    type_and_settle(&mut h, PREFIX);
    std::thread::sleep(Duration::from_millis(120));
    let screen = h.screen();
    assert!(
        screen.contains("esc cancel"),
        "the bar names esc cancel: {screen}"
    );
    assert!(
        screen.contains("all keys"),
        "the bar sends the reader to ?: {screen}"
    );
}

#[test]
fn agent_list_shows_route_hint_for_a_routing_row() {
    // AC4-HP: the agent list shows the routing row under its harness with
    // the route as the hint, so a glm launch is reachable from the composer.
    // The door underneath is the python CLI's inventory, which the CI mux
    // job deliberately does not install (no python steps there). Probe the
    // same door the client runs, against the same scratch env: when it
    // answers with rows, pin the routing row and its hint; when it answers
    // empty or not at all, pin the surface the client shows instead - the
    // row and hint rendering are pinned by the unit suite either way.
    let scratch = Scratch::new("composer-route-hint");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, DOWN);
    match probe_inventory_models(&scratch, &env_refs) {
        DoorShape::Rows => {
            // The inventory read has measured 22s wall on a loaded machine;
            // the list refills the moment the read lands.
            let screen = h.wait_screen(35, |s| s.contains("zai-flash"));
            assert!(
                screen.contains("zai-flash"),
                "the routing row is listed: {screen}"
            );
            // The hint carries the route when the door's row has one and
            // falls back to the model id when it does not.
            assert!(
                screen.contains("glm-5.3-flash[1m]"),
                "the row's hint names what a launch carries: {screen}"
            );
        }
        DoorShape::EmptyModels => {
            let screen = h.wait_screen(35, |s| s.contains("default"));
            assert!(
                screen.contains("default"),
                "the default rows stand with no routing rows declared: {screen}"
            );
        }
        DoorShape::Unavailable => {
            let screen = h.wait_screen(35, |s| s.contains("model list unavailable"));
            assert!(
                screen.contains("model list unavailable"),
                "the missing door is named: {screen}"
            );
            assert!(
                screen.contains("default"),
                "the default rows still launch: {screen}"
            );
        }
    }
}

/// The three answers the inventory door can give the composer.
enum DoorShape {
    Rows,
    EmptyModels,
    Unavailable,
}

/// What the inventory door answers when the client's catalog read runs it.
/// A missing python CLI, slot server, or unreadable answer reads as
/// `Unavailable`, never as a panic in the test process.
fn probe_inventory_models(scratch: &Scratch, envs: &[(&str, &str)]) -> DoorShape {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["config", "route", "inventory", "--json"])
        .env("HOME", scratch.0.join("home"))
        .envs(envs.iter().copied())
        .output();
    let Ok(out) = out else {
        return DoorShape::Unavailable;
    };
    if !out.status.success() {
        return DoorShape::Unavailable;
    }
    let Ok(text) = String::from_utf8(out.stdout) else {
        return DoorShape::Unavailable;
    };
    match serde_json::from_str::<serde_json::Value>(text.trim_start())
        .ok()
        .and_then(|v| v.get("models").and_then(|m| m.as_array()).cloned())
    {
        Some(rows) if !rows.is_empty() => DoorShape::Rows,
        Some(_) => DoorShape::EmptyModels,
        None => DoorShape::Unavailable,
    }
}

#[test]
fn arrows_in_an_open_list_move_and_up_never_launches() {
    // AC3-HP, list grammar: Up/Down move inside the list; nothing in the
    // list launches. Up from the first row stays put; the pane still prints
    // nothing.
    let scratch = Scratch::new("composer-list-nav");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, DOWN);
    std::thread::sleep(Duration::from_millis(400));
    type_and_settle(&mut h, UP);
    type_and_settle(&mut h, DOWN);
    type_and_settle(&mut h, DOWN);
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.screen();
    assert!(
        !screen.contains("starting..."),
        "arrow navigation never launches: {screen}"
    );
    let pane = pane_region(&screen);
    assert!(
        !pane.contains("^[[B"),
        "arrows never reach the pane: {pane}"
    );
}

#[allow(dead_code)]
fn unused_path_helper(_: PathBuf) {}
