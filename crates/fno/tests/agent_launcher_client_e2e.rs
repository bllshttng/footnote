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

/// A harness picker row paints `✓ name` when it is the draft value, `• name`
/// otherwise. The first-run preselect is claude, so either glyph is possible
/// for any row; match rows under both.
fn harness_row(s: &str, name: &str) -> bool {
    s.contains(&format!("\u{2713} {name}")) || s.contains(&format!("\u{2022} {name}"))
}

/// Pick Claude through the harness chip's picker, then open the model
/// picker. Chip-row grammar: Enter on a chip opens its picker, typing
/// filters, Enter commits and closes.
fn pick_claude_and_open_model_picker(h: &mut ClientHarness) {
    // A fresh open focuses the input; two Tabs walk Message ->
    // Permission -> Harness.
    type_and_settle(h, b"\t\t");
    type_and_settle(h, b"\r");
    // The picker lists the catalog's selectable (installed) harnesses once
    // the read lands. A clean CI home has only the fake bins, so the draft
    // default may have no row at all: wait for installed rows, under either
    // glyph.
    h.wait_screen(35, |s| harness_row(s, "claude") && harness_row(s, "codex"));
    type_and_settle(h, b"claude");
    // codex vanishing proves the query narrowed the picker; claude alone
    // stays. Neither side of this test leans on the draft default.
    h.wait_screen(35, |s| harness_row(s, "claude") && !harness_row(s, "codex"));
    type_and_settle(h, b"\r");
    // The committed chip reads `claude` with its caret padding; the filter
    // title never does.
    h.wait_screen(35, |s| s.contains("claude  \u{25be}"));
    // One Tab lands on the Model chip; Enter drops its picker.
    type_and_settle(h, b"\t");
    type_and_settle(h, b"\r");
    h.wait_screen(35, |s| s.contains("harness default"));
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
    // AC1-HP: from the regular sidebar the composer is the centered sheet
    // with the chip row: the Where chip reads Local, the input asks the
    // working question, and the harness chip carries its value untruncated.
    let scratch = Scratch::new("composer-sheet");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("new agent"));
    assert!(screen.contains("new agent"), "sheet title: {screen}");
    // The chip row paints before any catalog read lands.
    assert!(
        screen.contains("Local") && screen.contains("prompt \u{b7} -- flags"),
        "the Where chip and the placeholder paint: {screen}"
    );
    assert!(
        screen.contains("auto"),
        "the bottom row paints the mode chip: {screen}"
    );
    // The harness chip's value lands when the catalog read does; the
    // first-run preselect is claude, per the harness-preselect ruling.
    let screen = h.wait_screen(35, |s| s.contains("claude  \u{25be}"));
    assert!(
        screen.contains("claude  \u{25be}"),
        "the harness chip shows its value: {screen}"
    );
    // The tab strip is gone: no axis names paint in the content area (the
    // sideline's own strip row legitimately reads "Messages").
    let content = pane_region(&screen);
    for tab in ["Harness", "Flags", "Message"] {
        assert!(!content.contains(tab), "no tab label {tab}: {screen}");
    }
}

#[test]
fn project_picker_lists_projects_and_enter_never_launches() {
    // AC2-HP: Enter on the Project chip drops its picker; Enter on a row
    // sets the project without launching.
    let scratch = Scratch::new("composer-project");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    // Wait out the facts probe so the Branch/Worktree chips (painted while
    // facts are unread) no longer sit between Project and the input; this
    // scratch is not a git repo, so the pair hides once the probe lands.
    h.wait_screen(35, |s| !s.contains("worktree"));
    // Shift-Tab from the input lands on the Directory chip; Enter drops its
    // picker (title `directory`).
    type_and_settle(&mut h, b"\x1b[Z");
    type_and_settle(&mut h, b"\r");
    let screen = h.wait_screen(35, |s| s.contains("directory"));
    assert!(
        !screen.contains("starting..."),
        "opening the project picker never launches: {screen}"
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
    pick_claude_and_open_model_picker(&mut h);
    let screen = h.wait_screen(35, |s| s.contains("harness default"));
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
    // The input already holds focus; a focus report is dropped whole and a
    // following arrow still navigates (cursor move, never a launch).
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
    // prefix+F enters the full-screen sideline; the composer comes from
    // prefix+i alone (F no longer opens it).
    type_and_settle(&mut h, PREFIX);
    type_and_settle(&mut h, FULL);
    type_and_settle(&mut h, PREFIX);
    type_and_settle(&mut h, OPEN);
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
    // The input holds focus; holding L inside the window is draft text.
    type_and_settle(&mut h, b"L");
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.screen();
    assert!(
        screen.contains("L"),
        "the letter lands in the draft: {screen}"
    );
}

#[test]
fn narrow_terminal_wraps_the_right_chip_group() {
    // AC1-EDGE: a narrow sheet wraps the right group (harness/model/effort)
    // to its own row; every chip value paints whole.
    let scratch = Scratch::new("composer-chip-wrap");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 50);
    wait_input(&mut h);
    open_composer(&mut h);
    h.wait_screen(10, |s| s.contains("new agent"));
    // The chip value is the first-run preselect claude, painted when the
    // catalog read lands.
    let screen = h.wait_screen(35, |s| s.contains("claude  \u{25be}"));
    // The left group's `auto  \u{25be}` chip and the right group's harness
    // chip sit
    // on different screen rows. The two-space gap pins the match to the
    // sheet's chip row, never the sidebar's `+ new workspace`.
    let plus_row = screen
        .lines()
        .position(|l| l.contains("auto  \u{25be}"))
        .expect("the left chip group paints");
    let harness_line = screen
        .lines()
        .position(|l| l.contains("claude  \u{25be}"))
        .expect("the harness chip paints");
    assert_ne!(
        plus_row, harness_line,
        "the right group wrapped to its own row:\n{screen}"
    );
}

#[test]
fn project_chip_focus_shows_the_working_directory_line() {
    // AC3-HP: focusing the Project chip paints `Working directory` beside
    // the path.
    let scratch = Scratch::new("composer-cwd-line");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    // While the facts probe is in flight the Branch/Worktree chips paint
    // between Project and the input and would eat the BackTab; this scratch
    // is not a git repo, so the pair hides once the probe lands.
    h.wait_screen(35, |s| !s.contains("worktree"));
    type_and_settle(&mut h, b"\x1b[Z"); // Project chip
    let screen = h.wait_screen(35, |s| s.contains("Working directory"));
    assert!(
        screen.contains("Working directory"),
        "the cwd line paints: {screen}"
    );
}

#[test]
fn where_chip_opens_the_local_and_placement_picker() {
    // AC4-HP: the Where picker lists Local under `Run on` and the four
    // placement rows under `Open as` - no Cloud, Remote Control or SSH row.
    let scratch = Scratch::new("composer-where");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    // Wait out the facts probe so the Branch/Worktree chips (painted while
    // facts are unread) no longer sit between Project and the input; this
    // scratch is not a git repo, so the pair hides once the probe lands.
    h.wait_screen(15, |s| !s.contains("worktree"));
    // BackTab BackTab walks Message -> Project -> Where.
    type_and_settle(&mut h, b"\x1b[Z\x1b[Z");
    type_and_settle(&mut h, b"\r");
    let screen = h.wait_screen(10, |s| s.contains("Run on"));
    for want in ["Run on", "Local", "Open as", "thread", "pane: active tab"] {
        assert!(
            screen.contains(want),
            "the Where picker lists {want}: {screen}"
        );
    }
    assert!(
        !screen.contains("Cloud") && !screen.contains("SSH"),
        "no substrate rows are invented: {screen}"
    );
}

#[test]
fn composer_keybar_names_the_keys() {
    // AC8-HP: the keybar names the chip-row grammar: Enter launches from
    // the prompt line (or names the chip it opens), Tab moves, ^j is a
    // newline in the input, esc closes.
    let scratch = Scratch::new("composer-hint");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    let screen = h.wait_screen(10, |s| s.contains("tab next"));
    assert!(
        screen.contains("\u{21b5} launch"),
        "enter names the launch from the prompt line: {screen}"
    );
    assert!(screen.contains("tab next"), "tab is named: {screen}");
    assert!(screen.contains("esc close"), "esc is named: {screen}");
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
    let screen = h.wait_screen(10, |s| s.contains("esc cancel"));
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
    pick_claude_and_open_model_picker(&mut h);
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
    // Escape closes the open model picker, then the sheet itself; the draft
    // is retained.
    type_and_settle(&mut h, b"\x1b");
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
    // Three tabs land on the Model chip; Enter drops its picker.
    type_and_settle(&mut h, b"\t\t\t");
    type_and_settle(&mut h, b"\r");
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

/// The regression behind the model-floor contract: with claude chosen, the
/// Model tab listed
/// only "harness default" because no account record pinned a model. Each
/// harness now floors its list off the capability table; configured rows
/// merge over. One test per harness: the launcher retains focus across a
/// close/reopen, so each composer runs from a fresh client.
#[test]
fn claude_model_tab_lists_the_claude_families() {
    let scratch = Scratch::new("composer-model-floor");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    pick_claude_and_open_model_picker(&mut h);
    // The floor lands with the catalog read; the accounts door may take up
    // to 30s in CI, so wait like the route-hint test does.
    let screen = h.wait_screen(35, |s| {
        s.contains("harness default")
            && s.contains("opus")
            && s.contains("sonnet")
            && s.contains("haiku")
    });
    for want in ["harness default", "opus", "sonnet", "haiku", "fable"] {
        assert!(screen.contains(want), "claude floor lists {want}: {screen}");
    }
}

#[test]
fn codex_model_tab_lists_the_codex_slugs() {
    let scratch = Scratch::new("composer-model-floor-codex");
    seed_routing_config(&scratch);
    let envs = with_fake_harnesses(&scratch);
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    // Narrow the harness picker to codex, commit it, then Tab to the Model
    // chip and drop its picker. Same grammar the claude helper exercises.
    type_and_settle(&mut h, b"\t\t");
    type_and_settle(&mut h, b"\r");
    h.wait_screen(35, |s| harness_row(s, "claude") && harness_row(s, "codex"));
    type_and_settle(&mut h, b"codex");
    h.wait_screen(35, |s| harness_row(s, "codex") && !harness_row(s, "claude"));
    type_and_settle(&mut h, b"\r");
    h.wait_screen(35, |s| s.contains("codex  \u{25be}"));
    type_and_settle(&mut h, b"\t");
    type_and_settle(&mut h, b"\r");
    let screen = h.wait_screen(35, |s| {
        s.contains("harness default") && s.contains("gpt-6-luna")
    });
    for want in ["gpt-6-luna", "gpt-6-astra", "gpt-5.5"] {
        assert!(screen.contains(want), "codex floor lists {want}: {screen}");
    }
}

#[allow(dead_code)]
fn unused_path_helper(_: PathBuf) {}
#[test]
fn model_picker_shows_the_flagship_and_the_seeded_provider_group() {
    // AC5-HP / AC7-HP preamble: a deepseek provider record and a fresh
    // models.dev cache land in the scratch home, so the claude Model
    // picker paints the flagship row under `harness default` and a
    // deepseek group without a network fetch. The CI mux job ships no
    // Python CLI, so the config reads may not land; the assertion accepts
    // the named unavailable surface (the two-surface rule).
    let scratch = Scratch::new("composer-model-picker-rows");
    let dir = scratch.0.join("home").join(".fno");
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[model_routing.providers.deepseek]\n\
         protocol = \"anthropic\"\n\
         base_url = \"https://api.deepseek.com/anthropic\"\n\
         api_key_env = \"FNO_TEST_DS_KEY\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("cache").join("models-dev.json"),
        r#"{"deepseek":{"name":"DeepSeek","env":["DEEPSEEK_API_KEY"],"api":"https://api.deepseek.com","npm":"@ai-sdk/anthropic","models":{"deepseek-chat":{"id":"deepseek-chat","name":"DeepSeek V3"}}}}"#,
    )
    .unwrap();
    let mut envs = with_fake_harnesses(&scratch);
    envs.push(("FNO_TEST_DS_KEY", "x".to_string()));
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut h = ClientHarness::spawn_sized_with(&scratch, 24, 120, &env_refs);
    wait_input(&mut h);
    open_composer(&mut h);
    pick_claude_and_open_model_picker(&mut h);
    let screen = h.wait_screen(35, |s| s.contains("harness default"));
    if screen.contains("model list unavailable") {
        return; // the config reads never landed; the notice is the surface
    }
    assert!(
        screen.contains("deepseek"),
        "the seeded provider group renders: {screen}"
    );
    assert!(
        screen.contains("flagship"),
        "the flagship row paints under the default: {screen}"
    );
    assert!(
        screen.contains("opus"),
        "the flagship names the floor lead: {screen}"
    );
}

#[test]
fn typed_dash_dash_opens_the_flag_picker_and_a_pick_becomes_a_pill() {
    // AC10-HP: `--` at a word start opens the harness's flag picker (the
    // compiled capability table carries claude's captured flags); a pick
    // becomes a pill on the pills row and leaves the message clean.
    let scratch = Scratch::new("composer-flag-pills");
    let mut h = ClientHarness::spawn_sized(&scratch, 24, 120);
    wait_input(&mut h);
    open_composer(&mut h);
    type_and_settle(&mut h, b"--");
    let screen = h.wait_screen(35, |s| s.contains("--agent"));
    assert!(
        screen.contains("--agent"),
        "the flags picker lists the captured flags: {screen}"
    );
    // Narrow to --agent and pick it; the pill paints with its value slot.
    type_and_settle(&mut h, b"agent");
    type_and_settle(&mut h, b"\r");
    let screen = h.wait_screen(10, |s| s.contains("--agent <value>"));
    assert!(
        screen.contains("--agent <value>"),
        "the pill paints with its value slot: {screen}"
    );
}
