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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddKind {
    Claude,
    Codex,
    ApiKeyGlm,
}

impl AddKind {
    fn cli(self) -> &'static str {
        match self {
            AddKind::Claude => "claude",
            AddKind::Codex => "codex",
            // GLM is a claude-cli provider fronted by a base_url + api key.
            AddKind::ApiKeyGlm => "claude",
        }
    }
    fn label(self) -> &'static str {
        match self {
            AddKind::Claude => "claude (oauth login)",
            AddKind::Codex => "codex (oauth login)",
            AddKind::ApiKeyGlm => "api-key: GLM/z.ai",
        }
    }
    fn next(self) -> AddKind {
        match self {
            AddKind::Claude => AddKind::Codex,
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
    /// The `a`dd wizard, `Some` while collecting inputs. Keys divert to it.
    pub wizard: Option<Wizard>,
    /// Session-local pending logins, rendered as synthetic Accounts rows after
    /// the real records. Never persisted; register is the durable step.
    pub pending: Vec<PendingLogin>,
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
            confirm: None,
            wizard: None,
            pending: Vec::new(),
            gen: 0,
        }
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

    /// Apply a mutation's result: clear the single-flight guard and surface the
    /// outcome as a footer notice. The caller arms the re-read separately so the
    /// modal always shows current truth after a mutation (Locked Decision 5).
    pub fn apply_action_result(&mut self, ok: bool, msg: String) {
        self.acting = false;
        self.notice = Some(msg);
        let _ = ok; // both branches surface a notice; ok only shapes the text
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
    }

    /// Pure key reducer. Returns the intent for the async wrapper to execute.
    /// Task 1.2 handles navigation + refresh + close; later tasks extend the
    /// intent set (actions, wizard, order commit).
    pub fn on_key(&mut self, key: u8) -> ConnIntent {
        // The add wizard, while open, captures all keys (typed input + toggles).
        if self.wizard.is_some() {
            return self.wizard_key(key);
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
            b'u' if !degraded && self.tab == Tab::Accounts => self.act_use(),
            b'd' if !degraded && self.tab == Tab::Accounts => self.act_remove(),
            b'a' if !degraded && self.tab == Tab::Accounts => {
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
            b'r' if !degraded && self.tab == Tab::Accounts => self.act_register(),
            _ => {
                // No zero-feedback keypress: an unhandled key rings the bell so
                // the operator always gets feedback (AC1-UI).
                ConnIntent::Bell
            }
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
                    AddKind::Claude => {
                        // Prefill the conventional per-account config dir.
                        w.dir = format!("~/.claude-{}", w.id);
                        w.step = WizardStep::Dir;
                        ConnIntent::Redraw
                    }
                    AddKind::Codex => self.finish_login(),
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
    /// pane (claude: `claude /login` under CLAUDE_CONFIG_DIR; codex: `codex
    /// login`) via the `fno mux pane run` PaneRun front door.
    fn finish_login(&mut self) -> ConnIntent {
        let w = self.wizard.take().expect("wizard present");
        let (cli, dir) = (w.kind.cli().to_string(), w.dir.clone());
        self.pending.push(PendingLogin {
            id: w.id.clone(),
            cli: cli.clone(),
            dir: dir.clone(),
        });
        self.notice = Some(format!("login pane opened for {} - press r when done", w.id));
        let inner: Vec<String> = if cli == "claude" {
            vec![
                "env".into(),
                format!("CLAUDE_CONFIG_DIR={dir}"),
                "claude".into(),
                "/login".into(),
            ]
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
            ModalState::Ready => match (&self.wizard, self.tab) {
                (Some(w), _) => self.render_wizard(w, &mut out),
                (None, Tab::Accounts) => self.render_accounts(&mut out),
                (None, Tab::Order) => self.render_order(&mut out),
            },
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
            let snap = a
                .snapshot
                .as_deref()
                .map(|s| format!("  snap={s}"))
                .unwrap_or_default();
            out.push(format!(
                "{cursor}{badge} {id}  [{cli}] {auth}  {headroom}{snap}",
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
                "{cursor}  {id}  [{cli}] …login pending  (r: register)",
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
        out.push(field(w.step == WizardStep::Id, "id", &w.id));
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
            Tab::Accounts => {
                "Tab: order  j/k: move  u: use  d: remove  a: add  r: register  R: refresh  Esc: close"
                    .to_string()
            }
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
    fn action_result_clears_acting_and_sets_notice() {
        let mut v = ready_view();
        v.acting = true;
        v.apply_action_result(false, "providers use ccr failed: boom".into());
        assert!(!v.acting);
        assert!(v.notice.as_deref().unwrap().contains("boom"));
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

    // AC3-HP: a -> id/cli/dir -> spawns the login pane; row shows pending login.
    #[test]
    fn login_wizard_claude_spawns_pane_and_marks_pending() {
        let mut v = ready_view();
        assert_eq!(v.on_key(b'a'), ConnIntent::Redraw);
        assert!(v.wizard.is_some());
        type_str(&mut v, "ccm2");
        assert_eq!(v.on_key(b'\r'), ConnIntent::Redraw); // id -> kind (default claude)
        let intent = v.on_key(b'\r'); // kind=claude -> dir step (prefilled)
        assert_eq!(intent, ConnIntent::Redraw);
        assert_eq!(v.wizard.as_ref().unwrap().dir, "~/.claude-ccm2");
        let intent = v.on_key(b'\r'); // commit dir -> spawn login
        match intent {
            ConnIntent::SpawnLogin(argv) => {
                assert_eq!(
                    argv,
                    vec![
                        "mux", "pane", "run", "env", "CLAUDE_CONFIG_DIR=~/.claude-ccm2",
                        "claude", "/login",
                    ]
                );
            }
            other => panic!("expected SpawnLogin, got {other:?}"),
        }
        assert!(v.wizard.is_none());
        // Pending row rendered, and register targets it with the config-dir env.
        assert_eq!(v.pending.len(), 1);
        let out = v.render().join("\n");
        assert!(out.contains("ccm2  [claude] …login pending"));
    }

    #[test]
    fn login_wizard_codex_spawns_codex_login_no_dir_step() {
        let mut v = ready_view();
        v.on_key(b'a');
        type_str(&mut v, "cdx");
        v.on_key(b'\r'); // id -> kind
        v.on_key(b'\t'); // claude -> codex
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
        v.on_key(b'\t'); // claude -> codex
        v.on_key(b'\t'); // codex -> api-key glm
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
}
