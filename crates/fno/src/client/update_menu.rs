//! The update menu surface : the readiness payload structs, the
//! off-loop `--check` probe, the sideline menu + update modal builders, and
//! the foreground `fno agents restart --mux` runner. Split out of client.rs
//! so the over-budget file shrinks; everything here reaches the client's
//! private items through `super::*`.

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
pub(crate) fn build_sideline_menu(
    anchor: Anchor,
    probe: Option<&UpdateProbe>,
    backlog_on: bool,
) -> AuxPopup {
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
    rows.push(entry("＋", "new agent"));
    // The experimental backlog board: a toggle row always, the open
    // row only when on. Off by default (the pref's own default), so the
    // menu of an operator who never opted in is unchanged. The open row's
    // hint names the prefix chord that opens the board from anywhere the
    // menu is not (the pref gates it; the keybinds table documents it).
    let open_hint = crate::keys::key_for("open-backlog-board")
        .map(|k| format!("prefix {k}"))
        .unwrap_or_default();
    rows.push(entry(
        if backlog_on { "☑" } else { "☐" },
        "experimental: backlog view",
    ));
    if backlog_on {
        rows.push(PopupRow::Entry {
            glyph: "▦".into(),
            label: "backlog".into(),
            hint: open_hint,
            enabled: true,
        });
    }
    rows.push(entry("⌨", "keybinds"));
    rows.push(entry("⚙", "settings"));
    rows.push(entry("⇄", "connections"));
    rows.push(entry("⏏", "detach"));
    actions.push(AuxAction::OpenSweep);
    actions.push(AuxAction::OpenAgentLauncher);
    // Rows and actions pair by index: these two answer the backlog rows
    // pushed above, in the same order.
    actions.push(AuxAction::ToggleBacklogView);
    if backlog_on {
        actions.push(AuxAction::OpenBacklogView);
    }
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
                rows.push(PopupRow::Header(
                    "restart detaches, runs `fno agents restart --mux` in the \
                     foreground, then reattaches."
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

impl View {
    /// The verb's verdict lands as a notice, and a re-probe arms so the next
    /// menu shows the post-upgrade state.
    pub(crate) fn land_update_verdict(&mut self, verdict: String) {
        self.update_verb_inflight = false;
        self.set_notice(verdict);
        self.update_probe_want = true;
    }
}

/// Run `fno agents restart --mux` in the FOREGROUND with inherited stdio:
/// the caller has already restored the terminal, so the receipts print
/// live and are their own verdict - a captured run cannot carry them (the
/// mux leg kills this client's server out from under the TUI). Never
/// --force. Returns the verb's exit code.
pub(crate) async fn run_restart_foreground() -> i32 {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    match command.status().await {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fno: restart spawn failed: {e}");
            1
        }
    }
}

/// attach_and_run's sentinel for "unwound for the update-modal restart":
/// every other detach exit rides `exit_with_notice`, which is always 0, so
/// any nonzero code is unambiguous.
const RESTART_REATTACH_EXIT: i32 = 42;

/// The update-modal taps' shared guard (the modal named every effect, so
/// the tap is the confirmation): close the modal, and refuse while an
/// update verb is queued or in flight. True = proceed with the tapped one.
pub(crate) fn update_tap(view: &mut View) -> bool {
    view.aux = None;
    if view.update_verb_inflight || view.update_verb_want.is_some() {
        view.set_notice("an update action is already running".into());
        false
    } else {
        true
    }
}

/// The restart tap never queues: true arms the flag and detaches the
/// client, and run_inner runs the foreground restart and reattaches.
pub(crate) fn restart_tap(view: &mut View) -> bool {
    if update_tap(view) {
        view.restart_pending = true;
        true
    } else {
        false
    }
}

/// The detach break's exit code: the restart sentinel when the update
/// modal's tap armed one, else the plain detach notice path.
pub(crate) fn detach_exit(view: &View) -> i32 {
    if view.restart_pending {
        RESTART_REATTACH_EXIT
    } else {
        exit_with_notice("detached; run fno to reattach".into())
    }
}

/// run_inner's restart unwind: the sentinel means the TUI is gone (the
/// terminal guard dropped on the way out), so the verb runs where the
/// receipts are visible, then this process execs a fresh client for the
/// same session. exec, not an in-process restart: the detached client's
/// stdin thread holds a blocking stdin lock a second reader would deadlock
/// on. Some(failure) when the reattach itself failed; on success exec never
/// returns.
pub(crate) async fn maybe_reattach(code: i32, session: &str) -> Option<String> {
    if code != RESTART_REATTACH_EXIT {
        return None;
    }
    run_restart_foreground().await;
    use std::os::unix::process::CommandExt as _;
    let err = std::process::Command::new(crate::server::fno_bin())
        .arg("--session")
        .arg(session)
        .exec();
    Some(format!("restart finished; reattach failed: {err}"))
}

/// Kick a wanted release upgrade off the UI loop (the caller owns the
/// one-in-flight bound); its verdict lands through `tx` as a notice.
pub(crate) fn kick_upgrade(
    view: &mut View,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    channel: Channel,
) {
    view.update_verb_want = None;
    view.update_verb_inflight = true;
    tokio::spawn(async move {
        let verdict = run_upgrade_verb(channel).await;
        let _ = tx.send(verdict);
    });
}
