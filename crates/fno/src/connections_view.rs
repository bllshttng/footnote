//! The mux Connections modal (x-84d7): a stateful overlay listing managed
//! provider accounts + combos, driving register/use/remove/add and combo-order
//! edits through the `fno providers` CLI. The UI is a thin wrapper over that CLI
//! - it never writes provider records, combos, or runtime state directly (one
//! writer). Opened from the sideline MENU (`AuxAction::OpenConnections`).
//!
//! Structure mirrors the client's other stateful overlays (peek/nav): all pure
//! logic (JSON parse, state machine, render, key reducer) lives here and is unit
//! tested; `client.rs` owns only the thin async wiring (shell-out spawn, key
//! routing, compose branch). Reads are async fail-open shell-outs
//! (`needs_overlay::fold_now` idiom); a failed read degrades the modal, never
//! crashes it.

use serde::Deserialize;
use std::time::Duration;

/// Fail-open budget for a read shell-out, matching the needs/digest overlays: a
/// read slower than this degrades the modal with a visible banner (AC2-ERR),
/// never blocks the UI loop.
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

/// One provider record, as emitted by `fno providers list -J` (task 1.1).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub cli: String,
    pub auth: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub active: bool,
    /// Headroom state name (ok/low/exhausted/unknown). `unknown` is a
    /// first-class value rendered distinctly, never as healthy (x-d6be).
    #[serde(default = "unknown_headroom")]
    pub headroom: String,
    /// Snapshot age label for managed accounts; None for oauth/api-key records.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Session-local marker: a login pane was spawned for this id but register
    /// has not yet finalized. Never persisted (Locked Decision 6). Not on the
    /// wire - set client-side by the wizard.
    #[serde(skip)]
    pub pending_login: bool,
}

fn unknown_headroom() -> String {
    "unknown".to_string()
}

/// One combo, as emitted by `fno providers combos list -J` (task 1.1 added the
/// `active` field).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ComboRow {
    pub name: String,
    #[serde(default)]
    pub strategy: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub active: bool,
}

/// The two tabs of the modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Accounts,
    Order,
}

/// Overall modal lifecycle. `Degraded` carries the read failure to show in the
/// banner; while degraded, mutation keys are disabled (R retries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalState {
    Loading,
    Ready,
    Degraded(String),
}

/// The result of a full read fold: both lists, or a degrade reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Ok {
        accounts: Vec<Account>,
        combos: Vec<ComboRow>,
    },
    Degraded(String),
}

/// What a key press asks the async wrapper to do. The reducer stays pure by
/// returning an intent instead of touching the socket; `client.rs` executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnIntent {
    /// State changed in place - repaint only.
    Redraw,
    /// Key had no effect (ring the bell).
    Bell,
    /// Esc: close the modal.
    Close,
    /// R: re-run the reads (spawn a fresh fold).
    Refresh,
}

/// The Connections modal state. Owned by `View::connections` while open.
#[derive(Debug, Clone)]
pub struct ConnectionsView {
    pub tab: Tab,
    pub state: ModalState,
    pub accounts: Vec<Account>,
    pub combos: Vec<ComboRow>,
    /// Selection cursor in the Accounts tab.
    pub acct_sel: usize,
    /// Selection cursor over combos in the Order tab.
    pub combo_sel: usize,
    /// A single in-flight mutation guard (Locked Decision 5): a second action
    /// key while `acting` is a no-op with a footer notice.
    pub acting: bool,
    /// Footer notice (error tail / transient message), cleared on next action.
    pub notice: Option<String>,
    /// Generation token, bumped per open/refresh so a read landing after a newer
    /// refresh (or a close) is discarded.
    pub gen: u64,
}

impl ConnectionsView {
    /// Fresh modal in the loading state (reads are kicked by the caller).
    pub fn new() -> Self {
        Self {
            tab: Tab::Accounts,
            state: ModalState::Loading,
            accounts: Vec::new(),
            combos: Vec::new(),
            acct_sel: 0,
            combo_sel: 0,
            acting: false,
            notice: None,
            gen: 0,
        }
    }

    /// Apply a read fold under the caller's gen guard already checked. Clears the
    /// notice and clamps selections to the new list lengths.
    pub fn apply_read(&mut self, outcome: ReadOutcome) {
        match outcome {
            ReadOutcome::Ok { accounts, combos } => {
                self.accounts = accounts;
                self.combos = combos;
                self.state = ModalState::Ready;
                self.clamp_selection();
            }
            ReadOutcome::Degraded(reason) => {
                self.state = ModalState::Degraded(reason);
            }
        }
    }

    fn clamp_selection(&mut self) {
        if self.acct_sel >= self.accounts.len() {
            self.acct_sel = self.accounts.len().saturating_sub(1);
        }
        if self.combo_sel >= self.combos.len() {
            self.combo_sel = self.combos.len().saturating_sub(1);
        }
    }

    /// Pure key reducer. Returns the intent for the async wrapper to execute.
    /// Task 1.2 handles navigation + refresh + close; later tasks extend the
    /// intent set (actions, wizard, order commit).
    pub fn on_key(&mut self, key: u8) -> ConnIntent {
        // A degraded modal disables everything but refresh + close (AC2-ERR).
        let degraded = matches!(self.state, ModalState::Degraded(_));
        match key {
            0x1b => ConnIntent::Close, // Esc
            b'\t' => {
                self.tab = match self.tab {
                    Tab::Accounts => Tab::Order,
                    Tab::Order => Tab::Accounts,
                };
                self.notice = None;
                ConnIntent::Redraw
            }
            b'R' => ConnIntent::Refresh,
            b'j' if !degraded => self.move_sel(1),
            b'k' if !degraded => self.move_sel(-1),
            _ => {
                // No zero-feedback keypress: an unhandled key rings the bell so
                // the operator always gets feedback (AC1-UI).
                ConnIntent::Bell
            }
        }
    }

    fn move_sel(&mut self, delta: isize) -> ConnIntent {
        let (sel, len) = match self.tab {
            Tab::Accounts => (&mut self.acct_sel, self.accounts.len()),
            Tab::Order => (&mut self.combo_sel, self.combos.len()),
        };
        if len == 0 {
            return ConnIntent::Bell;
        }
        let next = (*sel as isize + delta).clamp(0, len as isize - 1) as usize;
        if next == *sel {
            return ConnIntent::Bell; // at the edge
        }
        *sel = next;
        ConnIntent::Redraw
    }

    /// Render the modal to overlay lines, padded to a uniform width so the
    /// inverse-video block is a clean rectangle (draw_lines_overlay does no
    /// padding of its own).
    pub fn render(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        out.push(self.tab_bar());
        out.push(String::new());
        match &self.state {
            ModalState::Loading => out.push("loading…".to_string()),
            ModalState::Degraded(reason) => {
                out.push(format!("fno unreachable: {reason}"));
                out.push("actions disabled - press R to retry".to_string());
            }
            ModalState::Ready => match self.tab {
                Tab::Accounts => self.render_accounts(&mut out),
                Tab::Order => self.render_order(&mut out),
            },
        }
        out.push(String::new());
        out.push(self.footer());
        if let Some(notice) = &self.notice {
            out.push(format!("! {notice}"));
        }
        pad_block(out)
    }

    fn tab_bar(&self) -> String {
        let (a, o) = match self.tab {
            Tab::Accounts => ("[Accounts]", " Order "),
            Tab::Order => (" Accounts ", "[Order]"),
        };
        format!("connections   {a}{o}")
    }

    fn render_accounts(&self, out: &mut Vec<String>) {
        if self.accounts.is_empty() {
            out.push("no accounts registered - press a to add".to_string());
            return;
        }
        for (i, a) in self.accounts.iter().enumerate() {
            let cursor = if i == self.acct_sel { ">" } else { " " };
            let badge = if a.active { "●" } else { " " };
            let snap = a
                .snapshot
                .as_deref()
                .map(|s| format!("  snap={s}"))
                .unwrap_or_default();
            let pending = if a.pending_login { "  …login pending" } else { "" };
            out.push(format!(
                "{cursor}{badge} {id}  [{cli}] {auth}  {headroom}{snap}{pending}",
                id = a.id,
                cli = a.cli,
                auth = a.auth,
                headroom = a.headroom,
            ));
        }
    }

    fn render_order(&self, out: &mut Vec<String>) {
        if self.combos.is_empty() {
            out.push("no combos - press n to add".to_string());
            return;
        }
        for (i, c) in self.combos.iter().enumerate() {
            let cursor = if i == self.combo_sel { ">" } else { " " };
            let badge = if c.active { "●" } else { " " };
            out.push(format!(
                "{cursor}{badge} {name}  [{strategy}]",
                name = c.name,
                strategy = c.strategy,
            ));
            if i == self.combo_sel {
                for (m, member) in c.members.iter().enumerate() {
                    out.push(format!("      {n}. {member}", n = m + 1));
                }
            }
        }
    }

    fn footer(&self) -> String {
        match self.tab {
            Tab::Accounts => "Tab: order   j/k: move   R: refresh   Esc: close".to_string(),
            Tab::Order => "Tab: accounts   j/k: move   R: refresh   Esc: close".to_string(),
        }
    }
}

impl Default for ConnectionsView {
    fn default() -> Self {
        Self::new()
    }
}

/// Pad every line to the max width so the inverse block is a rectangle.
fn pad_block(mut lines: Vec<String>) -> Vec<String> {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    for line in &mut lines {
        let pad = width - line.chars().count();
        line.push_str(&" ".repeat(pad));
    }
    lines
}

// ── async reads (fail-open shell-outs, the needs_overlay idiom) ─────────────

/// Parse `fno providers list -J` stdout. `None` on unparseable output (torn
/// stdout degrades the read rather than crashing the modal).
pub fn parse_accounts(stdout: &[u8]) -> Option<Vec<Account>> {
    serde_json::from_slice(stdout).ok()
}

/// Parse `fno providers combos list -J` stdout.
pub fn parse_combos(stdout: &[u8]) -> Option<Vec<ComboRow>> {
    serde_json::from_slice(stdout).ok()
}

/// Run both reads in parallel and fold into a [`ReadOutcome`]. Any read failing
/// (missing `fno`, nonzero exit, unparseable JSON, timeout) degrades the whole
/// modal with a named reason (AC2-ERR) - the CLI is the single source, so a
/// partial render would be a silent lie.
pub async fn load_all() -> ReadOutcome {
    let (acc, com) = tokio::join!(
        read_json(&["providers", "list", "-J"]),
        read_json(&["providers", "combos", "list", "-J"]),
    );
    let accounts = match acc {
        Ok(bytes) => match parse_accounts(&bytes) {
            Some(v) => v,
            None => return ReadOutcome::Degraded("providers list: unparseable output".into()),
        },
        Err(e) => return ReadOutcome::Degraded(format!("providers list: {e}")),
    };
    let combos = match com {
        Ok(bytes) => match parse_combos(&bytes) {
            Some(v) => v,
            None => return ReadOutcome::Degraded("combos list: unparseable output".into()),
        },
        Err(e) => return ReadOutcome::Degraded(format!("combos list: {e}")),
    };
    ReadOutcome::Ok { accounts, combos }
}

/// One `fno <args>` read shell-out with the fail-open budget. Resolves the `fno`
/// binary from `$FNO_BIN` else PATH (the mux shells the deployed CLI).
async fn read_json(args: &[&str]) -> Result<Vec<u8>, String> {
    let fut = tokio::process::Command::new(fno_bin())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(READ_TIMEOUT, fut)
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("exit {}", output.status.code().unwrap_or(-1)));
    }
    Ok(output.stdout)
}

/// Resolve the `fno` binary: `$FNO_BIN`, else bare `fno` on PATH.
pub(crate) fn fno_bin() -> std::path::PathBuf {
    if let Some(v) = std::env::var_os("FNO_BIN") {
        return std::path::PathBuf::from(v);
    }
    std::path::PathBuf::from("fno")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_accounts() -> Vec<Account> {
        parse_accounts(
            br#"[
              {"id":"ccm","name":"CCM","cli":"claude","auth":"managed","priority":10,"active":true,"headroom":"ok","snapshot":"2h"},
              {"id":"ccr","name":"CCR","cli":"claude","auth":"managed","priority":20,"active":false,"headroom":"low","snapshot":"5d"},
              {"id":"glm","name":"GLM","cli":"claude","auth":"api_key","priority":30,"active":false,"headroom":"unknown","snapshot":null}
            ]"#,
        )
        .expect("valid accounts json")
    }

    fn sample_combos() -> Vec<ComboRow> {
        parse_combos(
            br#"[{"name":"main","strategy":"fallback","members":["ccm","ccr","glm"],"active":true}]"#,
        )
        .expect("valid combos json")
    }

    #[test]
    fn parses_account_rows_including_unknown_headroom() {
        let accts = sample_accounts();
        assert_eq!(accts.len(), 3);
        assert!(accts[0].active);
        assert_eq!(accts[2].headroom, "unknown");
        assert_eq!(accts[2].snapshot, None);
    }

    #[test]
    fn missing_optional_fields_default() {
        // A minimal row (no priority/active/headroom/snapshot) parses.
        let accts =
            parse_accounts(br#"[{"id":"x","cli":"codex","auth":"managed"}]"#).expect("parses");
        assert_eq!(accts[0].headroom, "unknown");
        assert!(!accts[0].active);
        assert_eq!(accts[0].priority, 0);
    }

    #[test]
    fn torn_json_fails_quiet() {
        assert!(parse_accounts(b"[{not json").is_none());
        assert!(parse_combos(b"nope").is_none());
    }

    #[test]
    fn empty_arrays_parse_clean() {
        assert_eq!(parse_accounts(b"[]").unwrap().len(), 0);
        assert_eq!(parse_combos(b"[]").unwrap().len(), 0);
    }

    fn ready_view() -> ConnectionsView {
        let mut v = ConnectionsView::new();
        v.apply_read(ReadOutcome::Ok {
            accounts: sample_accounts(),
            combos: sample_combos(),
        });
        v
    }

    // AC1-HP: modal lists accounts (badge, cli, auth, headroom incl unknown) and
    // combos with ordered members + active badge.
    #[test]
    fn renders_accounts_with_active_badge_and_unknown_headroom() {
        let v = ready_view();
        let out = v.render().join("\n");
        assert!(out.contains("ccm"));
        assert!(out.contains("[claude] managed"));
        assert!(out.contains("●")); // active badge on ccm
        assert!(out.contains("unknown")); // glm headroom shown, not hidden
        assert!(out.contains("snap=2h"));
    }

    #[test]
    fn order_tab_shows_members_in_order_with_active_badge() {
        let mut v = ready_view();
        v.on_key(b'\t'); // -> Order
        assert_eq!(v.tab, Tab::Order);
        let out = v.render().join("\n");
        assert!(out.contains("main"));
        assert!(out.contains("[fallback]"));
        assert!(out.contains("●")); // active combo badge
        // members numbered in rotation order under the selected combo
        assert!(out.contains("1. ccm"));
        assert!(out.contains("2. ccr"));
        assert!(out.contains("3. glm"));
    }

    // AC1-EDGE: empty world renders both tabs' empty states, no read error.
    #[test]
    fn empty_world_renders_add_hints() {
        let mut v = ConnectionsView::new();
        v.apply_read(ReadOutcome::Ok {
            accounts: vec![],
            combos: vec![],
        });
        let acct = v.render().join("\n");
        assert!(acct.contains("no accounts registered"));
        v.on_key(b'\t');
        let order = v.render().join("\n");
        assert!(order.contains("no combos"));
    }

    // AC2-ERR: a degraded read shows the banner + disables mutation keys.
    #[test]
    fn degraded_read_shows_banner_and_disables_nav() {
        let mut v = ConnectionsView::new();
        v.apply_read(ReadOutcome::Degraded("fno: not found".into()));
        let out = v.render().join("\n");
        assert!(out.contains("fno unreachable: fno: not found"));
        assert!(out.contains("press R to retry"));
        // j/k are inert while degraded (bell, no move); R still refreshes.
        assert_eq!(v.on_key(b'j'), ConnIntent::Bell);
        assert_eq!(v.on_key(b'R'), ConnIntent::Refresh);
        assert_eq!(v.on_key(0x1b), ConnIntent::Close);
    }

    #[test]
    fn tab_switch_and_selection_move() {
        let mut v = ready_view();
        assert_eq!(v.on_key(b'j'), ConnIntent::Redraw);
        assert_eq!(v.acct_sel, 1);
        assert_eq!(v.on_key(b'k'), ConnIntent::Redraw);
        assert_eq!(v.acct_sel, 0);
        // at the top edge, k is a bell (no wrap)
        assert_eq!(v.on_key(b'k'), ConnIntent::Bell);
        assert_eq!(v.on_key(b'\t'), ConnIntent::Redraw);
        assert_eq!(v.tab, Tab::Order);
    }

    // AC1-UI: no zero-feedback keypress - an unbound key rings the bell.
    #[test]
    fn unbound_key_rings_bell() {
        let mut v = ready_view();
        assert_eq!(v.on_key(b'z'), ConnIntent::Bell);
    }

    #[test]
    fn apply_read_clamps_stale_selection() {
        let mut v = ready_view();
        v.acct_sel = 2;
        // A refresh returning fewer accounts must not leave the cursor OOB.
        v.apply_read(ReadOutcome::Ok {
            accounts: vec![sample_accounts()[0].clone()],
            combos: sample_combos(),
        });
        assert_eq!(v.acct_sel, 0);
    }
}
