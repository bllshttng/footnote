//! The sideline new-agent composer: a dock pinned to the bottom of the
//! sideline column that launches a new harness session through the canonical
//! spawn door and hands off to the native session.
//!
//! This is a LAUNCHER, not a chat client: it owns the draft, the typed
//! request, and the launch lifecycle only. After a verified birth the pane
//! is focused through the existing command path (or the roster row is the
//! handoff for a bg thread). All spawn facts - capacity, routing,
//! permissions, seed acceptance - stay with the door; a refusal renders
//! verbatim and the draft survives.

use ratatui_core::buffer::Buffer as RtBuffer;
use ratatui_core::layout::{Constraint, Layout as RtLayout, Rect as RtRect};
use ratatui_core::style::{Modifier, Style as RtStyle};
use unicode_width::UnicodeWidthChar;

use super::{write_msg, ClientMsg, StdinFlow, View, MAX_MAIL_TEXT};
use crate::clipboard::on_path;
use crate::proto::agent_launch::{AgentLaunchRequest, AgentLaunchUpdate, LaunchState};

/// Ceiling on an open bracketed paste's carried bytes. The submit gate
/// refuses an over-cap message anyway; this only stops a close-marker-less
/// paste from growing the carry forever.
const MAX_PASTE_CARRY: usize = 16 * 1024;

/// One harness candidate off the platform capability table: `native` is the
/// compiled-in contract (a `[harness.<name>]` table in
/// `harness_capabilities.toml`), `installed` is the binary's presence on
/// PATH. Never a UI-only list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarnessChoice {
    pub name: String,
    pub native: bool,
    pub installed: bool,
}

impl HarnessChoice {
    pub(crate) fn selectable(&self) -> bool {
        self.native && self.installed
    }

    fn reason(&self) -> String {
        if !self.native {
            format!("{}: no native fno spawn", self.name)
        } else if !self.installed {
            format!("{}: not installed", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// The catalog read's outcome (the update-probe shape): the dock opens
/// instantly on whatever is in hand and refreshes when the probe lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogOutcome {
    Ok(Vec<HarnessChoice>),
    Degraded(String),
}

/// Which control owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Harness,
    Project,
    Model,
    Effort,
    /// The expand/collapse chip (`+`/`-`): Enter shows or hides the second
    /// chip row of pins (permission, placement).
    More,
    Permission,
    Placement,
    Message,
    Launch,
    /// The Unknown-outcome acknowledge chip: Enter resolves the blocked
    /// launch state back to editing (the plan's "explicit action").
    Dismiss,
}

impl Focus {
    fn tab_order() -> [Focus; 10] {
        [
            Focus::Harness,
            Focus::Project,
            Focus::Model,
            Focus::Effort,
            Focus::More,
            Focus::Permission,
            Focus::Placement,
            Focus::Message,
            Focus::Launch,
            Focus::Dismiss,
        ]
    }

    fn next(self, advanced: bool) -> Focus {
        let order = Self::tab_order();
        let pos = order.iter().position(|f| *f == self).unwrap_or(0);
        let mut next = order[(pos + 1) % order.len()];
        while matches!(next, Focus::Permission | Focus::Placement) && !advanced {
            next = match next {
                Focus::Permission => Focus::Placement,
                _ => Focus::Message,
            };
        }
        next
    }

    fn prev(self, advanced: bool) -> Focus {
        let order = Self::tab_order();
        let pos = order.iter().position(|f| *f == self).unwrap_or(0);
        let mut prev = order[(pos + order.len() - 1) % order.len()];
        while matches!(prev, Focus::Permission | Focus::Placement) && !advanced {
            prev = match prev {
                Focus::Placement => Focus::Permission,
                _ => Focus::More,
            };
        }
        prev
    }
}

/// The launch lifecycle the dock renders. `Submitting` freezes the
/// submitted snapshot; a terminal state keeps the draft and the reason side
/// by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    Editing,
    Submitting {
        request_id: u64,
    },
    Refused {
        request_id: u64,
        reason: String,
    },
    /// An ambiguous attempt: retry stays blocked until the operator
    /// acknowledges (Focus::Dismiss).
    Unknown {
        request_id: u64,
        reason: String,
    },
    Launched {
        request_id: u64,
        name: String,
        seed_note: Option<&'static str>,
    },
}

/// One composer instance. `open` false means the draft is RETAINED with
/// the dock hidden (Esc); nothing is dropped except by an explicit terminal
/// resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launcher {
    pub draft: LaunchDraft,
    pub focus: Focus,
    pub phase: Phase,
    /// The request id this dock's button armed, if a launch is owned.
    pub armed: Option<u64>,
    pub next_request_id: u64,
}

/// The draft: every visible field plus the nonsecret session-remembered
/// choices. `revision` bumps on every edit so a submitted request is a
/// frozen snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchDraft {
    pub harnesses: Vec<String>,
    pub harness_idx: usize,
    /// Project candidates: workspace cwds, most-recent first; the resolved
    /// choice is `cwd`.
    pub projects: Vec<String>,
    pub project_idx: usize,
    pub message: String,
    /// Cursor into `message`, in CHARS (the editor is char-addressed so a
    /// split UTF-8 sequence can never wedge it).
    pub cursor_chars: usize,
    /// Advanced pins; empty string = harness default (never an invented
    /// resolved value).
    pub model: String,
    pub effort: String,
    pub permission: String,
    pub placement: String,
    pub expanded: bool,
    pub revision: u64,
}

impl LaunchDraft {
    fn bump(&mut self) {
        self.revision += 1;
    }

    pub(crate) fn harness(&self) -> String {
        self.harnesses
            .get(self.harness_idx)
            .cloned()
            .unwrap_or_default()
    }

    fn cwd(&self) -> String {
        self.projects
            .get(self.project_idx)
            .cloned()
            .unwrap_or_default()
    }

    fn request(&self, request_id: u64) -> AgentLaunchRequest {
        AgentLaunchRequest {
            request_id,
            revision: self.revision,
            cwd: self.cwd(),
            harness: self.harness(),
            // v1 dock: the mux sideline launches pane-hosted sessions; the
            // native session is the pane itself. bg threads are NOT offered
            // here (the roster + row menu remain their surface).
            substrate: "pane".to_string(),
            model: non_empty(&self.model),
            effort: non_empty(&self.effort),
            permission_mode: non_empty(&self.permission),
            placement: non_empty(&self.placement),
            message: self.message.clone(),
        }
    }

    /// Message line count + the physical line the cursor sits on, so the
    /// editor window can follow it.
    fn message_lines(&self) -> Vec<&str> {
        if self.message.is_empty() {
            return vec![""];
        }
        self.message.split('\n').collect()
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// A terminal launch attempt the View remembers ACROSS dock close/reopen,
/// so a reopened dock shows the pending or resolved attempt instead of
/// silently allowing a replacement spawn (AC2-EDGE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentAttempt {
    pub request_id: u64,
    pub state: AttemptState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptState {
    Starting,
    Refused(String),
    Unknown(String),
    Launched { name: String },
}

/// Open the launcher (or reveal a retained draft). Restores a pending or
/// unresolved attempt's posture: the button stays disabled while an owned
/// attempt is in flight.
pub(crate) fn open(view: &mut View) {
    let mut launcher = match view.launcher_closed.take() {
        Some(l) => l,
        None => Launcher {
            draft: fresh_draft(view),
            focus: Focus::Harness,
            phase: Phase::Editing,
            armed: None,
            next_request_id: 1,
        },
    };
    // An in-flight owned attempt re-arms its posture (AC2-EDGE): the dock
    // reopens showing Starting, never offering a silent second spawn.
    if let Some(attempt) = &view.launch_attempt {
        if matches!(attempt.state, AttemptState::Starting)
            && launcher.armed == Some(attempt.request_id)
        {
            launcher.phase = Phase::Submitting {
                request_id: attempt.request_id,
            };
        }
    }
    view.launcher = Some(launcher);
    // A catalog read already in hand syncs the (retained) draft's harness
    // names immediately; a first open kicks the probe and the field renders
    // "<catalog...>" until it lands.
    if let Some(l) = view.launcher.as_mut() {
        sync_harness_names(l, &view.launcher_catalog);
    }
    // A missing OR degraded catalog re-probes: one transient failure must
    // not stick for the session while a healthy one stays last-outcome-wins.
    if !matches!(view.launcher_catalog, Some(CatalogOutcome::Ok(_))) {
        view.catalog_want = true;
    }
}

/// Offer every catalog name as a harness choice; an unavailable one refuses
/// AT SUBMIT with its own reason (visible inline), never by disappearing.
fn sync_harness_names(l: &mut Launcher, catalog: &Option<CatalogOutcome>) {
    if l.draft.harnesses.is_empty() {
        if let Some(CatalogOutcome::Ok(rows)) = catalog {
            l.draft.harnesses = rows.iter().map(|r| r.name.clone()).collect();
            if l.draft.harness_idx >= rows.len() {
                l.draft.harness_idx = 0;
            }
        }
    }
}

fn fresh_draft(view: &View) -> LaunchDraft {
    // Project candidates: the active workspace's squads, cwd-most-recent is
    // the session's own launch cwd. The exact cwd sent is the one shown.
    let mut projects: Vec<String> = Vec::new();
    let own = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if !own.is_empty() {
        projects.push(own);
    }
    for s in &view.layout.squads {
        if !projects.contains(&s.canonical_cwd) {
            projects.push(s.canonical_cwd.clone());
        }
    }
    LaunchDraft {
        harnesses: Vec::new(),
        harness_idx: 0,
        projects,
        project_idx: 0,
        message: String::new(),
        cursor_chars: 0,
        model: String::new(),
        effort: String::new(),
        permission: String::new(),
        placement: String::new(),
        expanded: false,
        revision: 1,
    }
}

/// Close (Esc): hide and retain. A submitted attempt keeps running.
pub(crate) fn close(view: &mut View) {
    if let Some(l) = view.launcher.take() {
        view.launcher_closed = Some(l);
    }
}

/// Fold one terminal update into the View. Returns the pane to focus when
/// the birth was a verified pane launch (the requesting client focuses it
/// through the existing command path).
pub(crate) fn apply_launch_update(view: &mut View, update: AgentLaunchUpdate) -> Option<u64> {
    let request_id = update.request_id;
    // The seed note folds in here, once, from the same update the rest of
    // the state derives from - never re-derived from a moved-out value.
    let seed_note: Option<&'static str>;
    let (attempt_state, pane) = match update.state {
        LaunchState::Starting => {
            seed_note = None;
            (AttemptState::Starting, None)
        }
        LaunchState::Refused { reason } => {
            seed_note = None;
            (AttemptState::Refused(reason), None)
        }
        LaunchState::Unknown { reason } => {
            seed_note = None;
            (AttemptState::Unknown(reason), None)
        }
        LaunchState::Launched {
            name,
            pane,
            seed_delivered,
        } => {
            seed_note = match seed_delivered {
                Some(true) => None,
                Some(false) => Some("interactive launch; no seed sent"),
                None => Some("seed fact unavailable from the receipt"),
            };
            (AttemptState::Launched { name }, pane)
        }
    };
    // Always remember the attempt (survives close/reopen), then mirror into
    // an open dock that owns this request. A stale id cannot overwrite a
    // newer draft: the dock only applies updates it armed.
    if let Some(l) = view.launcher.as_mut() {
        if l.armed == Some(request_id) {
            l.phase = match &attempt_state {
                AttemptState::Starting => Phase::Submitting { request_id },
                AttemptState::Refused(reason) => Phase::Refused {
                    request_id,
                    reason: reason.clone(),
                },
                AttemptState::Unknown(reason) => Phase::Unknown {
                    request_id,
                    reason: reason.clone(),
                },
                AttemptState::Launched { name } => Phase::Launched {
                    request_id,
                    name: name.clone(),
                    seed_note,
                },
            };
            // A terminal state releases the arm; retry re-arms afresh.
            if !matches!(attempt_state, AttemptState::Starting) {
                l.armed = None;
            }
        }
    }
    let focus_pane = match &attempt_state {
        AttemptState::Launched { .. } => pane,
        _ => None,
    };
    view.launch_attempt = Some(AgentAttempt {
        request_id,
        state: attempt_state,
    });
    focus_pane
}

/// Submit the current draft: freeze the snapshot, disable the button, and
/// put ONE typed request on the wire.
async fn submit(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(l) = view.launcher.as_mut() else {
        return Ok(());
    };
    // Duplicate submissions are suppressed at the source (AC2-HP), and an
    // unresolved attempt blocks retry until the operator dismisses it.
    if matches!(l.phase, Phase::Submitting { .. } | Phase::Unknown { .. }) || l.armed.is_some() {
        return Ok(());
    }
    let request_id = l.next_request_id;
    l.next_request_id += 1;
    // The catalog stays the authority on availability: an unavailable
    // (not-installed or non-native) selection refuses pre-wire with its own
    // reason, draft intact.
    let selected = l.draft.harness();
    let availability = match &view.launcher_catalog {
        Some(CatalogOutcome::Ok(rows)) => match rows.iter().find(|r| r.name == selected) {
            Some(r) if !r.selectable() => Err(r.reason()),
            Some(_) => Ok(()),
            None => Err(format!("harness {selected:?} is not in the catalog")),
        },
        _ => Err("harness catalog unavailable; cannot launch".to_string()),
    };
    if let Err(reason) = availability {
        l.phase = Phase::Refused { request_id, reason };
        return Ok(());
    }
    let request = l.draft.request(request_id);
    l.armed = Some(request_id);
    l.phase = Phase::Submitting { request_id };
    write_msg(sock_w, &ClientMsg::AgentLaunch(request))
        .await
        .map_err(|e| {
            // The send failed before the server ever saw the request: no
            // attempt exists, so the dock returns to editing with the
            // draft intact and the reason visible.
            if let Some(l) = view.launcher.as_mut() {
                l.armed = None;
                l.phase = Phase::Refused {
                    request_id,
                    reason: format!("send failed: {e}"),
                };
            }
            format!("launch send failed: {e}")
        })
}

// -- input folding -----------------------------------------------------------

/// One folded key. The launcher folds its OWN bytes (not the shared
/// selector folds) because the message field needs multiline Enter, Tab
/// navigation, and bracketed paste as data - semantics the selector folds
/// would misread as commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LKey {
    Esc,
    Enter,
    Tab,
    BackTab,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    Char(char),
    Paste(String),
}

/// Escape/UTF-8/paste carry across reads, like every overlay's esc buffer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LauncherEsc {
    esc: Vec<u8>,
    utf8: Vec<u8>,
    paste: Option<Vec<u8>>,
}

impl LauncherEsc {
    pub(crate) fn fold(&mut self, bytes: &[u8]) -> Vec<LKey> {
        let mut keys = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            // Bracketed paste swallows everything verbatim, including the
            // closing marker, into one Paste key (AC1-EDGE: pasted bytes are
            // data, never selector shortcuts).
            if let Some(buf) = self.paste.as_mut() {
                const CLOSE: &[u8] = b"\x1b[201~";
                buf.push(b);
                if buf.ends_with(CLOSE) {
                    let mut inner = std::mem::take(buf);
                    inner.truncate(inner.len() - CLOSE.len());
                    self.paste = None;
                    let text = String::from_utf8_lossy(&inner).to_string();
                    keys.push(LKey::Paste(text));
                } else if buf.len() >= MAX_PASTE_CARRY {
                    // A paste whose close marker never arrived (lost bytes, a
                    // wedged terminal) would otherwise grow the carry without
                    // limit. Treat the overflow as the end of the paste: the
                    // bytes collected so far land as text and normal folding
                    // resumes.
                    let inner = std::mem::take(buf);
                    self.paste = None;
                    keys.push(LKey::Paste(String::from_utf8_lossy(&inner).to_string()));
                }
                continue;
            }
            // Escape sequences via a small CSI accumulator: Esc, Tab,
            // Shift-Tab, arrows.
            match self.esc.last().copied() {
                None if b == 0x1b => {
                    self.esc.push(b);
                    continue;
                }
                Some(0x1b) => {
                    if b == b'[' && self.esc.len() == 1 {
                        self.esc.push(b);
                        continue;
                    }
                    // A lone ESC then a normal byte: the ESC was the key.
                    self.esc.clear();
                    keys.push(LKey::Esc);
                    if b == 0x1b {
                        self.esc.push(b);
                    } else {
                        i -= 1; // reprocess as a normal byte
                    }
                    continue;
                }
                Some(b'[') => {
                    self.esc.push(b);
                    let seq: Vec<u8> = self.esc[1..].to_vec();
                    if seq == b"[200~" {
                        self.esc.clear();
                        self.paste = Some(Vec::new());
                        continue;
                    }
                    match seq.as_slice() {
                        b"[Z" => {
                            self.esc.clear();
                            keys.push(LKey::BackTab);
                        }
                        b"[A" => {
                            self.esc.clear();
                            keys.push(LKey::Up);
                        }
                        b"[B" => {
                            self.esc.clear();
                            keys.push(LKey::Down);
                        }
                        b"[D" => {
                            self.esc.clear();
                            keys.push(LKey::Left);
                        }
                        b"[C" => {
                            self.esc.clear();
                            keys.push(LKey::Right);
                        }
                        _ => {
                            if self.esc.len() > 6 {
                                // Unknown long sequence: drop it silently.
                                self.esc.clear();
                            }
                            continue;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            // Plain bytes.
            match b {
                b'\r' | b'\n' => keys.push(LKey::Enter),
                b'\t' => keys.push(LKey::Tab),
                0x7f | 0x08 => keys.push(LKey::Backspace),
                0x01..=0x1a | 0x1c..=0x1f => {
                    // Control keys other than Enter/Tab/Backspace are not
                    // composer keys; swallow them so a chord can never
                    // fabricate text.
                }
                _ => {
                    // UTF-8 continuation folding: accumulate until the
                    // sequence decodes; a split sequence waits in the carry.
                    self.utf8.push(b);
                    match std::str::from_utf8(&self.utf8) {
                        Ok(s) => {
                            for c in s.chars() {
                                keys.push(LKey::Char(c));
                            }
                            self.utf8.clear();
                        }
                        Err(e) if e.error_len().is_none() => {
                            // Incomplete: wait for more bytes.
                        }
                        Err(_) => {
                            // Invalid: drop the carry (never wedge).
                            self.utf8.clear();
                        }
                    }
                }
            }
        }
        keys
    }
}

// -- editing -----------------------------------------------------------------

fn insert_char(draft: &mut LaunchDraft, c: char) {
    let byte = char_byte(&draft.message, draft.cursor_chars);
    draft.message.insert(byte, c);
    draft.cursor_chars += 1;
    draft.bump();
}

fn backspace(draft: &mut LaunchDraft) {
    if draft.cursor_chars == 0 {
        return;
    }
    let cur = char_byte(&draft.message, draft.cursor_chars);
    let prev = char_byte(&draft.message, draft.cursor_chars - 1);
    draft.message.replace_range(prev..cur, "");
    draft.cursor_chars -= 1;
    draft.bump();
}

fn char_byte(s: &str, chars: usize) -> usize {
    s.char_indices()
        .nth(chars)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn cursor_line_col(draft: &LaunchDraft) -> (usize, usize) {
    let before: String = draft.message.chars().take(draft.cursor_chars).collect();
    let lines: Vec<&str> = before.split('\n').collect();
    let line = lines.len().saturating_sub(1);
    (line, lines[line].chars().count())
}

fn move_left(draft: &mut LaunchDraft) {
    draft.cursor_chars = draft.cursor_chars.saturating_sub(1);
}

fn move_right(draft: &mut LaunchDraft) {
    let total = draft.message.chars().count();
    draft.cursor_chars = (draft.cursor_chars + 1).min(total);
}

fn move_up_down(draft: &mut LaunchDraft, delta: i32) {
    let (line, col) = cursor_line_col(draft);
    let lines = draft.message_lines();
    let target = line as i32 + delta;
    if target < 0 || target as usize >= lines.len() {
        return;
    }
    let target = target as usize;
    let prefix: usize = lines
        .iter()
        .take(target)
        .map(|l| l.chars().count() + 1)
        .sum();
    let width = lines[target].chars().count();
    draft.cursor_chars = prefix + col.min(width);
}

// -- keys --------------------------------------------------------------------

/// The launcher owns the keyboard while open. Returns like every other key
/// folder; nothing here ever reaches a pane.
pub(crate) async fn launcher_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.launcher_esc);
    let keys = esc.fold(bytes);
    view.launcher_esc = esc;
    for key in keys {
        if view.launcher.is_none() {
            break;
        }
        match key {
            LKey::Esc => {
                // Hide, retain the draft. A submitted launch keeps running.
                // Full-screen sideline leaves with it: keys must never reach
                // a pane that is not painted.
                if view.sideline_full {
                    view.sideline_full = false;
                }
                close(view);
                break;
            }
            LKey::Tab => {
                if let Some(l) = view.launcher.as_mut() {
                    let advanced = l.draft.expanded;
                    l.focus = l.focus.next(advanced);
                }
            }
            LKey::BackTab => {
                if let Some(l) = view.launcher.as_mut() {
                    let advanced = l.draft.expanded;
                    l.focus = l.focus.prev(advanced);
                }
            }
            LKey::Up | LKey::Down => {
                let delta = if matches!(key, LKey::Up) { -1 } else { 1 };
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message {
                        move_up_down(&mut l.draft, delta);
                    } else if delta < 0 {
                        let advanced = l.draft.expanded;
                        l.focus = l.focus.prev(advanced);
                    } else {
                        let advanced = l.draft.expanded;
                        l.focus = l.focus.next(advanced);
                    }
                }
            }
            LKey::Left => {
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => move_left(&mut l.draft),
                        Focus::Harness => {
                            cycle_harness(&mut l.draft, -1);
                        }
                        Focus::Project => {
                            l.draft.project_idx = l.draft.project_idx.saturating_sub(1);
                            l.draft.bump();
                        }
                        _ => {}
                    }
                }
            }
            LKey::Right => {
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => move_right(&mut l.draft),
                        Focus::Harness => {
                            cycle_harness(&mut l.draft, 1);
                        }
                        Focus::Project => {
                            if l.draft.project_idx + 1 < l.draft.projects.len() {
                                l.draft.project_idx += 1;
                                l.draft.bump();
                            }
                        }
                        _ => {}
                    }
                }
            }
            LKey::Backspace => {
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => backspace(&mut l.draft),
                        Focus::Model => {
                            l.draft.model.pop();
                            l.draft.bump();
                        }
                        Focus::Effort => {
                            l.draft.effort.pop();
                            l.draft.bump();
                        }
                        Focus::Permission => {
                            l.draft.permission.pop();
                            l.draft.bump();
                        }
                        Focus::Placement => {
                            l.draft.placement.pop();
                            l.draft.bump();
                        }
                        _ => {}
                    }
                }
            }
            LKey::Enter => {
                let focus = view
                    .launcher
                    .as_ref()
                    .map(|l| l.focus)
                    .unwrap_or(Focus::Launch);
                match focus {
                    Focus::Message => {
                        if let Some(l) = view.launcher.as_mut() {
                            insert_char(&mut l.draft, '\n');
                        }
                    }
                    Focus::Launch => submit(view, sock_w).await?,
                    Focus::More => {
                        if let Some(l) = view.launcher.as_mut() {
                            l.draft.expanded = !l.draft.expanded;
                            l.draft.bump();
                        }
                    }
                    Focus::Dismiss => {
                        // The explicit action that resolves an outcome the
                        // operator chooses not to wait on: an Unknown, or a
                        // Starting attempt whose update may never arrive.
                        // The draft is untouched; retry arms a fresh id (a
                        // new request, one attempt each).
                        if let Some(l) = view.launcher.as_mut() {
                            if matches!(l.phase, Phase::Unknown { .. } | Phase::Submitting { .. }) {
                                l.phase = Phase::Editing;
                                l.armed = None;
                            }
                        }
                    }
                    Focus::Harness | Focus::Project => {
                        submit(view, sock_w).await?;
                    }
                    _ => {}
                }
            }
            LKey::Char(c) => {
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => insert_char(&mut l.draft, c),
                        Focus::Model => {
                            if l.draft.model.chars().count() < MAX_MAIL_TEXT {
                                l.draft.model.push(c);
                                l.draft.bump();
                            }
                        }
                        Focus::Effort => {
                            if l.draft.effort.chars().count() < 64 {
                                l.draft.effort.push(c);
                                l.draft.bump();
                            }
                        }
                        Focus::Permission => {
                            if l.draft.permission.chars().count() < 64 {
                                l.draft.permission.push(c);
                                l.draft.bump();
                            }
                        }
                        Focus::Placement => {
                            if l.draft.placement.chars().count() < 64 {
                                l.draft.placement.push(c);
                                l.draft.bump();
                            }
                        }
                        _ => {}
                    }
                }
            }
            LKey::Paste(text) => {
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message {
                        // The draft never exceeds the submit ceiling: chars
                        // past it are dropped here, visibly at the next
                        // render, rather than being refused only at Launch.
                        let room = MAX_MAIL_TEXT.saturating_sub(l.draft.message.chars().count());
                        for c in text.chars().take(room) {
                            insert_char(&mut l.draft, c);
                        }
                    }
                    // A paste while another field is focused: the bytes are
                    // data and are dropped, never forwarded to a pane.
                }
            }
        }
    }
    Ok(StdinFlow::Continue)
}

fn cycle_harness(draft: &mut LaunchDraft, delta: i32) {
    if draft.harnesses.is_empty() {
        return;
    }
    let n = draft.harnesses.len() as i32;
    let idx = draft.harness_idx as i32;
    draft.harness_idx = ((idx + delta).rem_euclid(n)) as usize;
    draft.bump();
}

// -- catalog -----------------------------------------------------------------

/// The harness capability contract the mux already ships and the spawn door
/// enforces: `include_str!`ed at compile time, regenerated from the
/// canonical copy by fno-agents' build.rs. The catalog is the set of
/// `[harness.<name>]` tables; there is no UI-only list to drift.
const CAPABILITY_TOML: &str = include_str!("../harness_capabilities.toml");

/// The catalog read (instant: a compiled-in table plus PATH stats). Still
/// delivered through the probe channel so the dock's render flow has one
/// shape for "not yet read" and "read".
pub(crate) fn load_catalog() -> CatalogOutcome {
    let Ok(parsed) = toml::from_str::<toml::Value>(CAPABILITY_TOML) else {
        return CatalogOutcome::Degraded("harness catalog: capability table unparseable".into());
    };
    let Some(table) = parsed.get("harness").and_then(|h| h.as_table()) else {
        return CatalogOutcome::Degraded("harness catalog: no [harness] table".into());
    };
    let mut rows: Vec<HarnessChoice> = table
        .keys()
        .map(|name| HarnessChoice {
            name: name.clone(),
            native: true,
            installed: on_path(name),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    if rows.is_empty() {
        return CatalogOutcome::Degraded("harness catalog: empty capability table".into());
    }
    CatalogOutcome::Ok(rows)
}

// -- render ------------------------------------------------------------------

impl Launcher {
    /// The chip row's labels, left to right, minus the expanded-row pins:
    /// harness, project basename, model, effort, the expand chip, the phase
    /// controls pinned right (dismiss while an attempt is in flight or
    /// unresolved, then launch). One table feeds the layout, the paint and
    /// the mouse hit-test.
    fn chip_texts(&self, view: &View) -> Vec<(Focus, String)> {
        let d = &self.draft;
        // Harness: the selected name, or the catalog's live state when no
        // names have synced yet.
        let harness = if d.harnesses.is_empty() {
            match &view.launcher_catalog {
                None => "<catalog...>".to_string(),
                Some(CatalogOutcome::Degraded(e)) => format!("unavailable ({e})"),
                Some(CatalogOutcome::Ok(rows)) if rows.iter().all(|r| !r.selectable()) => {
                    "none available".to_string()
                }
                Some(CatalogOutcome::Ok(_)) => "default".to_string(),
            }
        } else {
            d.harness()
        };
        let project = match d.cwd().rsplit('/').find(|s| !s.is_empty()) {
            Some(base) => base.to_string(),
            None => "<none>".to_string(),
        };
        let mut chips = vec![
            (Focus::Harness, harness),
            (Focus::Project, project),
            (Focus::Model, chip_field(&d.model)),
            (Focus::Effort, chip_field(&d.effort)),
            (Focus::More, if d.expanded { "-" } else { "+" }.to_string()),
        ];
        if matches!(self.phase, Phase::Unknown { .. } | Phase::Submitting { .. }) {
            chips.push((Focus::Dismiss, "[dismiss]".to_string()));
        }
        chips.push((Focus::Launch, "[Launch]".to_string()));
        chips
    }

    /// The dock's rects inside `area` (its own rows at the sideline bottom).
    /// The painter and [`launcher_mouse`] both call this, so a click always
    /// lands on the chip that was drawn.
    pub(crate) fn dock_layout_rects(&self, view: &View, area: RtRect) -> DockRects {
        let text_w = area.width.max(1) as usize;
        let (_, editor_rows) = self.dock_layout(area.height as usize, text_w);
        let expanded_row = u16::from(self.draft.expanded);
        // Primary chip row: the field chips absorb the slack (Min), the
        // expand chip and the phase controls are fixed, Launch pinned right.
        let primary: Vec<(Focus, String)> = self
            .chip_texts(view)
            .into_iter()
            .filter(|(f, _)| !matches!(f, Focus::Permission | Focus::Placement))
            .collect();
        let constraints: Vec<Constraint> = primary
            .iter()
            .map(|(f, _)| match f {
                Focus::More => Constraint::Length(4),
                Focus::Launch => Constraint::Length(8),
                Focus::Dismiss => Constraint::Length(9),
                Focus::Harness => Constraint::Min(6),
                _ => Constraint::Min(4),
            })
            .collect();
        let row =
            RtLayout::horizontal(constraints).split(RtRect::new(area.x, area.y, area.width, 1));
        let mut chips: Vec<(Focus, String, RtRect)> = primary
            .iter()
            .zip(row.iter())
            .map(|((f, label), r)| (*f, label.clone(), *r))
            .collect();
        if self.draft.expanded {
            // The second chip row: the two pins, same Min treatment.
            let pins = [
                (Focus::Permission, chip_field(&self.draft.permission)),
                (Focus::Placement, chip_field(&self.draft.placement)),
            ];
            let pc: Vec<Constraint> = pins.iter().map(|_| Constraint::Min(4)).collect();
            let row2 =
                RtLayout::horizontal(pc).split(RtRect::new(area.x, area.y + 1, area.width, 1));
            for ((f, label), r) in pins.iter().zip(row2.iter()) {
                chips.push((*f, label.clone(), *r));
            }
        }
        // The editor window: bottom-anchored, then pulled up just enough to
        // keep the cursor row visible.
        let chunks = wrap_message(&self.draft.message, text_w);
        let (cur_row, _) = wrapped_cursor(&self.draft.message, self.draft.cursor_chars, text_w);
        let mut start = chunks.len().saturating_sub(editor_rows);
        if cur_row < start {
            start = cur_row;
        } else if cur_row >= start + editor_rows {
            start = cur_row + 1 - editor_rows;
        }
        let body_y = area.y + 1 + expanded_row;
        let avail = (area.height as usize).saturating_sub(1 + expanded_row as usize + 1);
        let editor_h = editor_rows.min(avail) as u16;
        let message = RtRect::new(area.x, body_y, area.width, editor_h);
        let footer = RtRect::new(
            area.x,
            (body_y + editor_h).min(area.bottom().saturating_sub(1)),
            area.width,
            1,
        );
        DockRects {
            chips,
            message,
            footer,
            start_chunk: start,
            editor_rows: editor_h as usize,
        }
    }

    /// Paint the dock into `buf` (the sideline's Buffer) inside `area`: the
    /// chip row, the wrapped message window with its cursor mark, and the
    /// outcome footer. Everything renders into the Buffer x-177c introduced;
    /// the single blit in `draw_sideline` carries it to the frame.
    pub(crate) fn paint(&self, view: &View, buf: &mut RtBuffer, area: RtRect) {
        let rects = self.dock_layout_rects(view, area);
        for (focus, label, r) in &rects.chips {
            let style = if *focus == self.focus {
                RtStyle::new().add_modifier(Modifier::REVERSED)
            } else {
                RtStyle::new().add_modifier(Modifier::DIM)
            };
            paint_chip(buf, *r, label, style);
        }
        let text_w = rects.message.width.max(1) as usize;
        let chunks = wrap_message(&self.draft.message, text_w);
        let (cur_row, cur_col) =
            wrapped_cursor(&self.draft.message, self.draft.cursor_chars, text_w);
        for k in 0..rects.editor_rows {
            let Some((_, text)) = chunks.get(rects.start_chunk + k) else {
                break;
            };
            let y = rects.message.y + k as u16;
            buf.set_string(rects.message.x, y, text, RtStyle::new());
            if self.focus == Focus::Message
                && cur_row == rects.start_chunk + k
                && (cur_col as u16) < rects.message.width
            {
                buf[(rects.message.x + cur_col as u16, y)].set_char('\u{258f}');
            }
        }
        buf.set_string(
            rects.footer.x,
            rects.footer.y,
            self.footer(),
            RtStyle::new().add_modifier(Modifier::DIM),
        );
    }

    /// The outcome / error footer: refusal reasons, unknown-evidence, seed
    /// doubt, or blank.
    pub(crate) fn footer(&self) -> String {
        match &self.phase {
            Phase::Editing => "tab: field  enter: edit/launch  esc: keep draft".to_string(),
            Phase::Submitting { .. } => "starting...".to_string(),
            Phase::Refused { reason, .. } => format!("refused: {reason}"),
            Phase::Unknown { reason, .. } => format!("outcome unknown: {reason}"),
            Phase::Launched {
                name, seed_note, ..
            } => {
                let note = seed_note.map(|n| format!(" ({n})")).unwrap_or_default();
                format!("launched {name}{note}")
            }
        }
    }

    /// The dock's geometry in a `panel_rows`-tall sideline at `text_w` text
    /// columns: (total rows, the editor's visible window). The editor is
    /// dynamic - exactly the wrapped message rows up to a cap that holds the
    /// dock near a third of the panel - so typing grows it and deleting
    /// gives the rows back. Below the cap's floor the fields still paint;
    /// the dock never hides.
    pub(crate) fn dock_layout(&self, panel_rows: usize, text_w: usize) -> (usize, usize) {
        let footer = 1;
        let chips = 1 + usize::from(self.draft.expanded);
        let cap = (panel_rows / 3).saturating_sub(chips + footer).max(1);
        let editor = wrap_message(&self.draft.message, text_w)
            .len()
            .clamp(1, cap);
        (chips + footer + editor, editor)
    }
}

/// One wrapped message row: `(char offset into the message, the chunk's
/// text)`. A HARD character wrap, not a word wrap, on purpose: one chunker
/// answers three questions - how many rows the editor needs, what each row
/// shows, and which row and column holds the cursor - so height, paint and
/// cursor can never disagree. A wide glyph that would cross the edge starts
/// the next chunk; an empty message is one empty chunk.
pub(crate) fn wrap_message(message: &str, width: usize) -> Vec<(usize, String)> {
    let width = width.max(1);
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut off = 0usize;
    let mut col = 0usize;
    let mut cur = String::new();
    for (i, c) in message.chars().enumerate() {
        if c == '\n' {
            out.push((off, std::mem::take(&mut cur)));
            col = 0;
            off = i + 1;
            continue;
        }
        let w = usize::from(UnicodeWidthChar::width(c).unwrap_or(0));
        if col + w > width && !cur.is_empty() {
            out.push((off, std::mem::take(&mut cur)));
            col = 0;
            off = i;
        }
        cur.push(c);
        col += w;
    }
    out.push((off, cur));
    out
}

/// Which wrapped chunk and column hold char offset `cursor_chars`: the
/// first chunk that spans it. A cursor on the newline that ends a physical
/// line paints at that line's end; any other chunk boundary (a hard wrap)
/// paints at the next chunk's start, so the mark is always at the text it
/// precedes.
pub(crate) fn wrapped_cursor(message: &str, cursor_chars: usize, width: usize) -> (usize, usize) {
    let chunks = wrap_message(message, width);
    for (r, (off, text)) in chunks.iter().enumerate() {
        let end = off + text.chars().count();
        if cursor_chars < end {
            return (r, cursor_chars - off);
        }
        if cursor_chars == end && message[char_byte(message, cursor_chars)..].starts_with('\n') {
            return (r, text.chars().count());
        }
    }
    let (off, text) = chunks.last().expect("wrap_message always yields one chunk");
    (
        chunks.len() - 1,
        cursor_chars.saturating_sub(*off).min(text.chars().count()),
    )
}

/// A pin chip's label: the value, or the shared "default" when empty.
fn chip_field(s: &str) -> String {
    if s.trim().is_empty() {
        "default".to_string()
    } else {
        s.to_string()
    }
}

/// One chip's paint: fill the rect (so REVERSED covers the whole chip, not
/// just the glyphs), then the label truncated to the rect with an ellipsis.
fn paint_chip(buf: &mut RtBuffer, r: RtRect, label: &str, style: RtStyle) {
    if r.width == 0 {
        return;
    }
    buf.set_style(r, style);
    let mut text = String::new();
    let mut w = 0usize;
    let mut truncated = false;
    for c in label.chars() {
        let cw = usize::from(UnicodeWidthChar::width(c).unwrap_or(1));
        if w + cw > r.width as usize {
            truncated = true;
            break;
        }
        text.push(c);
        w += cw;
    }
    if truncated {
        if w == r.width as usize {
            // Make room for the ellipsis over the last kept glyph.
            let last = text.chars().next_back();
            w -= last.and_then(UnicodeWidthChar::width).unwrap_or(1);
        }
        if w < r.width as usize {
            text.push('\u{2026}');
        }
    }
    buf.set_string(r.x, r.y, &text, style);
}

/// The dock's hit-and-paint geometry. `chips` is `(focus, label, rect)` in
/// paint order (the tab order); the painter and `launcher_mouse` both walk
/// it, so a click always lands on the chip that was drawn.
pub(crate) struct DockRects {
    pub chips: Vec<(Focus, String, RtRect)>,
    pub message: RtRect,
    pub footer: RtRect,
    pub start_chunk: usize,
    pub editor_rows: usize,
}

/// Mouse: a left press inside the dock focuses the chip it hit (Launch
/// submits); a press in the message rect focuses the editor. Anything else -
/// the footer line, the list above, the panes - reads unconsumed so the
/// normal mouse routes still run while the composer is open.
pub(crate) async fn launcher_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<bool, String> {
    use crate::proto::{MouseButton, MouseKind};
    if !matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
        return Ok(false);
    }
    let Some(l) = view.launcher.as_ref() else {
        return Ok(false);
    };
    // Full-screen sideline paints the dock across the terminal; otherwise
    // the dock lives in the sideline column. The SAME width the painter
    // used, so a click maps onto the chip that was drawn.
    let pw = if view.sideline_full {
        view.term.1 as usize
    } else {
        view.panel_w() as usize
    };
    if pw == 0 || (rep.col as usize) + 1 >= pw {
        // The divider column and the content area beyond are never the dock's.
        return Ok(false);
    }
    let text_w = pw - 1;
    // The SAME usable height the painter computes with (chrome row
    // subtracted), so a click maps onto the dock that was drawn.
    let chrome = view.bottom_row_is_chrome() as usize;
    let body_rows = (view.term.0 as usize).saturating_sub(chrome);
    let (total, _) = l.dock_layout(body_rows, text_w);
    let top = body_rows.saturating_sub(total) as u16;
    let area = RtRect::new(0, top, text_w as u16, total as u16);
    let rects = l.dock_layout_rects(view, area);
    let hit = |r: RtRect| {
        rep.col >= r.x && rep.col < r.x + r.width && rep.row >= r.y && rep.row < r.y + r.height
    };
    let focus = rects
        .chips
        .iter()
        .find(|(_, _, r)| hit(*r))
        .map(|(f, _, _)| *f)
        .or_else(|| hit(rects.message).then_some(Focus::Message));
    let Some(focus) = focus else {
        return Ok(false);
    };
    if let Some(l) = view.launcher.as_mut() {
        l.focus = focus;
    }
    if focus == Focus::Launch {
        submit(view, sock_w).await?;
    }
    Ok(true)
}
