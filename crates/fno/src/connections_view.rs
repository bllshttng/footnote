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

// Reuse the server's `fno` binary resolver ($FNO_BIN, else the running exe) so
// the modal's shell-outs and the server never drift (PR #421 review).
use crate::server::fno_bin;

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
    /// Run a single-flight `fno <argv>` mutation, then re-read. The reducer only
    /// yields this when not already acting (single-flight, Locked Decision 5).
    Run(Vec<String>),
    /// Run a single-flight `fno <argv>` mutation with extra child env (register
    /// needs `CLAUDE_CONFIG_DIR`), then re-read.
    RunEnv {
        argv: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Spawn the interactive login pane via `fno mux pane run` (the documented
    /// PaneRun front door - zero proto bump). Not single-flight/re-read: it opens
    /// a pane and returns; the modal marks the id pending-login locally.
    SpawnLogin(Vec<String>),
    /// (x-c914) Set the client's session-local active account to the carried
    /// value (the post-toggle state: `Some(id)`, or `None` when toggled off).
    /// Shells NOTHING - the client mirrors this into its own `active_account`
    /// so later spawns append `--account`. Distinct from the `use` verb (the
    /// global credential slot-swap); this only routes NEW spawns (Locked
    /// Decision 1).
    SetActiveAccount(Option<String>),
}

/// A session-local pending login: a wizard spawned its login pane but register
/// has not finalized yet. Rendered as a synthetic Accounts row; never persisted
/// (Locked Decision 6). `dir` is the claude config dir (empty for codex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLogin {
    pub id: String,
    pub cli: String,
    pub dir: String,
}

/// The `a`dd wizard: collect an id, pick a cli/kind, and (claude) a config dir,
/// then spawn a login pane or run the api-key add. One wizard at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wizard {
    pub step: WizardStep,
    pub id: String,
    pub kind: AddKind,
    pub dir: String,
    /// api-key kind: the key value typed (base_url is the GLM preset).
    pub api_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Id,
    Kind,
    Dir,
    ApiKey,
}

/// The `n` new-combo form (Order tab): a name then a comma-separated member list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboInput {
    pub step: ComboInputStep,
    pub name: String,
    pub members: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComboInputStep {
    Name,
    Members,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddKind {
    /// Managed claude account: shares the ONE `~/.claude` slot (managed.py). The
    /// login runs in `~/.claude` and register snapshots that login into the
    /// managed store; `use` swaps the Keychain token back into `~/.claude`. All
    /// accounts reuse your one config (skills/hooks/plugins) - the default.
    Claude,
    /// Isolated claude account: its OWN `CLAUDE_CONFIG_DIR` (a separate config
    /// dir + Keychain item). Opt-in - most users want the shared slot above.
    ClaudeIsolated,
    Codex,
    ApiKeyGlm,
}

impl AddKind {
    fn cli(self) -> &'static str {
        match self {
            AddKind::Claude | AddKind::ClaudeIsolated => "claude",
            AddKind::Codex => "codex",
            // GLM is a claude-cli provider fronted by a base_url + api key.
            AddKind::ApiKeyGlm => "claude",
        }
    }
    fn label(self) -> &'static str {
        match self {
            AddKind::Claude => "claude (shared ~/.claude)",
            AddKind::ClaudeIsolated => "claude (isolated config dir)",
            AddKind::Codex => "codex (oauth login)",
            AddKind::ApiKeyGlm => "api-key: GLM/z.ai",
        }
    }
    fn next(self) -> AddKind {
        match self {
            AddKind::Claude => AddKind::ClaudeIsolated,
            AddKind::ClaudeIsolated => AddKind::Codex,
            AddKind::Codex => AddKind::ApiKeyGlm,
            AddKind::ApiKeyGlm => AddKind::Claude,
        }
    }
}

/// The GLM/z.ai base_url preset (Claude's Discretion 4: preset-first is fine).
const GLM_BASE_URL: &str = "https://api.z.ai/api/anthropic";

/// Validate a new-account id BEFORE any subprocess: lowercase alphanumeric +
/// hyphens, leading letter (the register verb's `_ID_PATTERN` contract).
pub fn valid_account_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Expand a leading `~/` to the home dir. `env`/exec do NOT expand `~`, so a
/// literal `~/.claude-<id>` in `CLAUDE_CONFIG_DIR` would resolve against the
/// spawned process's cwd, not `$HOME`. Pure (home injected) for testability;
/// falls back to the input verbatim when there is no `~/` prefix or no home.
pub fn expand_tilde(path: &str, home: Option<&std::ffi::OsStr>) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(h) = home {
            let mut p = std::path::PathBuf::from(h);
            p.push(rest);
            return p.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// A pending destructive action awaiting the operator's one-key confirm. `label`
/// is shown in the footer prompt; `argv` is what Enter runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConfirm {
    pub label: String,
    pub argv: Vec<String>,
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
    /// A pending remove confirm (Enter runs, any other key cancels). While
    /// `Some`, keys route to the confirm (AC1-ERR / destructive-guard).
    pub confirm: Option<PendingConfirm>,
    /// Member cursor within the selected combo's list (Order tab).
    pub member_sel: usize,
    /// Buffered member order for the selected combo while reordering (Order tab).
    /// `None` = viewing committed order; `Some` = dirty, committed on Enter,
    /// reverted on Esc / combo switch (AC2-FR: never a verb per keystroke).
    pub dirty_order: Option<Vec<String>>,
    /// The `n` new-combo input form (name -> providers csv), Order tab.
    pub combo_input: Option<ComboInput>,
    /// The `a`dd wizard, `Some` while collecting inputs. Keys divert to it.
    pub wizard: Option<Wizard>,
    /// Session-local pending logins, rendered as synthetic Accounts rows after
    /// the real records. Never persisted; register is the durable step.
    pub pending: Vec<PendingLogin>,
    /// Generation token, bumped per open/refresh so a read landing after a newer
    /// refresh (or a close) is discarded.
    pub gen: u64,
    /// (x-c914) The client's session-local active account, mirrored here for the
    /// "active for spawns" marker. Seeded from the client on open and re-set by
    /// the set-active toggle (`act_set_active`); the client reads the yielded
    /// `SetActiveAccount` intent back into its own authoritative copy. Never a
    /// credential mutation (Locked Decisions 1-2).
    pub active_account: Option<String>,
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
            confirm: None,
            member_sel: 0,
            dirty_order: None,
            combo_input: None,
            wizard: None,
            pending: Vec::new(),
            gen: 0,
            active_account: None,
        }
    }

    /// (x-c914) Seed the "active for spawns" marker from the client's current
    /// session-local active account when the modal opens, so the marker is
    /// correct on first paint (the client owns the authoritative value across
    /// modal open/close).
    pub fn with_active_account(mut self, account: Option<String>) -> Self {
        self.active_account = account;
        self
    }

    /// The combo the Order-tab cursor is on, if any.
    fn selected_combo(&self) -> Option<&ComboRow> {
        self.combos.get(self.combo_sel)
    }

    /// The member order currently shown for the selected combo: the dirty buffer
    /// if reordering, else the committed order.
    fn shown_members(&self) -> Vec<String> {
        if let Some(dirty) = &self.dirty_order {
            return dirty.clone();
        }
        self.selected_combo()
            .map(|c| c.members.clone())
            .unwrap_or_default()
    }

    /// Total selectable Accounts rows: real records + session-local pending logins.
    fn accounts_len(&self) -> usize {
        self.accounts.len() + self.pending.len()
    }

    /// The pending login at Accounts-tab cursor `acct_sel`, if the cursor sits on
    /// a synthetic pending row (past the real records).
    pub fn selected_pending(&self) -> Option<&PendingLogin> {
        self.acct_sel
            .checked_sub(self.accounts.len())
            .and_then(|i| self.pending.get(i))
    }

    /// The account the Accounts-tab cursor is on, if any.
    pub fn selected_account(&self) -> Option<&Account> {
        self.accounts.get(self.acct_sel)
    }

    /// Apply a read fold under the caller's gen guard already checked. Clears the
    /// notice and clamps selections to the new list lengths.
    pub fn apply_read(&mut self, outcome: ReadOutcome) {
        match outcome {
            ReadOutcome::Ok { accounts, combos } => {
                // A pending login that now has a real record has been registered;
                // drop its synthetic row (register is the durable step).
                let real: std::collections::HashSet<&str> =
                    accounts.iter().map(|a| a.id.as_str()).collect();
                self.pending.retain(|p| !real.contains(p.id.as_str()));
                self.accounts = accounts;
                self.combos = combos;
                self.state = ModalState::Ready;
                // Fresh data is the committed truth; drop any stale reorder buffer.
                self.dirty_order = None;
                self.clamp_selection();
            }
            ReadOutcome::Degraded(reason) => {
                self.state = ModalState::Degraded(reason);
            }
        }
    }

    fn clamp_selection(&mut self) {
        let alen = self.accounts_len();
        if self.acct_sel >= alen {
            self.acct_sel = alen.saturating_sub(1);
        }
        if self.combo_sel >= self.combos.len() {
            self.combo_sel = self.combos.len().saturating_sub(1);
        }
        let mlen = self.shown_members().len();
        if self.member_sel >= mlen {
            self.member_sel = mlen.saturating_sub(1);
        }
    }

    /// Pure key reducer. Returns the intent for the async wrapper to execute.
    /// Task 1.2 handles navigation + refresh + close; later tasks extend the
    /// intent set (actions, wizard, order commit).
    pub fn on_key(&mut self, key: u8) -> ConnIntent {
        // The add wizard, while open, captures all keys (typed input + toggles).
        if self.wizard.is_some() {
            return self.wizard_key(key);
        }
        // The new-combo form captures all keys while open.
        if self.combo_input.is_some() {
            return self.combo_input_key(key);
        }
        // A pending confirm captures all keys: Enter commits, anything else
        // cancels (no destructive action without an explicit confirm).
        if let Some(pending) = self.confirm.take() {
            if key == b'\r' || key == b'\n' {
                self.acting = true;
                self.notice = None;
                return ConnIntent::Run(pending.argv);
            }
            self.notice = Some("cancelled".to_string());
            return ConnIntent::Redraw;
        }

        // A degraded modal disables everything but refresh + close (AC2-ERR).
        let degraded = matches!(self.state, ModalState::Degraded(_));
        // Esc discards an uncommitted reorder before it closes the modal (AC2-FR).
        if key == 0x1b {
            if self.dirty_order.take().is_some() {
                self.notice = Some("reorder discarded".to_string());
                return ConnIntent::Redraw;
            }
            return ConnIntent::Close;
        }
        match key {
            b'\t' => {
                self.tab = match self.tab {
                    Tab::Accounts => Tab::Order,
                    Tab::Order => Tab::Accounts,
                };
                // Switching tabs abandons an uncommitted reorder (AC2-FR).
                self.dirty_order = None;
                self.notice = None;
                ConnIntent::Redraw
            }
            b'R' => ConnIntent::Refresh,
            _ if degraded => ConnIntent::Bell,
            _ if self.tab == Tab::Accounts => self.accounts_key(key),
            _ => self.order_key(key),
        }
    }

    /// Accounts-tab keys (not degraded).
    fn accounts_key(&mut self, key: u8) -> ConnIntent {
        match key {
            b'j' => self.move_sel(1),
            b'k' => self.move_sel(-1),
            b'u' => self.act_use(),
            b's' => self.act_set_active(),
            b'd' => self.act_remove(),
            b'a' => {
                self.wizard = Some(Wizard {
                    step: WizardStep::Id,
                    id: String::new(),
                    kind: AddKind::Claude,
                    dir: String::new(),
                    api_key: String::new(),
                });
                self.notice = None;
                ConnIntent::Redraw
            }
            b'r' => self.act_register(),
            _ => ConnIntent::Bell,
        }
    }

    /// Order-tab keys (not degraded): j/k move the member cursor, h/l switch
    /// combos, J/K reorder (buffered), Enter commits, space activates, d removes,
    /// n creates.
    fn order_key(&mut self, key: u8) -> ConnIntent {
        match key {
            b'h' | b'l' => {
                // Switch the selected combo; abandon any uncommitted reorder.
                let n = self.combos.len();
                if n == 0 {
                    return ConnIntent::Bell;
                }
                let delta: isize = if key == b'l' { 1 } else { -1 };
                let next = (self.combo_sel as isize + delta).clamp(0, n as isize - 1) as usize;
                if next == self.combo_sel {
                    return ConnIntent::Bell;
                }
                self.combo_sel = next;
                self.member_sel = 0;
                self.dirty_order = None;
                ConnIntent::Redraw
            }
            b'j' | b'k' => {
                let len = self.shown_members().len();
                if len == 0 {
                    return ConnIntent::Bell;
                }
                let delta: isize = if key == b'j' { 1 } else { -1 };
                let next = (self.member_sel as isize + delta).clamp(0, len as isize - 1) as usize;
                if next == self.member_sel {
                    return ConnIntent::Bell;
                }
                self.member_sel = next;
                ConnIntent::Redraw
            }
            b'J' | b'K' => self.reorder_member(if key == b'J' { 1 } else { -1 }),
            b'\r' | b'\n' => self.commit_reorder(),
            b' ' => self.act_activate_combo(),
            b'd' => self.act_remove_combo(),
            b'n' => {
                self.combo_input = Some(ComboInput {
                    step: ComboInputStep::Name,
                    name: String::new(),
                    members: String::new(),
                });
                self.notice = None;
                ConnIntent::Redraw
            }
            _ => ConnIntent::Bell,
        }
    }

    /// Buffer a member move at `member_sel` by `delta` (down=+1/up=-1). Seeds the
    /// dirty buffer from the committed order on the first move; the member cursor
    /// follows the moved member.
    fn reorder_member(&mut self, delta: isize) -> ConnIntent {
        let mut members = self.shown_members();
        let len = members.len();
        if len < 2 {
            return ConnIntent::Bell;
        }
        let from = self.member_sel;
        let to = from as isize + delta;
        if to < 0 || to >= len as isize {
            return ConnIntent::Bell; // at the edge
        }
        let to = to as usize;
        members.swap(from, to);
        self.member_sel = to;
        self.dirty_order = Some(members);
        ConnIntent::Redraw
    }

    /// Enter: commit a dirty reorder as one atomic `combos update` (AC4-HP). No
    /// dirty buffer -> a bell (nothing to commit); this is not a zero-feedback
    /// no-op (AC1-UI).
    fn commit_reorder(&mut self) -> ConnIntent {
        let Some(order) = self.dirty_order.take() else {
            return ConnIntent::Bell;
        };
        let Some(name) = self.selected_combo().map(|c| c.name.clone()) else {
            return ConnIntent::Bell;
        };
        if self.acting {
            self.dirty_order = Some(order);
            self.notice = Some("busy - one action at a time".to_string());
            return ConnIntent::Redraw;
        }
        self.acting = true;
        self.notice = None;
        ConnIntent::Run(vec![
            "providers".into(),
            "combos".into(),
            "update".into(),
            name,
            "--providers".into(),
            order.join(","),
        ])
    }

    /// space: set the selected combo active. Refused with a notice when it has a
    /// dangling member (AC2-EDGE) - activating a broken combo is a footgun.
    fn act_activate_combo(&mut self) -> ConnIntent {
        if self.acting {
            self.notice = Some("busy - one action at a time".to_string());
            return ConnIntent::Redraw;
        }
        let known: std::collections::HashSet<&str> =
            self.accounts.iter().map(|a| a.id.as_str()).collect();
        let Some(combo) = self.selected_combo() else {
            return ConnIntent::Bell;
        };
        if let Some(missing) = combo.members.iter().find(|m| !known.contains(m.as_str())) {
            self.notice = Some(format!(
                "combo {} has a dangling member {missing} - fix the order first",
                combo.name
            ));
            return ConnIntent::Redraw;
        }
        let name = combo.name.clone();
        self.acting = true;
        self.notice = None;
        ConnIntent::Run(vec![
            "providers".into(),
            "combos".into(),
            "use".into(),
            name,
        ])
    }

    /// d: remove the selected combo behind an Enter-confirm.
    fn act_remove_combo(&mut self) -> ConnIntent {
        if self.acting {
            self.notice = Some("busy - one action at a time".to_string());
            return ConnIntent::Redraw;
        }
        let Some(combo) = self.selected_combo() else {
            return ConnIntent::Bell;
        };
        let name = combo.name.clone();
        self.confirm = Some(PendingConfirm {
            label: format!("remove combo {name}? (Enter=yes, any key=no)"),
            argv: vec!["providers".into(), "combos".into(), "remove".into(), name],
        });
        ConnIntent::Redraw
    }

    /// Handle a key while the new-combo form is open (name -> members csv).
    fn combo_input_key(&mut self, key: u8) -> ConnIntent {
        if key == 0x1b {
            self.combo_input = None;
            self.notice = Some("new combo cancelled".to_string());
            return ConnIntent::Redraw;
        }
        let Some(ci) = self.combo_input.as_mut() else {
            return ConnIntent::Bell;
        };
        let field = match ci.step {
            ComboInputStep::Name => &mut ci.name,
            ComboInputStep::Members => &mut ci.members,
        };
        match key {
            b'\r' | b'\n' => match ci.step {
                ComboInputStep::Name => {
                    if ci.name.is_empty() {
                        self.notice = Some("combo name cannot be empty".into());
                        return ConnIntent::Redraw;
                    }
                    ci.step = ComboInputStep::Members;
                    ConnIntent::Redraw
                }
                ComboInputStep::Members => {
                    if ci.members.trim().is_empty() {
                        self.notice = Some("members cannot be empty".into());
                        return ConnIntent::Redraw;
                    }
                    let ci = self.combo_input.take().expect("combo_input present");
                    // Accept comma- or space-separated ids; normalize to CSV.
                    let members: Vec<&str> = ci
                        .members
                        .split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|s| !s.is_empty())
                        .collect();
                    self.acting = true;
                    self.notice = None;
                    ConnIntent::Run(vec![
                        "providers".into(),
                        "combos".into(),
                        "add".into(),
                        ci.name,
                        "--providers".into(),
                        members.join(","),
                    ])
                }
            },
            0x7f | 0x08 => {
                field.pop();
                ConnIntent::Redraw
            }
            c if c.is_ascii_graphic() => {
                field.push(c as char);
                ConnIntent::Redraw
            }
            _ => ConnIntent::Bell,
        }
    }

    /// `u`: set the selected account active. Single-flight guarded.
    fn act_use(&mut self) -> ConnIntent {
        if self.acting {
            self.notice = Some("busy - one action at a time".to_string());
            return ConnIntent::Redraw;
        }
        let Some(acct) = self.selected_account() else {
            return ConnIntent::Bell;
        };
        if acct.active {
            self.notice = Some(format!("{} is already active", acct.id));
            return ConnIntent::Redraw;
        }
        let id = acct.id.clone();
        self.acting = true;
        self.notice = None;
        ConnIntent::Run(vec!["providers".into(), "use".into(), id])
    }

    /// `s`: set the selected account as the session-local active account for
    /// NEW spawns (distinct from `u`se, the global credential slot-swap - Locked
    /// Decision 1). Toggle: pressing it on the already-active account clears
    /// back to the default (`None`). NOT single-flight-guarded (it mutates no
    /// credential and shells nothing); a non-account row bells. Always repaints
    /// the marker (AC1-UI: no zero-feedback keypress). Yields the post-toggle
    /// value so the client mirrors it into its authoritative `active_account`.
    fn act_set_active(&mut self) -> ConnIntent {
        let Some(acct) = self.selected_account() else {
            return ConnIntent::Bell;
        };
        let id = acct.id.clone();
        self.active_account = if self.active_account.as_deref() == Some(id.as_str()) {
            None // toggle off: back to the default account
        } else {
            Some(id)
        };
        self.notice = Some(match &self.active_account {
            Some(id) => format!("spawns now bill {id}"),
            None => "spawns back to default account".to_string(),
        });
        ConnIntent::SetActiveAccount(self.active_account.clone())
    }

    /// `d`: stage a remove confirm for the selected account.
    fn act_remove(&mut self) -> ConnIntent {
        if self.acting {
            self.notice = Some("busy - one action at a time".to_string());
            return ConnIntent::Redraw;
        }
        let Some(acct) = self.selected_account() else {
            return ConnIntent::Bell;
        };
        let id = acct.id.clone();
        self.confirm = Some(PendingConfirm {
            label: format!("remove account {id}? (Enter=yes, any key=no)"),
            argv: vec!["providers".into(), "remove".into(), id],
        });
        ConnIntent::Redraw
    }

    /// `r`: register (finalize a pending login, or refresh a managed record).
    /// Single-flight; claude passes CLAUDE_CONFIG_DIR in the child env.
    fn act_register(&mut self) -> ConnIntent {
        if self.acting {
            self.notice = Some("busy - one action at a time".to_string());
            return ConnIntent::Redraw;
        }
        // A pending row: finalize its login into a managed record.
        if let Some(p) = self.selected_pending().cloned() {
            let argv = vec![
                "providers".into(),
                "register".into(),
                p.id.clone(),
                "--cli".into(),
                p.cli.clone(),
            ];
            self.acting = true;
            self.notice = None;
            return if p.cli == "claude" && !p.dir.is_empty() {
                ConnIntent::RunEnv {
                    argv,
                    env: vec![("CLAUDE_CONFIG_DIR".into(), p.dir.clone())],
                }
            } else {
                ConnIntent::Run(argv)
            };
        }
        // A real managed record: re-register (refresh the snapshot).
        let Some(acct) = self.selected_account() else {
            return ConnIntent::Bell;
        };
        if acct.auth != "managed" {
            self.notice = Some(format!("{} is not a managed account", acct.id));
            return ConnIntent::Redraw;
        }
        let argv = vec![
            "providers".into(),
            "register".into(),
            acct.id.clone(),
            "--cli".into(),
            acct.cli.clone(),
        ];
        self.acting = true;
        self.notice = None;
        ConnIntent::Run(argv)
    }

    /// Handle a key while the add wizard is open. Text steps: printable appends,
    /// Backspace pops, Enter advances, Esc cancels. The Kind step toggles.
    fn wizard_key(&mut self, key: u8) -> ConnIntent {
        if key == 0x1b {
            self.wizard = None;
            self.notice = Some("add cancelled".to_string());
            return ConnIntent::Redraw;
        }
        let Some(w) = self.wizard.as_mut() else {
            return ConnIntent::Bell;
        };
        match w.step {
            WizardStep::Id => match key {
                b'\r' | b'\n' => {
                    if !valid_account_id(&w.id) {
                        self.notice =
                            Some("id must be lowercase letters/digits/-, leading letter".into());
                        return ConnIntent::Redraw;
                    }
                    w.step = WizardStep::Kind;
                    self.notice = None;
                    ConnIntent::Redraw
                }
                0x7f | 0x08 => {
                    w.id.pop();
                    ConnIntent::Redraw
                }
                c if c.is_ascii_graphic() => {
                    w.id.push(c as char);
                    ConnIntent::Redraw
                }
                _ => ConnIntent::Bell,
            },
            WizardStep::Kind => match key {
                b'\t' | b' ' => {
                    w.kind = w.kind.next();
                    ConnIntent::Redraw
                }
                b'\r' | b'\n' => match w.kind {
                    // Shared managed slot: no dir - login runs in ~/.claude and
                    // register snapshots it (the token-swap model). dir stays "".
                    AddKind::Claude | AddKind::Codex => self.finish_login(),
                    // Isolated: prefill a per-account config dir, then collect it.
                    AddKind::ClaudeIsolated => {
                        w.dir = format!("~/.claude-{}", w.id);
                        w.step = WizardStep::Dir;
                        ConnIntent::Redraw
                    }
                    AddKind::ApiKeyGlm => {
                        w.step = WizardStep::ApiKey;
                        ConnIntent::Redraw
                    }
                },
                _ => ConnIntent::Bell,
            },
            WizardStep::Dir => match key {
                b'\r' | b'\n' => {
                    if w.dir.is_empty() {
                        self.notice = Some("config dir cannot be empty".into());
                        return ConnIntent::Redraw;
                    }
                    self.finish_login()
                }
                0x7f | 0x08 => {
                    w.dir.pop();
                    ConnIntent::Redraw
                }
                c if c.is_ascii_graphic() => {
                    w.dir.push(c as char);
                    ConnIntent::Redraw
                }
                _ => ConnIntent::Bell,
            },
            WizardStep::ApiKey => match key {
                b'\r' | b'\n' => {
                    if w.api_key.is_empty() {
                        self.notice = Some("api key cannot be empty".into());
                        return ConnIntent::Redraw;
                    }
                    self.finish_api_key()
                }
                0x7f | 0x08 => {
                    w.api_key.pop();
                    ConnIntent::Redraw
                }
                c if c.is_ascii_graphic() => {
                    w.api_key.push(c as char);
                    ConnIntent::Redraw
                }
                _ => ConnIntent::Bell,
            },
        }
    }

    /// Close the wizard, add a session-local pending row, and spawn the login
    /// pane via the `fno mux pane run` PaneRun front door. An empty `dir` (the
    /// default managed claude + codex) logs into the SHARED slot (no
    /// CLAUDE_CONFIG_DIR override, so ~/.claude and its skills/hooks are reused
    /// and register snapshots that login); a non-empty `dir` (isolated claude)
    /// logs into its own CLAUDE_CONFIG_DIR.
    fn finish_login(&mut self) -> ConnIntent {
        let w = self.wizard.take().expect("wizard present");
        let cli = w.kind.cli().to_string();
        // Expand `~/` NOW so the spawn + later register use an absolute dir
        // (env/exec never expand `~`). Empty stays empty (shared slot).
        let dir = if w.dir.is_empty() {
            String::new()
        } else {
            expand_tilde(&w.dir, std::env::var_os("HOME").as_deref())
        };
        self.pending.push(PendingLogin {
            id: w.id.clone(),
            cli: cli.clone(),
            dir: dir.clone(),
        });
        self.notice = Some(format!(
            "login pane opened for {} - press r when done",
            w.id
        ));
        let inner: Vec<String> = if cli == "claude" {
            if dir.is_empty() {
                // Shared ~/.claude slot: no CLAUDE_CONFIG_DIR override.
                vec!["claude".into(), "/login".into()]
            } else {
                vec![
                    "env".into(),
                    format!("CLAUDE_CONFIG_DIR={dir}"),
                    "claude".into(),
                    "/login".into(),
                ]
            }
        } else {
            vec!["codex".into(), "login".into()]
        };
        let mut argv = vec!["mux".into(), "pane".into(), "run".into()];
        argv.extend(inner);
        ConnIntent::SpawnLogin(argv)
    }

    /// Close the wizard and add the GLM api-key provider in one verb.
    fn finish_api_key(&mut self) -> ConnIntent {
        let w = self.wizard.take().expect("wizard present");
        self.acting = true;
        self.notice = None;
        ConnIntent::Run(vec![
            "providers".into(),
            "add".into(),
            w.id,
            "--cli".into(),
            "claude".into(),
            "--auth".into(),
            "api_key".into(),
            "--env".into(),
            format!("ANTHROPIC_BASE_URL={GLM_BASE_URL}"),
            "--env".into(),
            format!("ANTHROPIC_API_KEY={}", w.api_key),
        ])
    }

    fn move_sel(&mut self, delta: isize) -> ConnIntent {
        let (sel, len) = match self.tab {
            Tab::Accounts => (&mut self.acct_sel, self.accounts.len() + self.pending.len()),
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
            ModalState::Ready => {
                if let Some(w) = &self.wizard {
                    self.render_wizard(w, &mut out);
                } else if let Some(ci) = &self.combo_input {
                    self.render_combo_input(ci, &mut out);
                } else {
                    match self.tab {
                        Tab::Accounts => self.render_accounts(&mut out),
                        Tab::Order => self.render_order(&mut out),
                    }
                }
            }
        }
        out.push(String::new());
        if let Some(confirm) = &self.confirm {
            out.push(format!("? {}", confirm.label));
        } else if self.acting {
            out.push("running…".to_string());
        } else {
            out.push(self.footer());
        }
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
        if self.accounts.is_empty() && self.pending.is_empty() {
            out.push("no accounts registered - press a to add".to_string());
            return;
        }
        for (i, a) in self.accounts.iter().enumerate() {
            let cursor = if i == self.acct_sel { ">" } else { " " };
            let badge = if a.active { "●" } else { " " };
            // (x-c914) A distinct billing marker for the active-for-spawns
            // account, kept separate from the global-active `●` (Locked
            // Decision 1). Always a column so alignment never shifts.
            let spawn = if self.active_account.as_deref() == Some(a.id.as_str()) {
                "$"
            } else {
                " "
            };
            let snap = a
                .snapshot
                .as_deref()
                .map(|s| format!("  snap={s}"))
                .unwrap_or_default();
            out.push(format!(
                "{cursor}{badge}{spawn} {id}  [{cli}] {auth}  {headroom}{snap}",
                id = a.id,
                cli = a.cli,
                auth = a.auth,
                headroom = a.headroom,
            ));
        }
        // Session-local pending logins render after the real records; `r`
        // registers the selected one into a managed account.
        for (j, p) in self.pending.iter().enumerate() {
            let idx = self.accounts.len() + j;
            let cursor = if idx == self.acct_sel { ">" } else { " " };
            out.push(format!(
                "{cursor}   {id}  [{cli}] …login pending  (r: register)",
                id = p.id,
                cli = p.cli,
            ));
        }
    }

    /// Render the add-wizard input lines (replaces the list body while open).
    fn render_wizard(&self, w: &Wizard, out: &mut Vec<String>) {
        out.push("add account".to_string());
        let field = |active: bool, label: &str, val: &str| {
            let caret = if active { "_" } else { "" };
            format!("{} {label}: {val}{caret}", if active { ">" } else { " " })
        };
        // x-e9c3: "id" read as an opaque identifier; "name" + a format hint
        // makes the lowercase/hyphen constraint (valid_account_id) visible
        // while typing instead of only surfacing on a rejected submit.
        let id_hint = if w.step == WizardStep::Id {
            "  (lowercase, a-z0-9-)"
        } else {
            ""
        };
        out.push(format!(
            "{}{id_hint}",
            field(w.step == WizardStep::Id, "name", &w.id)
        ));
        if w.step != WizardStep::Id {
            let kind_line = format!(
                "{} kind: {}  (Tab/Space to change)",
                if w.step == WizardStep::Kind { ">" } else { " " },
                w.kind.label(),
            );
            out.push(kind_line);
        }
        if w.step == WizardStep::Dir {
            out.push(field(true, "config dir", &w.dir));
        }
        if w.step == WizardStep::ApiKey {
            out.push(field(true, "api key", &w.api_key));
        }
        out.push(String::new());
        out.push("Enter: next   Esc: cancel".to_string());
    }

    fn render_order(&self, out: &mut Vec<String>) {
        if self.combos.is_empty() {
            out.push("no combos - press n to add".to_string());
            return;
        }
        let known: std::collections::HashSet<&str> =
            self.accounts.iter().map(|a| a.id.as_str()).collect();
        for (i, c) in self.combos.iter().enumerate() {
            let selected = i == self.combo_sel;
            let cursor = if selected { ">" } else { " " };
            let badge = if c.active { "●" } else { " " };
            out.push(format!(
                "{cursor}{badge} {name}  [{strategy}]",
                name = c.name,
                strategy = c.strategy,
            ));
            if selected {
                // The selected combo expands its members in (dirty-or-committed)
                // order, with the member cursor and dangling flags.
                let members = self.shown_members();
                for (m, member) in members.iter().enumerate() {
                    let mcur = if m == self.member_sel { "»" } else { " " };
                    let dangling = if known.contains(member.as_str()) {
                        ""
                    } else {
                        "  (dangling)"
                    };
                    out.push(format!("    {mcur} {n}. {member}{dangling}", n = m + 1));
                }
                if self.dirty_order.is_some() {
                    out.push("      Enter: commit   Esc: discard".to_string());
                }
            }
        }
    }

    /// Render the new-combo form (name -> members csv).
    fn render_combo_input(&self, ci: &ComboInput, out: &mut Vec<String>) {
        out.push("new combo".to_string());
        let field = |active: bool, label: &str, val: &str| {
            let caret = if active { "_" } else { "" };
            format!("{} {label}: {val}{caret}", if active { ">" } else { " " })
        };
        out.push(field(ci.step == ComboInputStep::Name, "name", &ci.name));
        if ci.step == ComboInputStep::Members {
            out.push(field(true, "members (comma/space)", &ci.members));
        }
        out.push(String::new());
        out.push("Enter: next   Esc: cancel".to_string());
    }

    fn footer(&self) -> String {
        match self.tab {
            Tab::Accounts => {
                "Tab: order  j/k: move  u: use  s: spawn-acct  d: remove  a: add  r: register  R: refresh  Esc: close"
                    .to_string()
            }
            Tab::Order => {
                "Tab: acct  h/l: combo  j/k: member  J/K: reorder  space: use  d: rm  n: new  Enter: commit"
                    .to_string()
            }
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

/// The result of a single-flight mutation verb: a human message and whether it
/// succeeded. A failure carries the stderr tail so the modal can surface WHY
/// (AC1-ERR) rather than a bare "failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionResult {
    pub ok: bool,
    pub msg: String,
}

/// Run a single-flight `fno <argv>` mutation to completion. Never a read - this
/// is the write path. On non-zero exit the message is the trimmed stderr tail so
/// the modal surfaces the real reason. Longer timeout than a read: a `register`
/// can hit an interactive Keychain access prompt (Domain Pitfalls), so a slow
/// verb yields a named timeout error, never a hung modal.
pub async fn run_verb(argv: Vec<String>) -> ActionResult {
    run_verb_env(argv, Vec::new()).await
}

/// Like [`run_verb`] but sets extra child env vars (register needs
/// `CLAUDE_CONFIG_DIR` to pick the account's config dir / Keychain item).
pub async fn run_verb_env(argv: Vec<String>, env: Vec<(String, String)>) -> ActionResult {
    let mut cmd = tokio::process::Command::new(fno_bin());
    cmd.args(&argv);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let fut = cmd
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(Duration::from_secs(30), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return ActionResult {
                ok: false,
                msg: format!("{}: spawn failed: {e}", argv.join(" ")),
            }
        }
        Err(_) => {
            return ActionResult {
                ok: false,
                msg: format!("{}: timed out (try the verb in a terminal)", argv.join(" ")),
            }
        }
    };
    if output.status.success() {
        return ActionResult {
            ok: true,
            msg: format!("{} ok", argv.join(" ")),
        };
    }
    let tail = stderr_tail(&output.stderr);
    ActionResult {
        ok: false,
        msg: format!("{} failed: {tail}", argv.join(" ")),
    }
}

/// The last non-empty line of stderr, trimmed - the operator-facing reason.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no error output)")
        .to_string()
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

    // x-c914 piece 1: set-active-account (`s`) is a session-local spawn-routing
    // toggle, distinct from `use` (the global slot-swap). AC1-HP / AC1-UI / AC1-ERR.
    #[test]
    fn set_active_marks_and_routes_new_spawns(/* AC1-HP + AC1-UI */) {
        let mut v = ready_view();
        v.acct_sel = 1; // ccr, NOT the globally-active ccm
        let id = v.accounts[1].id.clone();
        let intent = v.on_key(b's');
        assert_eq!(intent, ConnIntent::SetActiveAccount(Some(id.clone())));
        assert_eq!(v.active_account.as_deref(), Some(id.as_str()));
        // The billing marker repaints immediately (no zero-feedback keypress).
        assert!(v.render().join("\n").contains('$'));
    }

    #[test]
    fn set_active_toggles_back_to_default(/* AC1-UI toggle-off */) {
        let mut v = ready_view();
        v.acct_sel = 1;
        v.on_key(b's'); // set ccr active for spawns
        let off = v.on_key(b's'); // same row again -> clear to default
        assert_eq!(off, ConnIntent::SetActiveAccount(None));
        assert_eq!(v.active_account, None);
    }

    #[test]
    fn set_active_on_non_account_row_bells(/* AC1-ERR */) {
        let mut v = ready_view();
        v.pending.push(PendingLogin {
            id: "x".into(),
            cli: "claude".into(),
            dir: String::new(),
        });
        v.acct_sel = v.accounts.len(); // the synthetic pending row (not an account)
        assert_eq!(v.on_key(b's'), ConnIntent::Bell);
        assert!(v.active_account.is_none());
    }

    #[test]
    fn seeded_active_account_paints_marker_on_open() {
        // The client seeds the modal with its current active account so the
        // marker is correct on first paint (survives modal close/reopen).
        let id = sample_accounts()[0].id.clone();
        let mut v = ConnectionsView::new().with_active_account(Some(id.clone()));
        v.apply_read(ReadOutcome::Ok {
            accounts: sample_accounts(),
            combos: sample_combos(),
        });
        assert!(v.render().join("\n").contains('$'));
        assert_eq!(v.active_account.as_deref(), Some(id.as_str()));
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

    // AC2-HP: `u` on a non-active account runs `providers use <id>`.
    #[test]
    fn use_key_runs_use_verb_for_inactive_account() {
        let mut v = ready_view();
        v.acct_sel = 1; // ccr, not active
        let intent = v.on_key(b'u');
        assert_eq!(
            intent,
            ConnIntent::Run(vec!["providers".into(), "use".into(), "ccr".into()])
        );
        assert!(v.acting); // single-flight guard armed
    }

    #[test]
    fn use_key_on_active_account_is_a_noop_notice() {
        let mut v = ready_view();
        v.acct_sel = 0; // ccm is active
        assert_eq!(v.on_key(b'u'), ConnIntent::Redraw);
        assert!(!v.acting);
        assert!(v.notice.as_deref().unwrap().contains("already active"));
    }

    // single-flight: a second action while acting is a no-op notice, not a spawn.
    #[test]
    fn second_action_while_acting_is_blocked() {
        let mut v = ready_view();
        v.acct_sel = 1;
        assert!(matches!(v.on_key(b'u'), ConnIntent::Run(_)));
        assert!(v.acting);
        let intent = v.on_key(b'u');
        assert_eq!(intent, ConnIntent::Redraw);
        assert!(v.notice.as_deref().unwrap().contains("busy"));
    }

    // AC1-ERR path: remove stages a confirm; Enter runs remove, other key cancels.
    #[test]
    fn remove_requires_confirm_then_runs() {
        let mut v = ready_view();
        v.acct_sel = 1; // ccr
        assert_eq!(v.on_key(b'd'), ConnIntent::Redraw);
        assert!(v.confirm.is_some());
        assert!(v.render().join("\n").contains("remove account ccr?"));
        let intent = v.on_key(b'\r');
        assert_eq!(
            intent,
            ConnIntent::Run(vec!["providers".into(), "remove".into(), "ccr".into()])
        );
        assert!(v.confirm.is_none());
    }

    #[test]
    fn remove_confirm_any_other_key_cancels() {
        let mut v = ready_view();
        v.acct_sel = 1;
        v.on_key(b'd');
        assert!(v.confirm.is_some());
        assert_eq!(v.on_key(b'x'), ConnIntent::Redraw); // not Enter -> cancel
        assert!(v.confirm.is_none());
        assert!(v.notice.as_deref().unwrap().contains("cancelled"));
    }

    #[test]
    fn expand_tilde_expands_leading_home_only() {
        use std::ffi::OsStr;
        let home = Some(OsStr::new("/Users/x"));
        assert_eq!(expand_tilde("~/.claude-ccm", home), "/Users/x/.claude-ccm");
        // No `~/` prefix -> verbatim; a bare `~` is not expanded.
        assert_eq!(expand_tilde("/abs/dir", home), "/abs/dir");
        assert_eq!(expand_tilde("~notme", home), "~notme");
        // No home -> verbatim (fail-open).
        assert_eq!(expand_tilde("~/.claude", None), "~/.claude");
    }

    #[test]
    fn stderr_tail_picks_last_nonempty_line() {
        assert_eq!(stderr_tail(b"warn\nerror: bad id\n\n"), "error: bad id");
        assert_eq!(stderr_tail(b""), "(no error output)");
        assert_eq!(stderr_tail(b"   \n  \n"), "(no error output)");
    }

    // ── Task 1.4: login wizard + api-key add ────────────────────────────────

    #[test]
    fn account_id_validation_matches_register_contract() {
        assert!(valid_account_id("ccm"));
        assert!(valid_account_id("ccm-2"));
        assert!(!valid_account_id("Ccm")); // uppercase
        assert!(!valid_account_id("2cm")); // leading digit
        assert!(!valid_account_id("cc_m")); // underscore
        assert!(!valid_account_id("")); // empty
    }

    fn type_str(v: &mut ConnectionsView, s: &str) {
        for b in s.bytes() {
            v.on_key(b);
        }
    }

    // AC3-HP (default): the managed claude account shares ~/.claude - login runs
    // in the shared slot (NO CLAUDE_CONFIG_DIR override, no dir step, no
    // per-account dir), so all accounts reuse one config (the token-swap model).
    #[test]
    fn login_wizard_claude_shared_spawns_plain_login_no_dir() {
        let mut v = ready_view();
        assert_eq!(v.on_key(b'a'), ConnIntent::Redraw);
        assert!(v.wizard.is_some());
        type_str(&mut v, "ccm2");
        assert_eq!(v.on_key(b'\r'), ConnIntent::Redraw); // id -> kind (default = shared claude)
        let intent = v.on_key(b'\r'); // kind=Claude(shared) -> spawn directly (no dir step)
        match intent {
            // No `env CLAUDE_CONFIG_DIR=...`: login runs in the shared ~/.claude.
            ConnIntent::SpawnLogin(argv) => {
                assert_eq!(argv, vec!["mux", "pane", "run", "claude", "/login"]);
                assert!(!argv.iter().any(|a| a.contains("CLAUDE_CONFIG_DIR")));
            }
            other => panic!("expected SpawnLogin, got {other:?}"),
        }
        assert!(v.wizard.is_none());
        assert_eq!(v.pending.len(), 1);
        assert!(v.pending[0].dir.is_empty()); // shared slot -> register snapshots ~/.claude
        let out = v.render().join("\n");
        assert!(out.contains("ccm2  [claude] …login pending"));
    }

    // The opt-in isolated path: an explicit own CLAUDE_CONFIG_DIR (a separate
    // config dir), tilde-expanded before the spawn.
    #[test]
    fn login_wizard_claude_isolated_uses_own_dir() {
        let mut v = ready_view();
        v.on_key(b'a');
        type_str(&mut v, "ccm2");
        v.on_key(b'\r'); // id -> kind
        v.on_key(b'\t'); // Claude(shared) -> ClaudeIsolated
        let intent = v.on_key(b'\r'); // -> dir step (prefilled)
        assert_eq!(intent, ConnIntent::Redraw);
        assert_eq!(v.wizard.as_ref().unwrap().dir, "~/.claude-ccm2");
        let intent = v.on_key(b'\r'); // commit dir -> spawn
        let expected_dir = expand_tilde("~/.claude-ccm2", std::env::var_os("HOME").as_deref());
        match intent {
            ConnIntent::SpawnLogin(argv) => {
                assert_eq!(argv[0..4], ["mux", "pane", "run", "env"]);
                assert_eq!(argv[4], format!("CLAUDE_CONFIG_DIR={expected_dir}"));
                assert!(
                    !argv[4].contains('~'),
                    "tilde must be expanded: {}",
                    argv[4]
                );
                assert_eq!(argv[5..], ["claude", "/login"]);
            }
            other => panic!("expected SpawnLogin, got {other:?}"),
        }
        assert_eq!(v.pending[0].dir, expected_dir);
    }

    #[test]
    fn login_wizard_codex_spawns_codex_login_no_dir_step() {
        let mut v = ready_view();
        v.on_key(b'a');
        type_str(&mut v, "cdx");
        v.on_key(b'\r'); // id -> kind
        v.on_key(b'\t'); // Claude -> ClaudeIsolated
        v.on_key(b'\t'); // ClaudeIsolated -> Codex
        let intent = v.on_key(b'\r'); // codex has no dir step -> spawn
        assert_eq!(
            intent,
            ConnIntent::SpawnLogin(vec![
                "mux".into(),
                "pane".into(),
                "run".into(),
                "codex".into(),
                "login".into()
            ])
        );
        assert_eq!(v.pending[0].cli, "codex");
        assert!(v.pending[0].dir.is_empty());
    }

    #[test]
    fn register_pending_row_uses_config_dir_env() {
        let mut v = ready_view();
        v.pending.push(PendingLogin {
            id: "ccm2".into(),
            cli: "claude".into(),
            dir: "~/.claude-ccm2".into(),
        });
        // Move the cursor onto the pending row (past the 3 real accounts).
        v.acct_sel = v.accounts.len(); // first pending
        let intent = v.on_key(b'r');
        assert_eq!(
            intent,
            ConnIntent::RunEnv {
                argv: vec![
                    "providers".into(),
                    "register".into(),
                    "ccm2".into(),
                    "--cli".into(),
                    "claude".into()
                ],
                env: vec![("CLAUDE_CONFIG_DIR".into(), "~/.claude-ccm2".into())],
            }
        );
        assert!(v.acting);
    }

    #[test]
    fn apply_read_prunes_registered_pending() {
        let mut v = ready_view();
        v.pending.push(PendingLogin {
            id: "ccr".into(), // ccr already a real account in sample
            cli: "claude".into(),
            dir: String::new(),
        });
        v.apply_read(ReadOutcome::Ok {
            accounts: sample_accounts(),
            combos: sample_combos(),
        });
        // ccr became a real record -> its pending row drops.
        assert!(v.pending.is_empty());
    }

    #[test]
    fn api_key_wizard_builds_add_verb_with_glm_preset() {
        let mut v = ready_view();
        v.on_key(b'a');
        type_str(&mut v, "glm2");
        v.on_key(b'\r'); // id -> kind
        v.on_key(b'\t'); // Claude -> ClaudeIsolated
        v.on_key(b'\t'); // ClaudeIsolated -> Codex
        v.on_key(b'\t'); // Codex -> api-key glm
        v.on_key(b'\r'); // -> api key step
        type_str(&mut v, "sk-abc");
        let intent = v.on_key(b'\r');
        match intent {
            ConnIntent::Run(argv) => {
                assert_eq!(argv[0..2], ["providers", "add"]);
                assert!(argv.contains(&"api_key".to_string()));
                assert!(argv.iter().any(|a| a.contains("ANTHROPIC_BASE_URL=")));
                assert!(argv.iter().any(|a| a == "ANTHROPIC_API_KEY=sk-abc"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn wizard_rejects_invalid_id_before_advancing() {
        let mut v = ready_view();
        v.on_key(b'a');
        type_str(&mut v, "Bad");
        assert_eq!(v.on_key(b'\r'), ConnIntent::Redraw); // stays on Id
        assert_eq!(v.wizard.as_ref().unwrap().step, WizardStep::Id);
        assert!(v.notice.as_deref().unwrap().contains("lowercase"));
    }

    #[test]
    fn wizard_esc_cancels() {
        let mut v = ready_view();
        v.on_key(b'a');
        type_str(&mut v, "x");
        assert_eq!(v.on_key(0x1b), ConnIntent::Redraw);
        assert!(v.wizard.is_none());
        assert!(v.pending.is_empty());
    }

    // ── Task 1.5: Order tab (reorder / activate / remove / new) ─────────────

    fn order_view() -> ConnectionsView {
        let mut v = ready_view();
        v.on_key(b'\t'); // -> Order
        v
    }

    // AC4-HP: J/K buffer locally; Enter commits exactly one combos update.
    #[test]
    fn reorder_buffers_then_commits_one_update() {
        let mut v = order_view();
        // members: [ccm, ccr, glm]; cursor on member 0 (ccm). Move it down twice.
        assert_eq!(v.member_sel, 0);
        assert_eq!(v.on_key(b'J'), ConnIntent::Redraw); // ccm <-> ccr
        assert_eq!(v.on_key(b'J'), ConnIntent::Redraw); // ccm <-> glm
        assert_eq!(
            v.dirty_order,
            Some(vec!["ccr".into(), "glm".into(), "ccm".into()])
        );
        // Dirty hint shows; Enter commits one update call with the new order.
        assert!(v.render().join("\n").contains("Enter: commit"));
        let intent = v.on_key(b'\r');
        assert_eq!(
            intent,
            ConnIntent::Run(vec![
                "providers".into(),
                "combos".into(),
                "update".into(),
                "main".into(),
                "--providers".into(),
                "ccr,glm,ccm".into(),
            ])
        );
        assert!(v.dirty_order.is_none());
        assert!(v.acting);
    }

    // AC2-FR: Esc on a dirty reorder reverts, no verb runs.
    #[test]
    fn esc_discards_dirty_reorder_before_closing() {
        let mut v = order_view();
        v.on_key(b'J'); // dirty
        assert!(v.dirty_order.is_some());
        assert_eq!(v.on_key(0x1b), ConnIntent::Redraw); // discards, does NOT close
        assert!(v.dirty_order.is_none());
        // A second Esc (nothing dirty) closes.
        assert_eq!(v.on_key(0x1b), ConnIntent::Close);
    }

    #[test]
    fn tab_switch_abandons_dirty_reorder() {
        let mut v = order_view();
        v.on_key(b'J');
        assert!(v.dirty_order.is_some());
        v.on_key(b'\t'); // -> Accounts, abandons reorder
        assert!(v.dirty_order.is_none());
    }

    #[test]
    fn commit_with_no_dirty_is_a_bell() {
        let mut v = order_view();
        assert_eq!(v.on_key(b'\r'), ConnIntent::Bell);
    }

    // space activates the selected combo (combos use).
    #[test]
    fn space_activates_combo() {
        let mut v = order_view();
        let intent = v.on_key(b' ');
        assert_eq!(
            intent,
            ConnIntent::Run(vec![
                "providers".into(),
                "combos".into(),
                "use".into(),
                "main".into()
            ])
        );
    }

    // AC2-EDGE: a dangling member flags the combo and refuses activate.
    #[test]
    fn dangling_member_flagged_and_activate_refused() {
        let mut v = ConnectionsView::new();
        let combos = parse_combos(
            br#"[{"name":"main","strategy":"fallback","members":["ccm","gone"],"active":false}]"#,
        )
        .unwrap();
        v.apply_read(ReadOutcome::Ok {
            accounts: vec![sample_accounts()[0].clone()], // only ccm exists
            combos,
        });
        v.on_key(b'\t'); // -> Order
        assert!(v.render().join("\n").contains("gone  (dangling)"));
        assert_eq!(v.on_key(b' '), ConnIntent::Redraw); // activate refused
        assert!(v
            .notice
            .as_deref()
            .unwrap()
            .contains("dangling member gone"));
    }

    #[test]
    fn remove_combo_confirms_then_runs() {
        let mut v = order_view();
        assert_eq!(v.on_key(b'd'), ConnIntent::Redraw);
        assert!(v.confirm.is_some());
        let intent = v.on_key(b'\r');
        assert_eq!(
            intent,
            ConnIntent::Run(vec![
                "providers".into(),
                "combos".into(),
                "remove".into(),
                "main".into()
            ])
        );
    }

    #[test]
    fn new_combo_form_builds_add_verb() {
        let mut v = order_view();
        assert_eq!(v.on_key(b'n'), ConnIntent::Redraw);
        assert!(v.combo_input.is_some());
        type_str(&mut v, "backup");
        v.on_key(b'\r'); // name -> members
        type_str(&mut v, "ccr, glm");
        let intent = v.on_key(b'\r');
        assert_eq!(
            intent,
            ConnIntent::Run(vec![
                "providers".into(),
                "combos".into(),
                "add".into(),
                "backup".into(),
                "--providers".into(),
                "ccr,glm".into(),
            ])
        );
    }

    #[test]
    fn h_l_switch_combo_and_reset_member_cursor() {
        let mut v = ConnectionsView::new();
        let combos = parse_combos(
            br#"[{"name":"a","strategy":"fallback","members":["ccm","ccr"],"active":false},
                {"name":"b","strategy":"fallback","members":["glm"],"active":true}]"#,
        )
        .unwrap();
        v.apply_read(ReadOutcome::Ok {
            accounts: sample_accounts(),
            combos,
        });
        v.on_key(b'\t'); // Order
        v.on_key(b'j'); // member_sel -> 1 on combo a
        assert_eq!(v.member_sel, 1);
        assert_eq!(v.on_key(b'l'), ConnIntent::Redraw); // -> combo b
        assert_eq!(v.combo_sel, 1);
        assert_eq!(v.member_sel, 0); // reset
    }
}
