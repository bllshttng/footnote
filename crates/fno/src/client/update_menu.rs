//! The update menu surface : the readiness payload structs, the
//! off-loop `--check` probe, the sideline menu + update modal builders, and
//! the queued `fno agents restart` verb. Split out of client.rs so the
//! over-budget file shrinks; everything here reaches the client's private
//! items through `super::*`.

use super::release_check::{probe_release, run_upgrade_verb, Channel, ReleaseOutcome};
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
    /// One row per running long-lived process (change 7). Tolerated
    /// absent so a payload from an older fno still parses; an empty list
    /// offers no restart action.
    #[serde(default)]
    pub(crate) running: Vec<RunningRow>,
    #[serde(default)]
    pub(crate) running_stale: usize,
    /// The source-pin verdict. Tolerated absent (an older Python
    /// payload); only `behind` is read here.
    #[serde(default)]
    pub(crate) source_pin: Option<SourcePinView>,
}

/// The slice of the source-pin answer the menu needs. Serde ignores
/// the pin's other keys; this struct does not set `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct SourcePinView {
    #[serde(default)]
    pub(crate) behind: Option<u64>,
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

/// Both off-loop answers the menu reads: the source-checkout readiness and
/// the published-release check. They land together from one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateProbe {
    pub(crate) readiness: UpdateOutcome,
    pub(crate) release: ReleaseOutcome,
}

impl From<UpdateOutcome> for UpdateProbe {
    fn from(readiness: UpdateOutcome) -> Self {
        UpdateProbe {
            readiness,
            release: ReleaseOutcome::NotApplicable,
        }
    }
}

/// Run the readiness check and the release check together, off the UI loop.
pub(crate) async fn probe_update() -> UpdateProbe {
    let (readiness, release) = tokio::join!(probe_update_readiness(), probe_release());
    UpdateProbe { readiness, release }
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
pub(crate) fn build_sideline_menu(anchor: Anchor, probe: Option<&UpdateProbe>) -> AuxPopup {
    let update = probe.map(|p| &p.readiness);
    let release = probe.map(|p| &p.release);
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
        enabled: true,
    };
    let mut rows = vec![PopupRow::Header("menu".into()), PopupRow::Rule];
    let mut actions = Vec::new();
    let behind = match update {
        Some(UpdateOutcome::Ok(r)) => r.source_pin.as_ref().and_then(|p| p.behind),
        _ => None,
    };
    // A probe still in flight (or never fired yet) builds the menu
    // WITHOUT an update row rather than waiting - the menu opens instantly.
    match update {
        Some(UpdateOutcome::Ok(r)) if r.update_ready => {
            rows.push(entry("⬆", "update ready"));
            actions.push(AuxAction::OpenUpdate);
        }
        // The source checkout is behind origin, so a sync (not an
        // update) is what's owed. Ranks above restart: a restart onto a
        // behind build still runs old code.
        Some(UpdateOutcome::Ok(_)) if behind.is_some_and(|n| n > 0) => {
            rows.push(entry(
                "⬆",
                &format!("source {} behind origin", behind.unwrap_or_default()),
            ));
            actions.push(AuxAction::OpenUpdate);
        }
        // A published release newer than a uv or brew install. Ranks
        // below a source update: a source install never reads Newer.
        _ if matches!(release, Some(ReleaseOutcome::Newer { .. })) => {
            if let Some(ReleaseOutcome::Newer { latest, .. }) = release {
                rows.push(entry("⬆", &format!("release {latest} available")));
            }
            actions.push(AuxAction::OpenUpdate);
        }
        // change 7: stale long-lived processes are their own reason
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
pub(crate) fn build_update_modal(probe: Option<&UpdateProbe>) -> AuxPopup {
    let outcome = probe.map(|p| &p.readiness);
    let mut rows = vec![PopupRow::Header("update".into()), PopupRow::Rule];
    let mut actions = Vec::new();
    match probe.map(|p| &p.release) {
        Some(ReleaseOutcome::Newer {
            channel,
            installed,
            latest,
        }) => {
            rows.push(PopupRow::Header(format!(
                "release {installed} -> {latest} ({})",
                channel.name()
            )));
            rows.push(PopupRow::Header(
                "upgrades the fno wheel; restart afterwards to run it".into(),
            ));
            rows.push(PopupRow::Entry {
                glyph: "⬆".into(),
                label: format!("upgrade now: {}", channel.upgrade_command()),
                hint: String::new(),
                enabled: true,
            });
            rows.push(PopupRow::Rule);
            actions.push(AuxAction::UpgradeRelease(*channel));
        }
        Some(ReleaseOutcome::Degraded(reason)) => {
            rows.push(PopupRow::Header(format!("release check failed: {reason}")));
            rows.push(PopupRow::Rule);
        }
        Some(ReleaseOutcome::Current { channel }) => {
            rows.push(PopupRow::Header(format!(
                "release: current ({})",
                channel.name()
            )));
            rows.push(PopupRow::Rule);
        }
        Some(ReleaseOutcome::NotApplicable) | None => {}
    }
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
            // change 7: one row per stale process naming what a
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

/// A queued update-modal verb. One runs at a time; its verdict is the notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateVerb {
    RestartAgents,
    Upgrade(Channel),
}

impl View {
    /// The verb's verdict lands as a notice, and a re-probe arms so the next
    /// menu shows the post-upgrade state.
    pub(crate) fn land_update_verdict(&mut self, verdict: String) {
        self.update_verb_inflight = false;
        self.set_notice(verdict);
        self.update_probe_want = true;
    }
}

pub(crate) async fn run_update_verb(verb: UpdateVerb) -> String {
    match verb {
        UpdateVerb::RestartAgents => run_restart_verb().await,
        UpdateVerb::Upgrade(channel) => run_upgrade_verb(channel).await,
    }
}

/// Run `fno agents restart` off the UI loop (change 7) and return
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
