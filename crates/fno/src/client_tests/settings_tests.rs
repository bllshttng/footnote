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

    // Keybindings: the prefix first, then every rebindable chord under its
    // section, then the editor row and the file it opens.
    let (rows, actions) = v.settings_rows_for(SettingsTab::Keys);
    assert!(matches!(
        &rows[0],
        PopupRow::Entry { glyph, label, .. }
            if *glyph == crate::keys::prefix_display() && label == "prefix"
    ));
    assert_eq!(actions[0], AuxAction::KeyCapture("prefix".into()));
    for kb in crate::keys::editable_bindings() {
        assert!(
            actions.contains(&AuxAction::KeyCapture(kb.action.into())),
            "{} has a row",
            kb.action
        );
    }
    for title in ["global", "navigation", "workspaces & tabs", "panes"] {
        assert!(rows.contains(&PopupRow::Header(title.into())), "{title}");
    }
    assert!(rows.iter().any(
        |r| matches!(r, PopupRow::Entry { glyph, label, .. } if glyph == "unbound" && label == "split up")
    ));
    assert_eq!(actions.last(), Some(&AuxAction::EditKeysFile));
    let cwd = std::env::current_dir().unwrap();
    let path = crate::digest_overlay::keys_file_path(&cwd);
    assert!(rows.contains(&PopupRow::Info {
        label: "file".into(),
        value: path.display().to_string(),
    }));
}

#[tokio::test]
async fn settings_tabs_switch_by_tab_and_by_tap() {
    let mut v = settings_on(SettingsTab::General);
    let mut keys = Keys::new();
    for want in [
        SettingsTab::Theme,
        SettingsTab::Keys,
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
    assert_eq!(v.settings_tab, SettingsTab::Keys);
    assert!(v.aux.is_some(), "a tab tap keeps settings open");
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

#[tokio::test]
async fn key_capture_refuses_a_conflict_and_takes_a_free_key() {
    let _config = IsolatedConfig::new();
    let before = crate::keys::key_bindings()
        .into_iter()
        .map(|kb| (kb.action, kb.key))
        .collect::<Vec<_>>();
    let mut v = settings_on(SettingsTab::Keys);
    let mut keys = Keys::new();
    // The page is taller than the terminal: select the row, then Enter.
    let modal = v.aux.as_mut().unwrap();
    let detach = AuxAction::KeyCapture("detach".into());
    let row = modal.actions.iter().position(|a| *a == detach).unwrap();
    modal.popup.sel = row;
    keys.send(&mut v, b"\r").await;
    assert!(frame_text(&mut v)
        .iter()
        .any(|l| l.contains("press the new key for detach")));
    // `c` is new tab: refused with the resolver's sentence, nothing moves.
    // An arrow, CSI or SS3, is no key: dropped whole, the capture stays.
    keys.send(&mut v, b"\x1b[A\x1bOA").await;
    assert!(
        v.key_capture.is_some(),
        "an arrow neither binds nor cancels"
    );
    keys.send(&mut v, b"c").await;
    assert!(
        v.notice
            .as_ref()
            .is_some_and(|(n, _)| n.starts_with("config.mux.keys.detach: c would also be")),
        "{:?}",
        v.notice
    );
    assert!(v.key_capture.is_some(), "the capture stays open");
    let now: Vec<_> = crate::keys::key_bindings()
        .into_iter()
        .map(|kb| (kb.action, kb.key))
        .collect();
    assert_eq!(now, before, "the live keymap is unchanged");
    // A digit is refused the same way.
    keys.send(&mut v, b"3").await;
    assert!(v.notice.as_ref().is_some_and(|(n, _)| n.contains("1-9")));
    // Its own shipped key is free: it takes, saves, and reads back. (A key
    // that moves detach would change the process keymap other tests read.)
    keys.send(&mut v, b"d").await;
    assert!(v.key_capture.is_none(), "the capture closes");
    assert_eq!(
        v.aux.as_ref().unwrap().popup.sel,
        row,
        "the cursor returns to the edited row"
    );
    assert_eq!(crate::keys::key_for("detach").as_deref(), Some("d"));
    assert_eq!(
        v.notice.as_ref().map(|(n, _)| n.as_str()),
        Some("detach: d")
    );
}
