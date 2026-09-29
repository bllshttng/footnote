use super::*;
use std::os::unix::fs::PermissionsExt;

struct RestoreConfigEnv {
    config: Option<std::ffi::OsString>,
    binary: Option<std::ffi::OsString>,
    mux_dir: Option<std::ffi::OsString>,
}

impl Drop for RestoreConfigEnv {
    fn drop(&mut self) {
        match self.config.take() {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        match self.binary.take() {
            Some(value) => std::env::set_var("FNO_BIN", value),
            None => std::env::remove_var("FNO_BIN"),
        }
        match self.mux_dir.take() {
            Some(value) => std::env::set_var("FNO_MUX_DIR", value),
            None => std::env::remove_var("FNO_MUX_DIR"),
        }
    }
}

// The settings modal sits on the theme ground (plain body). The inverse body
// block under a named theme read as a white slab - the keys-modal fix on a path
// it missed. This owner test also guards the theme-import entry and preview.
#[tokio::test]
async fn settings_modal_body_paints_no_inverse_under_a_named_theme() {
    let source_dir = tempfile::tempdir().unwrap();
    let _env_lock = crate::digest_overlay::ENVIRONMENT_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = RestoreConfigEnv {
        config: std::env::var_os("FNO_CONFIG"),
        binary: std::env::var_os("FNO_BIN"),
        mux_dir: std::env::var_os("FNO_MUX_DIR"),
    };
    let config_path = source_dir.path().join("isolated-config.toml");
    std::fs::write(&config_path, "").unwrap();
    let mux_dir = source_dir.path().join("isolated-mux");
    std::fs::create_dir_all(&mux_dir).unwrap();
    let fake_fno = source_dir.path().join("fno-config-stub");
    std::fs::write(&fake_fno, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = std::fs::metadata(&fake_fno).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_fno, permissions).unwrap();
    std::env::set_var("FNO_CONFIG", &config_path);
    std::env::set_var("FNO_MUX_DIR", &mux_dir);
    std::env::set_var("FNO_BIN", &fake_fno);

    let mut view = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: vec![],
            active_squad: 0,
            panes: vec![],
            focus: 0,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
        },
    );
    view.theme = crate::theme::Theme::from_name("footnote-superscript").0;
    let (theme_rows, theme_actions) = view.settings_rows_for(SettingsTab::Theme);
    assert!(matches!(
        theme_rows.last(),
        Some(PopupRow::Entry { label, .. }) if label == "+ add own theme"
    ));
    assert_eq!(theme_actions.len(), crate::theme::THEME_NAMES.len() + 1);
    assert!(theme_actions
        .iter()
        .take(crate::theme::THEME_NAMES.len())
        .all(|action| matches!(action, AuxAction::ApplyTheme(_))));
    let name = format!("theme-import-{}", std::process::id());
    let source_path = source_dir.path().join(format!("{name}.toml"));
    std::fs::write(
        &source_path,
        format!("[mux.themes.{name}]\nbase = \"#101018\"\n"),
    )
    .unwrap();
    view.settings_tab = SettingsTab::Theme;
    theme_import_ui::open(&mut view);
    let mut typed = source_path.to_string_lossy().as_bytes().to_vec();
    typed.push(b'\n');
    theme_import_ui::entry_keys(&mut view, &typed)
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    theme_import_ui::maybe_kick(&mut view, &tx);
    let message = rx.recv().await.unwrap();
    theme_import_ui::apply_result(&mut view, message);
    assert!(matches!(
        &view.theme_import,
        theme_import_ui::ThemeImportUi::Preview { candidates, .. }
            if candidates.len() == 1 && candidates[0].name == name
    ));
    let local_preview = view.theme_import.clone();

    view.theme_import = theme_import_ui::ThemeImportUi::Entry(String::new());
    view.reopen_settings_keeping_sel();
    let refused_url = b"http://github.com/octo/themes/blob/main/theme.conf\n".to_vec();
    theme_import_ui::entry_keys(&mut view, &refused_url)
        .await
        .unwrap();
    assert!(
        view.notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("HTTPS")),
        "notice: {:?}",
        view.notice
    );
    view.theme_import = local_preview;
    view.reopen_settings_keeping_sel();

    let single_dir = source_dir.path().join("saved-single-theme");
    crate::digest_overlay::set_themes_dir_for_test(Some(single_dir.clone()));
    theme_import_ui::save(&mut view).await.unwrap();
    assert!(single_dir.join(format!("{name}.toml")).is_file());
    assert!(view
        .user_themes
        .iter()
        .any(|(user_name, _)| user_name == &name));
    assert_eq!(view.theme.name, name);
    assert!(view.pending_ground.is_some());
    crate::digest_overlay::set_themes_dir_for_test(None);

    let terminal_candidate = crate::theme_import::candidate(
        "terminal-import",
        vec![("inherit".into(), "terminal".into())],
        &std::collections::HashSet::new(),
    )
    .unwrap();
    view.theme_import = theme_import_ui::ThemeImportUi::Preview {
        source: "local theme".into(),
        candidates: vec![terminal_candidate],
        skipped: Vec::new(),
    };
    let (preview_rows, preview_actions) = view.settings_rows_for(SettingsTab::Theme);
    assert!(preview_rows.iter().any(|row| matches!(
        row,
        PopupRow::Entry { label, .. } if label == "ground: none - your terminal keeps its own background"
    )));
    assert!(preview_rows.iter().any(|row| matches!(
        row,
        PopupRow::Entry { label, enabled: true, .. } if label == "save and apply"
    )));
    assert!(matches!(
        preview_actions.first(),
        Some(AuxAction::ThemeImportSave)
    ));

    let invalid = crate::theme_import::candidate(
        "invalid-import",
        vec![("base".into(), "#zzzzzz".into())],
        &std::collections::HashSet::new(),
    )
    .unwrap();
    view.theme_import = theme_import_ui::ThemeImportUi::Preview {
        source: "bad theme".into(),
        candidates: vec![invalid],
        skipped: Vec::new(),
    };
    let (warning_rows, warning_actions) = view.settings_rows_for(SettingsTab::Theme);
    assert!(warning_rows.iter().any(|row| matches!(
        row,
        PopupRow::Entry { label, enabled: false, .. } if label == "save and apply"
    )));
    assert!(matches!(
        warning_actions.as_slice(),
        [AuxAction::ThemeImportCancel]
    ));

    let themes_dir = source_dir.path().join("saved-themes");
    crate::digest_overlay::set_themes_dir_for_test(Some(themes_dir.clone()));
    let first = crate::theme_import::candidate(
        &format!("{name}-one"),
        vec![("base".into(), "#101018".into())],
        &std::collections::HashSet::new(),
    )
    .unwrap();
    let second = crate::theme_import::candidate(
        &format!("{name}-two"),
        vec![("base".into(), "#202028".into())],
        &std::collections::HashSet::new(),
    )
    .unwrap();
    view.theme_import = theme_import_ui::ThemeImportUi::Preview {
        source: "local folder".into(),
        candidates: vec![first, second],
        skipped: Vec::new(),
    };
    theme_import_ui::save(&mut view).await.unwrap();
    assert!(themes_dir.join(format!("{name}-one.toml")).is_file());
    assert!(themes_dir.join(format!("{name}-two.toml")).is_file());
    assert!(view
        .user_themes
        .iter()
        .any(|(user_name, _)| user_name == &format!("{name}-one")));
    assert!(matches!(
        view.theme_import,
        theme_import_ui::ThemeImportUi::Idle
    ));
    crate::digest_overlay::set_themes_dir_for_test(None);

    view.theme_import = theme_import_ui::ThemeImportUi::Idle;
    for tab in [
        SettingsTab::General,
        SettingsTab::Theme,
        SettingsTab::Keys,
        SettingsTab::Colors,
    ] {
        view.settings_tab = tab;
        if tab == SettingsTab::Colors {
            view.lane.axis = Some("route".into());
        }
        view.aux = Some(view.build_settings_modal());
        let aux = view.aux.as_ref().expect("modal open");
        let rendered = aux.popup.render(view.term);
        let rows_n = view.term.0 as usize;
        let cols = view.term.1 as usize;
        let mut cells = vec![crate::proto::Cell::default(); rows_n * cols];
        crate::popup::draw(&mut cells, rows_n, cols, &rendered, &view.theme);
        let inverse = cells
            .iter()
            .filter(|cell| cell.flags & crate::proto::cell_flags::INVERSE != 0)
            .count();
        assert_eq!(
            inverse, 0,
            "tab {tab:?}: no INVERSE on the plain-body settings modal"
        );
    }
}
