use super::input_field::{back_row, InputField};
use super::*;
use std::path::{Path, PathBuf};

const MAX_INPUT_BYTES: usize = 1_024;

#[derive(Debug, Clone, Default)]
pub(super) enum ThemeImportUi {
    #[default]
    Idle,
    Entry(InputField),
    /// The macOS file picker is open; its answer feeds the same preview.
    Picking {
        gen: u64,
        started: bool,
    },
    Loading {
        gen: u64,
        input: String,
        source: crate::theme_import::Source,
        started: bool,
    },
    Preview {
        source: String,
        candidates: Vec<crate::theme_import::Candidate>,
        skipped: Vec<String>,
    },
}

pub(super) struct ImportMsg {
    gen: u64,
    input: String,
    source: String,
    result: Result<crate::theme_import::Preview, String>,
}

pub(super) type ImportTx = tokio::sync::mpsc::UnboundedSender<ImportMsg>;

pub(super) fn is_entry(state: &ThemeImportUi) -> bool {
    matches!(state, ThemeImportUi::Entry(_))
}

pub(super) fn reset(view: &mut View) {
    view.theme_import_gen = view.theme_import_gen.wrapping_add(1);
    view.theme_import = ThemeImportUi::Idle;
}

fn field() -> InputField {
    InputField::new("path, folder or GitHub file URL", MAX_INPUT_BYTES)
}

pub(super) fn open(view: &mut View) {
    view.theme_import_gen = view.theme_import_gen.wrapping_add(1);
    view.theme_import = ThemeImportUi::Entry(field());
    view.reopen_settings_keeping_sel();
}

/// The argv that opens the system file picker and prints the chosen path:
/// osascript "choose file" on a local macOS session, nothing on Linux or
/// over SSH (the dialog would open on a screen the user cannot see).
pub(super) fn picker_argv(os: &str, ssh: bool) -> Option<Vec<String>> {
    (os == "macos" && !ssh).then(|| {
        vec![
            "osascript".into(),
            "-e".into(),
            "POSIX path of (choose file with prompt \"Choose a theme file\")".into(),
        ]
    })
}

fn local_picker() -> Option<Vec<String>> {
    let ssh = ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some());
    picker_argv(std::env::consts::OS, ssh)
}

pub(super) fn pick(view: &mut View) {
    if local_picker().is_none() {
        view.set_notice("no file picker on this session; type the path".into());
        return;
    }
    view.theme_import = ThemeImportUi::Picking {
        gen: view.theme_import_gen,
        started: false,
    };
    view.reopen_settings_keeping_sel();
}

/// A dropped path as a terminal types it: `file://`, one pair of quotes,
/// and backslash escapes (Ghostty and iTerm2 escape spaces) come off.
fn unshell(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("file://").unwrap_or(s);
    let s = ['\'', '"']
        .iter()
        .find_map(|q| s.strip_prefix(*q).and_then(|t| t.strip_suffix(*q)))
        .unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' {
            chars.next().unwrap_or(c)
        } else {
            c
        });
    }
    out
}

/// Enter on the theme file field; true ends the chunk. An empty field opens
/// the picker where there is one. A path that fails as typed is retried
/// with its drop escapes removed.
pub(super) fn submit(view: &mut View, input: String) -> bool {
    if input.is_empty() {
        if local_picker().is_none() {
            return false;
        }
        pick(view);
        return true;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let parsed = crate::theme_import::parse_source(&input, &cwd).or_else(|reason| {
        let plain = unshell(&input);
        if plain == input {
            return Err(reason);
        }
        crate::theme_import::parse_source(&plain, &cwd).map_err(|_| reason)
    });
    match parsed {
        Ok(source) => {
            view.theme_import = ThemeImportUi::Loading {
                gen: view.theme_import_gen,
                input,
                source,
                started: false,
            };
        }
        Err(reason) => view.set_notice(reason),
    }
    true
}

pub(super) fn cancel(view: &mut View) {
    reset(view);
    view.reopen_settings_keeping_sel();
}

pub(super) fn maybe_kick(view: &mut View, tx: &ImportTx) {
    let tx = tx.clone();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let theme_dir = crate::digest_overlay::themes_dir();
    match &mut view.theme_import {
        ThemeImportUi::Loading {
            gen,
            input,
            source,
            started,
        } if !*started => {
            *started = true;
            let (gen, input, source) = (*gen, input.clone(), source.clone());
            tokio::spawn(async move {
                let result =
                    crate::theme_import::preview_with_theme_dir(&source, &cwd, theme_dir).await;
                let source = source_label(&source);
                let _ = tx.send(ImportMsg {
                    gen,
                    input,
                    source,
                    result,
                });
            });
        }
        ThemeImportUi::Picking { gen, started } if !*started => {
            *started = true;
            let gen = *gen;
            let Some(argv) = local_picker() else {
                return;
            };
            tokio::spawn(async move {
                let run = tokio::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .kill_on_drop(true)
                    .output();
                let path = match tokio::time::timeout(Duration::from_secs(600), run).await {
                    Ok(Ok(out)) if out.status.success() => {
                        String::from_utf8_lossy(&out.stdout).trim().to_string()
                    }
                    _ => String::new(),
                };
                let (source, result) = if path.is_empty() {
                    (String::new(), Err("file picker: cancelled".to_string()))
                } else {
                    match crate::theme_import::parse_source(&path, &cwd) {
                        Ok(source) => (
                            source_label(&source),
                            crate::theme_import::preview_with_theme_dir(&source, &cwd, theme_dir)
                                .await,
                        ),
                        Err(reason) => (path.clone(), Err(reason)),
                    }
                };
                let _ = tx.send(ImportMsg {
                    gen,
                    input: path,
                    source,
                    result,
                });
            });
        }
        _ => {}
    }
}

pub(super) fn apply_result(view: &mut View, message: ImportMsg) {
    if view.aux.is_none() || view.settings_tab != SettingsTab::Theme {
        return;
    }
    let still_loading = matches!(
        &view.theme_import,
        ThemeImportUi::Loading { gen, .. } | ThemeImportUi::Picking { gen, .. }
            if *gen == message.gen
    );
    if !still_loading || view.theme_import_gen != message.gen {
        return;
    }
    match message.result {
        Ok(preview) => {
            view.theme_import = ThemeImportUi::Preview {
                source: message.source,
                candidates: preview.candidates,
                skipped: preview.skipped,
            };
        }
        Err(reason) => {
            view.set_notice(reason);
            view.theme_import = ThemeImportUi::Entry(field().with_text(&message.input));
        }
    }
    view.reopen_settings_keeping_sel();
}

pub(super) fn rows(state: &ThemeImportUi, cwd: &Path) -> (Vec<PopupRow>, Vec<AuxAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    match state {
        ThemeImportUi::Idle => return (rows, actions),
        ThemeImportUi::Entry(field) => {
            rows.push(back_row());
            actions.push(AuxAction::SettingsBack);
            rows.push(field.row("theme file"));
            inert_entry(&mut rows, "type a path or drop a file here");
            inert_entry(&mut rows, "fno theme files and Ghostty theme files");
            if local_picker().is_some() {
                selectable(
                    &mut rows,
                    &mut actions,
                    "choose file…",
                    AuxAction::ThemePick,
                    true,
                );
            }
        }
        ThemeImportUi::Picking { .. } => {
            rows.push(back_row());
            actions.push(AuxAction::SettingsBack);
            inert_entry(&mut rows, "waiting for the file picker…");
        }
        ThemeImportUi::Loading { source, .. } => {
            inert(&mut rows, PopupRow::Header("import theme".into()));
            inert_entry(&mut rows, &format!("reading {}...", source_label(source)));
            selectable(
                &mut rows,
                &mut actions,
                "cancel",
                AuxAction::ThemeImportCancel,
                true,
            );
        }
        ThemeImportUi::Preview {
            source,
            candidates,
            skipped,
            ..
        } => {
            inert(&mut rows, PopupRow::Header(format!("preview: {source}")));
            let mut warned = false;
            let paint_background = crate::digest_overlay::paint_background_enabled(cwd);
            for candidate in candidates {
                inert(&mut rows, PopupRow::Header(candidate.name.clone()));
                if let Some(reason) = &candidate.rename_reason {
                    inert_entry(&mut rows, reason);
                }
                let ground = if crate::theme::ground_set(&candidate.theme).is_some() {
                    format!("ground: {}", color_text(candidate.theme.base))
                } else {
                    "ground: none - your terminal keeps its own background".into()
                };
                inert_entry(&mut rows, &ground);
                if !paint_background {
                    inert_entry(&mut rows, "(not painted: mux.paint_background is off)");
                }
                for (role, color) in role_colors(&candidate.theme) {
                    let set = candidate.spec.iter().any(|(key, _)| key == role);
                    let parent = candidate.theme.inherit_from;
                    rows.push(PopupRow::SwatchEntry {
                        glyph: " ".into(),
                        label: format!("{role}: {}", color_text(color)),
                        hint: if set {
                            "set".into()
                        } else {
                            format!("from {parent}")
                        },
                        enabled: false,
                        color,
                    });
                }
                for warning in &candidate.warnings {
                    warned = true;
                    inert_entry(&mut rows, &warning.0);
                }
            }
            for skipped in skipped {
                inert_entry(&mut rows, &format!("skipped: {skipped}"));
            }
            if candidates.is_empty() {
                inert_entry(&mut rows, "no theme files to save");
            }
            let save_label = if candidates.len() == 1 {
                "save and apply".to_string()
            } else {
                format!("save {} themes", candidates.len())
            };
            selectable(
                &mut rows,
                &mut actions,
                &save_label,
                AuxAction::ThemeImportSave,
                !warned && !candidates.is_empty(),
            );
            selectable(
                &mut rows,
                &mut actions,
                "cancel",
                AuxAction::ThemeImportCancel,
                true,
            );
        }
    }
    (rows, actions)
}

fn role_colors(theme: &Theme) -> [(&'static str, crate::proto::Color); 9] {
    [
        ("base", theme.base),
        ("stamp", theme.stamp),
        ("border", theme.border),
        ("title", theme.title),
        ("brand", theme.brand),
        ("needs_you", theme.needs_you),
        ("sel", theme.sel),
        ("dim", theme.dim),
        ("chip", theme.chip),
    ]
}

fn color_text(color: crate::proto::Color) -> String {
    match color {
        crate::proto::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        crate::proto::Color::Indexed(n) => format!("indexed({n})"),
        crate::proto::Color::Default => "default".into(),
    }
}

fn inert(rows: &mut Vec<PopupRow>, row: PopupRow) {
    rows.push(row);
}

fn inert_entry(rows: &mut Vec<PopupRow>, label: &str) {
    inert(
        rows,
        PopupRow::Entry {
            glyph: " ".into(),
            label: label.into(),
            hint: String::new(),
            enabled: false,
        },
    );
}

fn selectable(
    rows: &mut Vec<PopupRow>,
    actions: &mut Vec<AuxAction>,
    label: &str,
    action: AuxAction,
    enabled: bool,
) {
    rows.push(PopupRow::Entry {
        glyph: " ".into(),
        label: label.into(),
        hint: String::new(),
        enabled,
    });
    if enabled {
        actions.push(action);
    }
}

pub(super) async fn save(view: &mut View) -> Result<(), String> {
    let ThemeImportUi::Preview {
        source, candidates, ..
    } = view.theme_import.clone()
    else {
        return Ok(());
    };
    if candidates.is_empty()
        || candidates
            .iter()
            .any(|candidate| !candidate.warnings.is_empty())
    {
        return Ok(());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(dir) = crate::digest_overlay::themes_dir() else {
        view.set_notice("cannot resolve the user theme folder".into());
        view.reopen_settings_keeping_sel();
        return Ok(());
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let names = match crate::theme_import::save_all(&dir, &candidates, &source, &date) {
        Ok(names) => names,
        Err(reason) => {
            view.set_notice(reason);
            view.reopen_settings_keeping_sel();
            return Ok(());
        }
    };
    view.user_themes = crate::digest_overlay::user_themes(&cwd).0;
    if names.iter().any(|name| {
        crate::theme::Theme::from_name_in(name, &view.user_themes)
            .1
            .is_some()
    }) {
        view.set_notice(format!(
            "saved {} but it did not read back from {}",
            names.join(", "),
            dir.display()
        ));
        reset(view);
        view.reopen_settings_keeping_sel();
        return Ok(());
    }
    reset(view);
    if names.len() == 1 {
        super::theme_ground::apply(view, &names[0]).await?;
    } else {
        view.set_notice(format!("saved themes: {}", names.join(", ")));
        view.reopen_settings_keeping_sel();
    }
    Ok(())
}

fn source_label(source: &crate::theme_import::Source) -> String {
    match source {
        crate::theme_import::Source::File(path) | crate::theme_import::Source::Folder(path) => {
            path.display().to_string()
        }
        crate::theme_import::Source::Url(url) => url.clone(),
    }
}
