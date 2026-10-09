//! The settings modal on its real key and tap paths: every chunk goes
//! through `handle_stdin`, and the echo checks read the composed frame.

use super::*;
use crate::client::input_field::InputField;

/// Settings open on `tab`, on a 30x100 terminal.
fn settings_on(tab: SettingsTab) -> View {
    let mut v = two_pane_view();
    v.settings_tab = tab;
    v.aux = Some(v.build_settings_modal());
    v
}

struct Keys {
    scanner: Scanner,
    carry: Vec<u8>,
    buf: Vec<u8>,
}

impl Keys {
    fn new() -> Self {
        Keys {
            scanner: Scanner::default(),
            carry: Vec::new(),
            buf: Vec::new(),
        }
    }

    async fn send(&mut self, v: &mut View, bytes: &[u8]) {
        handle_stdin(v, &mut self.scanner, &mut self.carry, bytes, &mut self.buf)
            .await
            .unwrap();
        assert!(self.buf.is_empty(), "settings keys never reach a pane");
    }

    /// A real Esc press: the lone byte, then the quiet-window flush.
    async fn esc(&mut self, v: &mut View) {
        self.send(v, b"\x1b").await;
        overlay_keys::flush_lone_esc(v, &mut self.scanner, &mut self.buf)
            .await
            .unwrap();
    }

    /// A left press and release on the settings cell showing `needle`.
    async fn tap(&mut self, v: &mut View, needle: &str) {
        let (row, col) = aux_cell(v, needle);
        for end in ['M', 'm'] {
            let report = format!("\x1b[<0;{};{}{end}", col + 1, row + 1);
            self.send(v, report.as_bytes()).await;
        }
    }
}

/// The screen cell where `needle` starts inside the open aux popup.
fn aux_cell(v: &View, needle: &str) -> (u16, u16) {
    let r = v.aux.as_ref().expect("settings open").popup.render(v.term);
    r.lines
        .iter()
        .enumerate()
        .find_map(|(i, l)| {
            let at = l.text.find(needle)?;
            Some((
                (r.origin.0 + i) as u16,
                (r.origin.1 + l.text[..at].chars().count()) as u16,
            ))
        })
        .unwrap_or_else(|| panic!("{needle:?} is not on the popup"))
}

/// The composed frame, row by row.
fn frame_text(v: &mut View) -> Vec<String> {
    crate::vt::frame_text(&v.compose())
        .lines()
        .map(String::from)
        .collect()
}

/// Pin config reads and writes to a temp file and a `fno` stub that
/// accepts every `config set`, so a test save never touches a real config.
struct IsolatedConfig {
    _dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
    restore: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl IsolatedConfig {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let lock = crate::digest_overlay::ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let stub = dir.path().join("fno-stub");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let restore = ["FNO_CONFIG", "FNO_BIN"]
            .into_iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect();
        std::env::set_var("FNO_CONFIG", &config);
        std::env::set_var("FNO_BIN", &stub);
        IsolatedConfig {
            _dir: dir,
            _lock: lock,
            restore,
        }
    }
}

impl Drop for IsolatedConfig {
    fn drop(&mut self) {
        for (k, v) in self.restore.drain(..) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

#[test]
fn settings_rows() {
    // The Keys rows and the assertion both read FNO_CONFIG from the process
    // env; hold the environment lock so a parallel IsolatedConfig cannot
    // flip it between the two reads.
    let _env = crate::digest_overlay::ENVIRONMENT_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let v = two_pane_view();
    let (rows, actions) = v.settings_rows_for(SettingsTab::Theme);
    // One ApplyTheme action per shipped theme, in display order.
    let names: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            AuxAction::ApplyTheme(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, crate::theme::THEME_NAMES.to_vec());
    assert!(
        rows.iter()
            .any(|r| matches!(r, PopupRow::Entry { glyph, .. } if glyph == "●")),
        "active theme is marked"
    );
    assert!(matches!(
        rows.last(),
        Some(PopupRow::Entry { label, .. }) if label == "add theme file"
    ));

    let (rows, actions) = v.settings_rows_for(SettingsTab::General);
    assert!(actions.contains(&AuxAction::ToggleHoverFocus));
    assert!(actions.contains(&AuxAction::ToggleResourceMeter));
    assert!(rows.iter().all(|row| !matches!(
        row,
        PopupRow::Entry { hint, .. } if hint == "session only"
    )));

    // The Keys tab renders no rows of its own: switching to it opens the
    // which-key table (one table, US1), so the tab is a bare launcher.
    let (rows, actions) = v.settings_rows_for(SettingsTab::Keys);
    assert!(rows.is_empty() && actions.is_empty());
}

#[tokio::test]
async fn settings_tabs_switch_by_tab_and_by_tap() {
    let mut v = settings_on(SettingsTab::General);
    let mut keys = Keys::new();
    // The tab cycle skips the keybindings tab: it is a launcher, reached by
    // click, never something to tab past.
    for want in [
        SettingsTab::Theme,
        SettingsTab::Colors,
        SettingsTab::General,
    ] {
        keys.send(&mut v, b"\t").await;
        assert_eq!(v.settings_tab, want);
    }
    // A hover over a tab label moves no row selection.
    let sel = v.aux.as_ref().unwrap().popup.sel;
    let (row, col) = aux_cell(&v, "colors");
    keys.send(
        &mut v,
        format!("\x1b[<35;{};{}M", col + 1, row + 1).as_bytes(),
    )
    .await;
    assert_eq!(v.aux.as_ref().unwrap().popup.sel, sel);
    keys.tap(&mut v, "colors").await;
    assert_eq!(v.settings_tab, SettingsTab::Colors);
    keys.tap(&mut v, "keybindings").await;
    // The keybindings tab opens the SAME table the menu's keybindings row
    // opens: settings closes, the which-key modal takes over.
    assert!(v.aux.is_none(), "settings hands off to the table");
    let m = v.keys_modal.as_ref().expect("the which-key table opens");
    // The prefix line leads and the editor button rides the table.
    assert!(matches!(
        m.popup.rows.first(),
        Some(PopupRow::Entry { glyph, label, .. })
            if *glyph == crate::keys::prefix_display() && label == "prefix"
    ));
    let edit = m.edit_row.expect("the table carries an editor button");
    assert!(matches!(
        &m.popup.rows[edit],
        PopupRow::FullWidth(l) if l.starts_with("[ edit keys in ") && l.ends_with(" ]")
    ));
}

#[tokio::test]
async fn lane_key_entry_enter_opens_the_picker_for_the_typed_key() {
    let _config = IsolatedConfig::new();
    let mut v = settings_on(SettingsTab::Colors);
    let mut keys = Keys::new();
    let mut buf = Vec::new();
    execute_aux_action(&mut v, AuxAction::LaneColorAdd("model".into()), &mut buf)
        .await
        .unwrap();
    // Every byte repaints the field with the text so far and the cursor
    // cell after it.
    let mut typed = String::new();
    for b in "glm".bytes() {
        keys.send(&mut v, &[b]).await;
        typed.push(b as char);
        let want = format!("model key: {typed} ");
        assert!(
            frame_text(&mut v).iter().any(|l| l.contains(&want)),
            "the frame shows {want:?}"
        );
    }
    keys.send(&mut v, b"\r").await;
    assert!(v.lane.key_entry.is_none(), "the field closes on submit");
    assert_eq!(v.lane.pick, Some(("model".into(), "glm".into())));
    assert_eq!(v.lane.axis, None, "back from the picker lands on the list");
    keys.tap(&mut v, "magenta").await;
    assert_eq!(
        v.lane.pick, None,
        "a color pick saves and returns to the list"
    );
    assert!(v
        .notice
        .as_ref()
        .is_some_and(|(n, _)| n.starts_with("model.glm")));
}

#[tokio::test]
async fn lane_custom_entry_enter_refuses_an_invalid_color_without_saving() {
    let mut v = settings_on(SettingsTab::Colors);
    let mut keys = Keys::new();
    let mut buf = Vec::new();
    let custom = AuxAction::LaneColorCustom("route".into(), "zai".into());
    execute_aux_action(&mut v, custom, &mut buf).await.unwrap();
    keys.send(&mut v, b"#12a").await;
    assert!(frame_text(&mut v)
        .iter()
        .any(|l| l.contains("route.zai: #12a ")));
    keys.send(&mut v, b"\r").await;
    assert!(
        v.notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("invalid color (name, indexed(n), #rrggbb)")),
        "the refusal names the accepted shapes: {:?}",
        v.notice
    );
    assert_eq!(
        v.lane.custom_entry.as_ref().map(InputField::text),
        Some("#12a"),
        "nothing saved; the field stays open for a fix"
    );
}

#[tokio::test]
async fn settings_esc_and_back_step_out_one_level() {
    let mut v = settings_on(SettingsTab::Colors);
    let mut keys = Keys::new();
    let mut buf = Vec::new();
    // The back row closes an open key-name field and keeps settings.
    execute_aux_action(&mut v, AuxAction::LaneColorAdd("model".into()), &mut buf)
        .await
        .unwrap();
    assert!(frame_text(&mut v).iter().any(|l| l.contains("esc back")));
    keys.tap(&mut v, "‹ back").await;
    assert!(v.lane.key_entry.is_none() && v.lane.axis.is_none());
    assert!(v.aux.is_some(), "the colors list shows");

    // Colors list -> add model key -> "glm" -> custom: Esc walks back.
    execute_aux_action(&mut v, AuxAction::LaneColorAdd("model".into()), &mut buf)
        .await
        .unwrap();
    keys.send(&mut v, b"glm\r").await;
    let custom = AuxAction::LaneColorCustom("model".into(), "glm".into());
    execute_aux_action(&mut v, custom, &mut buf).await.unwrap();
    keys.esc(&mut v).await;
    assert!(v.lane.custom_entry.is_none());
    assert_eq!(v.lane.pick, Some(("model".into(), "glm".into())), "picker");
    keys.esc(&mut v).await;
    assert_eq!(v.lane.pick, None, "colors list");
    assert!(v.aux.is_some());
    assert!(frame_text(&mut v)
        .iter()
        .any(|l| l.contains("tab or click a section · esc close")));
    keys.esc(&mut v).await;
    assert!(v.aux.is_none(), "the top level closes");
}

#[tokio::test]
async fn lane_entry_buffer_dies_with_a_mouse_dismiss() {
    let mut v = settings_on(SettingsTab::Colors);
    v.lane.pick = Some(("route".into(), "zai".into()));
    v.lane.custom_entry = Some(InputField::new("", 64).with_text("#12"));
    v.reopen_settings_keeping_sel();
    let mut buf: Vec<u8> = Vec::new();
    // A click OFF the block dismisses the modal AND drops the buffer, so
    // a stale entry can never capture keys in a reopened modal.
    aux_mouse(&mut v, left_click(1, 1), &mut buf).await.unwrap();
    assert!(v.aux.is_none(), "off-block click dismisses");
    assert!(
        v.lane.custom_entry.is_none(),
        "the buffer died with the modal"
    );
    assert_eq!(v.lane.pick, Some(("route".into(), "zai".into())));
}
