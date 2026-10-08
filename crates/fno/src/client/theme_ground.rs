//! The mux theme surface: the launch latch, a live theme switch's ground
//! repaint, and the settings-modal apply. One module so `client.rs` (shrink
//! -only) answers one question fewer.

use std::path::Path;

use super::{Theme, View};
use crate::proto::Color;

/// A theme switch's ground repaint, staged by `ApplyTheme` and drained by
/// the run loop's stdin branch. The loop owns the compositor and the exit
/// guard, so the handler can only stage the intent here.
pub(super) struct PendingGround {
    /// The OSC bytes to write now: the theme's `ground_set`, or
    /// `GROUND_RESTORE` when the theme paints none.
    pub(super) osc: Vec<u8>,
    /// The new compositor ground (the theme base), `None` when it paints
    /// none.
    pub(super) ground: Option<Color>,
}

/// The repaint a theme switch stages: the theme's OSC ground set, or
/// `GROUND_RESTORE` when it paints none, plus the new compositor ground.
/// `None` when the paint_background kill switch is off - the takeover never
/// started, so a switch must not start one. The flag is an argument so
/// tests never read the live config ladder.
pub(super) fn ground_repaint(theme: &Theme, paint: bool) -> Option<PendingGround> {
    if !paint {
        return None;
    }
    Some(PendingGround {
        osc: crate::theme::ground_set(theme)
            .unwrap_or_else(|| crate::theme::GROUND_RESTORE.to_vec()),
        ground: match theme.base {
            Color::Default => None,
            c => Some(c),
        },
    })
}

/// The ground-paint rule: an INFERRED light theme (no config named one; the
/// COLORFGBG ladder picked paper) never repaints the ground. The terminal's
/// own light bg and fg stay as the user had them; the theme contributes the
/// chrome colors only (the x-41c8 ruling). An explicit theme pick, the dark
/// default, and the `paint_background` kill switch keep their behavior.
pub(super) fn ground_paint_allowed(paint_enabled: bool, inferred: bool, theme: &Theme) -> bool {
    paint_enabled && !(inferred && crate::theme::is_light(theme))
}

/// Apply a staged theme-switch ground repaint: write the OSC bytes, move
/// the compositor ground, and latch the exit restore. The takeover gate
/// (paint_background) was already checked at arm time.
pub(super) fn drain_pending_ground(
    view: &mut View,
    compositor: &mut super::compositor::Compositor,
    guard: &mut super::launch::TerminalGuard,
) {
    let Some(p) = view.pending_ground.take() else {
        return;
    };
    let _ = super::raw_out(&p.osc);
    compositor.set_ground(p.ground);
    guard.latch_ground();
}

/// What attach latches from the config ladder before the first paint: the
/// chrome theme (and its warning), the OSC ground bytes, and the
/// compositor's ground color. The user's own themes latch onto the view
/// here too - the picker lists them and a live apply resolves through the
/// table, so a mid-session config edit lands on the next attach, the same
/// posture as hover_focus.
pub(super) struct LaunchTheme {
    pub(super) theme: Theme,
    pub(super) theme_warn: Option<crate::keys::KeymapWarning>,
    pub(super) ground: Option<Vec<u8>>,
    pub(super) ground_color: Option<Color>,
}

pub(super) fn launch_theme(cwd: &Path, view: &mut View) -> LaunchTheme {
    let (theme, theme_warn, inferred) = crate::digest_overlay::theme_for(cwd);
    // The OSC ground: set + restore ride together through the kill switch,
    // so an operator who opts out gets byte-for-byte the old launch. The
    // paint rule (an inferred light theme never repaints) is
    // [`ground_paint_allowed`], pinned by its test.
    let paint = ground_paint_allowed(
        crate::digest_overlay::paint_background_enabled(cwd),
        inferred,
        &theme,
    );
    let ground = if paint {
        crate::theme::ground_set(&theme)
    } else {
        None
    };
    let ground_color = if paint {
        match theme.base {
            Color::Default => None,
            c => Some(c),
        }
    } else {
        None
    };
    view.theme = theme;
    view.user_themes = crate::digest_overlay::user_themes(cwd).0;
    LaunchTheme {
        theme,
        theme_warn,
        ground,
        ground_color,
    }
}

/// Run one ApplyTheme from the settings picker: resolve the name across the
/// built-ins and the view's latched user table, fold the role overrides,
/// swap the in-memory theme, stage the ground repaint, then persist via the
/// CLI - the mux never writes config itself, mirroring the rule that it
/// never writes the graph. On a write failure the in-memory theme STAYS
/// (applied this session) and the notice says so honestly, never claiming
/// a persistence it did not achieve.
pub(super) async fn apply(view: &mut View, name: &str) -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (theme, warn) = crate::digest_overlay::theme_role_overrides(
        Path::new(&cwd),
        Theme::from_name_in(name, &view.user_themes),
    );
    view.theme = theme;
    // The ground is a terminal property, not a view one: swapping the theme
    // alone left the OLD theme's OSC ground painted (the bug: a fresh
    // launch showed the new ground, a live switch kept the old one). Stage
    // the repaint for the run loop, which owns the compositor and the exit
    // guard - GROUND_RESTORE when the new theme paints none.
    view.pending_ground = ground_repaint(
        &theme,
        crate::digest_overlay::paint_background_enabled(Path::new(&cwd)),
    );
    let notice = match super::config_set::spawn_config_set("mux.theme", name).await {
        Ok(()) => match warn {
            None => format!("theme: {name}"),
            Some(w) => w.0,
        },
        Err(_) => format!("theme {name} applied this session; save failed"),
    };
    view.set_notice(notice);
    view.reopen_settings_keeping_sel();
    Ok(())
}
