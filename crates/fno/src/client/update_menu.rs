//! The update menu surface (x-f188): the readiness payload structs, the
//! off-loop `--check` probe, the sideline menu + update modal builders, and
//! the queued `fno agents restart` verb. Split out of client.rs so the
//! over-budget file shrinks; everything here reaches the client's private
//! items through `super::*`.

use super::*;

/// The client's view of `fno doctor update --check`'s payload - only
/// the fields the menu row and overlay render. `#[serde(default)]` on
/// `changelog` tolerates an absent key rather than failing the whole parse;
/// every other field is required, so a shape the Python resolver no longer
/// emits degrades the probe instead of silently rendering stale/zeroed data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct UpdateReadiness {
    pub(crate) update_ready: bool,
    pub(crate) installed_rev: Option<String>,
    pub(crate) source_rev: Option<String>,
    #[serde(default)]
    pub(crate) changelog: Vec<String>,
    pub(crate) guidance: String,
    pub(crate) degraded: Option<String>,
    /// One row per running long-lived process (x-f188 change 7). Tolerated
    /// absent so a payload from an older fno still parses; an empty list
    /// offers no restart action.
    #[serde(default)]
    pub(crate) running: Vec<RunningRow>,
    #[serde(default)]
    pub(crate) running_stale: usize,
}

/// One census row the modal renders: what a restart does to this process
/// and what survives it. The TUI renders and computes nothing (Locked
/// Decision 6, installed-fno-staleness.md); every string comes from Python.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct RunningRow {
    pub(crate) component: String,
    pub(crate) name: Option<String>,
    pub(crate) verdict: String,
    pub(crate) on_restart: String,
    pub(crate) survives: String,
}

/// The result of one `fno doctor update --check` probe: parsed
/// readiness, or a degraded reason (missing binary, non-zero exit, timeout,
/// unparseable JSON). Mirrors `connections_view::ReadOutcome` (Locked
/// Decision 4) - the TUI computes nothing beyond folding this into rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateOutcome {
    Ok(UpdateReadiness),
    Degraded(String),
}

/// Well above the Connections read timeout (1.5s): `--check` shells out to
/// `mux ls` (5s), `agents list` (15s), and `git log` (5s) SEQUENTIALLY on the
/// Python side, so its own worst-case latency alone is ~25s. This never
/// blocks the UI loop (the probe runs off it and the menu opens on whatever
/// outcome is already in hand), so there is no cost to sizing it well above
/// that worst case rather than racing it.
pub(crate) const UPDATE_PROBE_TIMEOUT: Duration = Duration::from_millis(30_000);

/// Run `fno doctor update --check` off the UI loop and fold it into an
/// [`UpdateOutcome`]. Mirrors `connections_view::read_json` exactly (Locked
/// Decision 4): the event loop never blocks on this subprocess: a
/// timeout, non-zero exit, or unparseable JSON all degrade rather than hang
/// or panic (AC6-EDGE).
///
/// `--check` already prints JSON on its own (`update` has no local `--json`
/// option, and the global `--json` flag only applies before the verb) - do
/// not add `--json` after `--check` here, it makes the CLI exit 2 and every
/// probe degrade (P1, codex on PR #881).
pub(crate) async fn probe_update_readiness() -> UpdateOutcome {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(["doctor", "update", "--check"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = match tokio::time::timeout(UPDATE_PROBE_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return UpdateOutcome::Degraded(format!("update --check: {e}")),
        Err(_) => return UpdateOutcome::Degraded("update --check: timed out".into()),
    };
    if !output.status.success() {
        return UpdateOutcome::Degraded(format!(
            "update --check: exit {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    match serde_json::from_slice::<UpdateReadiness>(&output.stdout) {
        Ok(r) => UpdateOutcome::Ok(r),
        Err(e) => UpdateOutcome::Degraded(format!("update --check: unparseable output ({e})")),
    }
}

/// Build the sideline MENU popup (US4), anchored at the footer's menu cell:
/// an update row (only when the last probe has landed and is ready or
/// degraded), then keybinds / settings / detach. `reload config` is
/// intentionally absent - there is no config-reload machinery to route it to
/// (a net-new capability, not a re-route), so the menu advertises only what
/// actually works.
pub(crate) fn build_sideline_menu(anchor: Anchor, update: Option<&UpdateOutcome>) -> AuxPopup {
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
        enabled: true,
    };
    let mut rows = vec![PopupRow::Header("menu".into()), PopupRow::Rule];
    let mut actions = Vec::new();
    // A probe still in flight (or never fired yet) builds the menu
    // WITHOUT an update row rather than waiting - the menu opens instantly.
    match update {
        Some(UpdateOutcome::Ok(r)) if r.update_ready => {
            rows.push(entry("⬆", "update ready"));
            actions.push(AuxAction::OpenUpdate);
        }
        // x-f188 change 7: stale long-lived processes are their own reason
        // to open the modal, even with no update pending.
        Some(UpdateOutcome::Ok(r)) if r.running_stale > 0 => {
            rows.push(entry(
                "⬆",
                &format!("restart: {} stale, panes kept", r.running_stale),
            ));
            actions.push(AuxAction::OpenUpdate);
        }
        // A successfully-parsed probe (Python always exits 0) can still be
        // internally degraded (e.g. `fno mux ls` failed inside the check).
        // Without this arm that state falls to `_ => {}` and the menu shows
        // nothing, hiding a real check failure from the operator.
        Some(UpdateOutcome::Ok(r)) if r.degraded.is_some() => {
            rows.push(entry("⬆", "update check degraded"));
            actions.push(AuxAction::OpenUpdate);
        }
        Some(UpdateOutcome::Degraded(_)) => {
            rows.push(entry("⬆", "update check failed"));
            actions.push(AuxAction::OpenUpdate);
        }
        _ => {}
    }
    rows.push(entry("♺", "sweep threads"));
    rows.push(entry("⌨", "keybinds"));
    rows.push(entry("⚙", "settings"));
    rows.push(entry("⇄", "connections"));
    rows.push(entry("⏏", "detach"));
    actions.push(AuxAction::OpenSweep);
    actions.push(AuxAction::OpenKeybinds);
    actions.push(AuxAction::OpenSettings);
    actions.push(AuxAction::OpenConnections);
    actions.push(AuxAction::Detach);
    AuxPopup {
        popup: Popup::new(rows, anchor),
        actions,
    }
}

/// Build the update-readiness overlay from the last probe outcome:
/// version pair, up to ten changelog subjects, a rule, then the one computed
/// guidance line - or, for a degraded probe, the degraded reason in the
/// guidance line's place. Never an empty body (AC5-HP/AC6-EDGE): `outcome`
/// is only `None` if this is somehow opened before any probe ever ran, which
/// `build_sideline_menu` never offers as a way in.
pub(crate) fn build_update_modal(outcome: Option<&UpdateOutcome>) -> AuxPopup {
    let mut rows = vec![PopupRow::Header("update".into()), PopupRow::Rule];
    match outcome {
        Some(UpdateOutcome::Ok(r)) => {
            let installed = r.installed_rev.as_deref().unwrap_or("unknown");
            let source = r.source_rev.as_deref().unwrap_or("unknown");
            rows.push(PopupRow::Header(format!("{installed} -> {source}")));
            if !r.changelog.is_empty() {
                rows.push(PopupRow::Rule);
                for subject in &r.changelog {
                    rows.push(PopupRow::Header(subject.clone()));
                }
            }
            rows.push(PopupRow::Rule);
            rows.push(PopupRow::Header(r.guidance.clone()));
            // x-f188 change 7: one row per stale process naming what a
            // restart does and what survives, then the fixed promise. The
            // tap is the confirmation, because the modal named every effect.
            let stale: Vec<&RunningRow> = r
                .running
                .iter()
                .filter(|row| row.verdict == "stale")
                .collect();
            if !stale.is_empty() {
                rows.push(PopupRow::Rule);
                for row in &stale {
                    let name = row.name.as_deref().unwrap_or("unnamed");
                    rows.push(PopupRow::Header(format!(
                        "{} {}: {}; keeps {}",
                        row.component, name, row.on_restart, row.survives
                    )));
                }
                rows.push(PopupRow::Header(
                    "restart keeps every pane. pane keepers stay on the old build \
                     until their pane ends."
                        .into(),
                ));
                rows.push(PopupRow::Rule);
                rows.push(PopupRow::Entry {
                    glyph: "↻".into(),
                    label: "restart now (keeps panes)".into(),
                    hint: String::new(),
                    enabled: true,
                });
            }
        }
        Some(UpdateOutcome::Degraded(reason)) => {
            rows.push(PopupRow::Header(format!("update check failed: {reason}")));
        }
        None => {
            rows.push(PopupRow::Header("update check has not run yet".into()));
        }
    }
    let mut actions = Vec::new();
    if matches!(
        outcome,
        Some(UpdateOutcome::Ok(r)) if r.running.iter().any(|row| row.verdict == "stale")
    ) {
        actions.push(AuxAction::RestartAgents);
    }
    AuxPopup {
        popup: Popup::new(rows, Anchor::Center)
            .title("update")
            .footer("esc close"),
        actions,
    }
}

/// Run `fno agents restart` off the UI loop (x-f188 change 7) and return
/// its verdict line for the notice: the last `fno agents restart:` stdout
/// line the verb printed, whatever it said. Text mode, never --json: the
/// verdict line IS the human receipt. Never --mux, never --force.
pub(crate) async fn run_restart_verb() -> String {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(["agents", "restart"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // Above the daemon restart's own worst case (30s SIGTERM grace + 2s
    // SIGKILL + 5s lock + a fresh start), so the verb is never cut off
    // mid-escalation by the UI's bound.
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = match tokio::time::timeout(Duration::from_secs(90), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return format!("restart spawn failed: {e}"),
        Err(_) => return "restart timed out after 90s".into(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("fno agents restart:"))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| {
            if output.status.success() {
                "restart finished without a verdict line".into()
            } else {
                format!("restart exited {} with no verdict line", output.status)
            }
        })
}
