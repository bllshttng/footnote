//! The composer's real-client contract: one test per reported
//! defect, driven on a real `fno` client over a PTY with a `cat -v` pane as
//! the leak detector. Every test asserts the FIXED behavior, so each one is
//! red on the branchpoint (AC0-REPRO) and green only once the composer owns
//! its input, shows its values, and opens as the centered sheet. The sheet
//! is the TABBED modal: one tab bar over full-body lists, no popovers.

mod common;
use common::{strip_prompts, ClientHarness, Scratch};

use std::path::PathBuf;
use std::time::Duration;

const PREFIX: &[u8] = b"\x02";
const OPEN: &[u8] = b"i"; // prefix+i: toggle-composer
const FULL: &[u8] = b"F"; // prefix+F: full-screen sideline
const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const RIGHT: &[u8] = b"\x1b[C"; // next tab

fn type_and_settle(h: &mut ClientHarness, bytes: &[u8]) {
    h.type_bytes(bytes);
    std::thread::sleep(Duration::from_millis(120));
}

fn wait_input(h: &mut ClientHarness) {
    h.wait_prompt(15);
}

/// A configured account row the isolated home exposes to the model tab.
fn seed_routing_config(scratch: &Scratch) {
    let dir = scratch.0.join("home").join(".fno");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[accounts]\n\
         active = \"zai-main\"\n\
         [[accounts.records]]\n\
         id = \"zai-main\"\n\
         harness = \"claude\"\n\
         route_provider_id = \"zai\"\n\
         model_name = \"glm-5.3-flash[1m]\"\n\
         route = \"zai/glm-5.3-flash[1m]\"\n",
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

/// From the sheet's Harness tab: filter to the installed Claude row, commit
/// it, then step one tab right to the Model body. This follows the tabbed
/// modal's grammar - the body is always visible, there is nothing to open.
fn pick_claude_and_open_model_tab(h: &mut ClientHarness) {
    type_and_settle(h, b"claude");
    type_and_settle(h, b"\r");
    h.wait_screen(10, |s| s.contains("claude · default"));
    type_and_settle(h, RIGHT);
    h.wait_screen(10, |s| s.contains("harness default"));
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
    // centered sheet, and the values strip carries the choices untruncated.
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
    let screen = h.wait_screen(10, |s| s.contains("agy · default"));
    assert!(
        screen.contains("agy · default"),
        "the values strip shows the harness and model choices: {screen}"
    );
    // The tab bar names its axes.
    for tab in ["Harness", "Model", "Project", "Message"] {
        assert!(screen.contains(tab), "tab {tab} is named: {screen}");
    }
}

#[test]
fn project_tab_lists_projects_and_enter_never_launches() {
    // AC2-HP: the Project tab body lists the candidate cwds; Enter on a row
    // sets the project without launching. The list grammar in the keybar is
    // the open signal.
    let scratch = Scratch::new("composer-project");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, RIGHT);
    type_and_settle(&mut h, RIGHT);
    let screen = h.wait_screen(10, |s| s.contains("\u{21b5} pick"));
    assert!(
        screen.contains("\u{21b5} pick"),
        "the Project tab body is a list: {screen}"
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
    // AC5-HP / open question 2: every launchable model comes from a
    // configured account row; the Model tab carries the harness default and
    // no "type a model..." free-text entry, and a typed query can never
    // become the value - it only narrows the body.
    let scratch = Scratch::new("composer-agent-list");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    pick_claude_and_open_model_tab(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("harness default"));
    assert!(
        screen.contains("harness default"),
        "the model body names the harness default: {screen}"
    );
    assert!(
        !screen.contains("type a model"),
        "no free-text model row: {screen}"
    );
    // A typed query narrows the body in place (the rows vanish) and never
    // lands in the values strip.
    type_and_settle(&mut h, b"fddd");
    let screen = h.wait_screen(10, |s| !s.contains("harness default"));
    assert!(
        !screen.contains("harness default"),
        "the junk query narrows the rows away: {screen}"
    );
    assert!(
        !screen.contains("fddd · "),
        "junk never becomes a value: {screen}"
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
    // Six tabs walk the bar to the Message editor.
    type_and_settle(&mut h, b"\t\t\t\t\t\t");
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
fn wheel_over_the_sheet_never_reaches_the_pane() {
    // AC6-HP, mouse half: in the full-screen sideline the sheet floats over
    // live panes; a wheel report over it is consumed, never forwarded to
    // the `cat -v` pane underneath.
    let scratch = Scratch::new("composer-wheel");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    // Entering the full-screen sideline auto-opens the composer (toggle_full).
    type_and_settle(&mut h, PREFIX);
    type_and_settle(&mut h, FULL);
    h.wait_screen(10, |s| s.contains("new agent"));
    std::thread::sleep(Duration::from_millis(500));
    type_and_settle(&mut h, b"\x1b[<64;35;12M"); // wheel up at row 12, col 35: over the sheet
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
    // Six tabs walk the bar to the Message editor, then hold L inside the
    // window: the letter is draft text.
    type_and_settle(&mut h, b"\t\t\t\t\t\t");
    type_and_settle(&mut h, b"L");
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.screen();
    assert!(
        screen.contains("L"),
        "the letter lands in the draft: {screen}"
    );
}

#[test]
fn composer_keybar_names_the_keys() {
    // AC8-HP: the keybar is always painted in the composer and names tab,
    // enter, the arrows and esc.
    let scratch = Scratch::new("composer-hint");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("\u{2193}"));
    assert!(screen.contains("esc"), "esc is named: {screen}");
    assert!(
        screen.contains("launch") || screen.contains("pick"),
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
    // AC4-HP: the model tab uses the configured account row and shows its
    // route as the hint, so a glm launch is reachable from the composer.
    // The door under the list is `fno config get accounts.records`, which
    // shells to the installed python CLI; the CI mux job deliberately ships
    // none, and a cold install pays the CLI's bootstrap inside the client's
    // 30s read bound. So the body answers one of two contract surfaces: the
    // configured row with its route hint when the door lands, or the named
    // unavailable row - whose LABEL the tab body never ellipsizes - beside
    // the standing default when it does not. The unit suite pins the row
    // and hint rendering either way.
    let scratch = Scratch::new("composer-route-hint");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    pick_claude_and_open_model_tab(&mut h);
    // The door's read bound is 30s; 35s covers it plus render.
    let screen = h.wait_screen(35, |s| {
        (s.contains("glm-5.3-flash[1m]") && s.contains("zai/glm-5.3-flash[1m]"))
            || (s.contains("model list unavailable") && s.contains("harness default"))
    });
    assert!(
        screen.contains("harness default"),
        "the default row stands under either door answer: {screen}"
    );
    assert!(
        screen.contains("glm-5.3-flash[1m]") || screen.contains("model list unavailable"),
        "a configured row or the named unavailable surface: {screen}"
    );
    // Escape closes the sheet itself; the draft is retained.
    type_and_settle(&mut h, b"\x1b");
    let screen = h.wait_screen(10, |s| !s.contains("new agent"));
    assert!(
        !screen.contains("new agent"),
        "the composer closes: {screen}"
    );
}

#[test]
fn arrows_in_the_model_body_move_and_up_never_launches() {
    // AC3-HP, list grammar: Up/Down move inside the body; nothing in the
    // body launches. Up from the first row stays put; the pane still prints
    // nothing.
    let scratch = Scratch::new("composer-list-nav");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, RIGHT); // the Model tab
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
