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

use crate::ratatui_blit::{rt_color, rt_modifier};
use crate::theme::{self, Role, Theme};

use super::{write_msg, ClientMsg, StdinFlow, View, MAX_MAIL_TEXT};
use crate::clipboard::on_path;
use crate::popup::{Anchor, NavDir, Popup, PopupRow};
use crate::proto::agent_launch::{AgentLaunchRequest, AgentLaunchUpdate, LaunchState};

/// Ceiling on an open bracketed paste's carried bytes. The submit gate
/// refuses an over-cap message anyway; this only stops a close-marker-less
/// paste from growing the carry forever.
const MAX_PASTE_CARRY: usize = 16 * 1024;

/// The editor's prompt gutter: the marker glyph and one space, before the
/// first message row. The message wraps inside what remains.
const PROMPT_GUTTER: usize = 2;

/// One harness candidate off the platform capability table: `native` is the
/// compiled-in contract (a `[harness.<name>]` table in
/// `harness_capabilities.toml`), `installed` is the binary's presence on
/// PATH. Never a UI-only list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarnessChoice {
    pub name: String,
    pub native: bool,
    pub installed: bool,
    /// This harness's routing rows off `fno config route inventory`, the
    /// model picker's real choices.
    pub models: Vec<ModelChoice>,
    /// See harness_capabilities.toml `efforts`: `None` = no surface at all,
    /// `Some([])` = the axis exists with provider passthrough (free text),
    /// a filled list = the enumerable choices.
    pub efforts: Option<Vec<String>>,
    /// Same three states as `efforts`, for the permission axis.
    pub permission_modes: Option<Vec<String>>,
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

/// One model choice off the routing inventory: `name` is the routing row's
/// label (what the chip shows once picked), `model` the model id the launch
/// carries, `verdict` the inventory's reachability verdict (`ok` selectable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub name: String,
    pub model: String,
    pub verdict: String,
}

/// The catalog read's outcome (the update-probe shape): the dock opens
/// instantly on whatever is in hand and refreshes when the probe lands. The
/// second field of `Ok` carries the routing-inventory read's failure, when
/// the model lists could not be fetched; the harness rows still stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogOutcome {
    Ok(Vec<HarnessChoice>, Option<String>),
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
    /// The open choice popover, keyed inside `launcher_keys` (never through
    /// `view.aux`: the aux route is raw-fed and holds Esc for the next key).
    pub picker: Option<Picker>,
}

/// What committing the highlighted row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerAction {
    /// Set the pin to this exact value (a harness name, an effort, a
    /// permission mode).
    Set(String),
    /// The "<harness> decides" first row: clear the pin.
    Clear,
    /// A picked routing row: the chip shows `name`, the launch carries
    /// `model` as a model-only pin.
    SetModel { name: String, model: String },
    /// Switch the pin to typed free text (the fallback, never the default).
    TypeIn,
    /// The `@` node picker: insert the node id into the draft message at
    /// the cursor.
    InsertNode(String),
    /// Set the placement.
    Place(Placement),
}

/// The open choice popover: the shared `Popup` widget anchored at the chip,
/// plus the commit action per row. Typing filters the rows in place
/// (change 7): the FULL row set is captured at open, `filter` is the live
/// query, and the popup rebuilds per keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Picker {
    pub popup: Popup,
    /// The commit action per DISPLAYED row.
    pub actions: Vec<Option<PickerAction>>,
    /// The full, unfiltered row set + actions captured at open.
    pub all_rows: Vec<PopupRow>,
    pub all_actions: Vec<Option<PickerAction>>,
    pub field: Focus,
    pub anchor: Anchor,
    pub filter: String,
}

/// Where the launched session goes. Thread placements are VIEW choices, not
/// a substrate: the session is a thread shown through a portal, sent as
/// `--substrate thread --portal N` with `--split` or `--tab` (operator
/// ruling 2026-09-21). The one pane entry remains for harness args a thread
/// lane cannot carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Placement {
    /// The door's default: a thread where the harness seats one (the chip
    /// reads `where: thread`), headless where it does not.
    #[default]
    Thread,
    /// A thread through a new portal, split beside the focused pane.
    ThreadSplitBeside,
    /// A thread through a new portal in a new tab.
    ThreadNewTab,
    /// The one pane entry: a pane-hosted session in the active tab.
    PaneActiveTab,
}

impl Placement {
    fn label(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::ThreadSplitBeside => "thread split beside",
            Self::ThreadNewTab => "thread new tab",
            Self::PaneActiveTab => "pane: active tab",
        }
    }
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
    /// The model id the launch carries; empty = harness default (never an
    /// invented resolved value).
    pub model: String,
    /// The picked routing ROW's name, when the model pin came from one; the
    /// chip shows it and the request rides as a model-only pin.
    pub model_row: Option<String>,
    pub effort: String,
    pub permission: String,
    pub placement: Placement,
    /// The portal index a thread placement opens through, resolved from the
    /// live layout when the placement is picked (next free index).
    pub placement_portal: u8,
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
        // EMPTY substrate = the door's thread default; a thread placement
        // names the lane explicitly because its placement flags need it, and
        // the one pane entry pins the pane lane.
        let (substrate, placement, portal, split): (&str, Option<&str>, Option<u8>, Option<&str>) =
            match self.placement {
                Placement::Thread => ("", None, None, None),
                Placement::ThreadSplitBeside => {
                    ("thread", None, Some(self.placement_portal), Some("right"))
                }
                Placement::ThreadNewTab => {
                    ("thread", Some("new"), Some(self.placement_portal), None)
                }
                Placement::PaneActiveTab => ("pane", Some("active"), None, None),
            };
        AgentLaunchRequest {
            request_id,
            revision: self.revision,
            cwd: self.cwd(),
            harness: self.harness(),
            substrate: substrate.to_string(),
            model: non_empty(&self.model),
            // A model picked from a routing row rides as a model-only pin:
            // the door resolves the row's harness, route, account and
            // effort. A typed model keeps the explicit --harness override.
            model_names_harness: self.model_row.is_some() && non_empty(&self.model).is_some(),
            effort: non_empty(&self.effort),
            permission_mode: non_empty(&self.permission),
            placement: placement.map(str::to_string),
            portal,
            split: split.map(str::to_string),
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
            picker: None,
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
    // A fresh open starts with a fresh esc carry: a lone ESC stranded in the
    // retained dock's carry must never close the reopened dock.
    view.launcher_esc = Default::default();
    // A catalog read already in hand syncs the (retained) draft's harness
    // names immediately; a first open kicks the probe and the field renders
    // "<catalog...>" until it lands.
    if let Some(l) = view.launcher.as_mut() {
        sync_harness_names(l, &view.launcher_catalog);
    }
    // A missing OR degraded catalog re-probes: one transient failure must
    // not stick for the session while a healthy one stays last-outcome-wins.
    if !matches!(view.launcher_catalog, Some(CatalogOutcome::Ok(_, _))) {
        view.catalog_want = true;
    }
}

/// Offer every catalog name as a harness choice; an unavailable one refuses
/// AT SUBMIT with its own reason (visible inline), never by disappearing.
fn sync_harness_names(l: &mut Launcher, catalog: &Option<CatalogOutcome>) {
    if l.draft.harnesses.is_empty() {
        if let Some(CatalogOutcome::Ok(rows, _)) = catalog {
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
        model_row: None,
        effort: String::new(),
        permission: String::new(),
        placement: Placement::default(),
        placement_portal: 0,
        expanded: false,
        revision: 1,
    }
}

/// Close (Esc): hide and retain. A submitted attempt keeps running. The
/// open picker drops with the dock - a retained draft reopens pickerless,
/// anchored to whatever chip has focus then.
pub(crate) fn close(view: &mut View) {
    if let Some(mut l) = view.launcher.take() {
        l.picker = None;
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
        Some(CatalogOutcome::Ok(rows, _)) => match rows.iter().find(|r| r.name == selected) {
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
        // A lone ESC left at the END of a read is a bare Esc press, never a
        // torn sequence prefix: every launcher chunk has already passed the
        // chord scanner, which rejoins split CSI sequences and releases this
        // byte only after its 40ms quiet window. Without it one Esc press
        // waits forever for a second key.
        if self.paste.is_none() && crate::keys::take_lone_esc(&mut self.esc) {
            keys.push(LKey::Esc);
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
        // The open picker consumes the keys first: Up/Down move, Enter
        // commits, Esc closes the PICKER only (a second Esc closes the
        // dock). Everything else - Tab included - falls through to the dock.
        if view.launcher.as_ref().is_some_and(|l| l.picker.is_some()) {
            let portal = next_free_portal(view);
            // Field-disjoint snapshot for the commit path (it may clear
            // unoffered pins against the catalog).
            let catalog = view.launcher_catalog.clone();
            if let Some(l) = view.launcher.as_mut() {
                let Some(mut picker) = l.picker.take() else {
                    unreachable!("checked Some above");
                };
                match key {
                    LKey::Esc | LKey::Tab | LKey::BackTab => {
                        // Esc closes the picker; the dock keeps the draft.
                        // Tab hands the keyboard back to the dock, which
                        // then moves focus normally.
                    }
                    LKey::Up => {
                        picker.popup.nav(NavDir::Up);
                        picker.popup.follow_sel(view.term.0 as usize);
                        l.picker = Some(picker);
                    }
                    LKey::Down => {
                        picker.popup.nav(NavDir::Down);
                        picker.popup.follow_sel(view.term.0 as usize);
                        l.picker = Some(picker);
                    }
                    LKey::Enter => {
                        // sel indexes the popup's SELECTABLE targets (headers,
                        // rules and greyed rows are skipped); the commit
                        // action lives at the ROW index those targets point
                        // at, so resolve through selected(), never raw sel.
                        let action = picker
                            .popup
                            .selected()
                            .and_then(|(ri, _)| picker.actions.get(ri).cloned().flatten());
                        if let Some(action) = action {
                            apply_picker_action(l, &catalog, action, portal);
                        }
                        // A disabled or header row: the picker stays open.
                    }
                    LKey::Char(c) => {
                        // Type-to-filter: the query narrows the rows in
                        // place; the visible list is the feedback.
                        picker.filter.push(c);
                        rebuild_picker(l, picker);
                    }
                    LKey::Backspace => {
                        picker.filter.pop();
                        rebuild_picker(l, picker);
                    }
                    _ => {
                        // Every other key keeps the picker as it is; the
                        // bytes are dropped, never forwarded to a pane.
                        l.picker = Some(picker);
                    }
                }
            }
            continue;
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
                    // An effort with no surface never takes focus.
                    let harness = l.draft.harness();
                    if l.focus == Focus::Effort && !effort_offered(&harness, &view.launcher_catalog)
                    {
                        l.focus = l.focus.next(advanced);
                    }
                }
            }
            LKey::BackTab => {
                if let Some(l) = view.launcher.as_mut() {
                    let advanced = l.draft.expanded;
                    l.focus = l.focus.prev(advanced);
                    let harness = l.draft.harness();
                    if l.focus == Focus::Effort && !effort_offered(&harness, &view.launcher_catalog)
                    {
                        l.focus = l.focus.prev(advanced);
                    }
                }
            }
            LKey::Up | LKey::Down => {
                let delta = if matches!(key, LKey::Up) { -1 } else { 1 };
                // Down on a picker chip drops its popover; the anchor is
                // read before the mutable borrow.
                let open_anchor = view.launcher.as_ref().and_then(|l| {
                    (delta > 0 && is_picker_chip(l.focus) && l.focus != Focus::Project)
                        .then(|| picker_anchor(l, view))
                        .flatten()
                });
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message {
                        move_up_down(&mut l.draft, delta);
                    } else if delta < 0 {
                        let advanced = l.draft.expanded;
                        l.focus = l.focus.prev(advanced);
                    } else if let Some(anchor) = open_anchor {
                        open_picker_at(
                            l,
                            &view.launcher_catalog,
                            &view.backlog,
                            Some(anchor),
                            l.focus,
                        );
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
                            clear_unoffered_pins(&mut l.draft, &view.launcher_catalog);
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
                            clear_unoffered_pins(&mut l.draft, &view.launcher_catalog);
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
                    Focus::Project => {
                        submit(view, sock_w).await?;
                    }
                    f if is_picker_chip(f) => {
                        let anchor = view.launcher.as_ref().and_then(|l| picker_anchor(l, view));
                        if let Some(l) = view.launcher.as_mut() {
                            open_picker_at(
                                l,
                                &view.launcher_catalog,
                                &view.backlog,
                                anchor,
                                f,
                            );
                        }
                    }
                    _ => {}
                }
            }
            LKey::Char(c) => {
                // The palette's node gesture: `@` in the message opens the
                // node picker; the glyph itself never lands. The anchor is
                // read before the mutable borrow.
                let at_anchor = if c == '@' {
                    view.launcher.as_ref().and_then(|l| picker_anchor(l, view))
                } else {
                    None
                };
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message if c == '@' => {
                            open_picker_at(
                                l,
                                &view.launcher_catalog,
                                &view.backlog,
                                at_anchor,
                                Focus::Message,
                            );
                        }
                        Focus::Message => insert_char(&mut l.draft, c),
                        Focus::Model => {
                            if l.draft.model.chars().count() < MAX_MAIL_TEXT {
                                // Typing over a picked row switches the chip
                                // to free text: the row provenance drops.
                                if l.draft.model_row.take().is_some() {
                                    l.draft.model.clear();
                                }
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

/// Whether the named harness offers ANY effort value (a list, or the
/// provider passthrough free text). Unknown until the catalog lands: assume
/// yes, so the chip never dims on a read that has not happened yet.
fn effort_offered(harness: &str, catalog: &Option<CatalogOutcome>) -> bool {
    match catalog {
        Some(CatalogOutcome::Ok(rows, _)) => rows
            .iter()
            .find(|r| r.name == harness)
            .map(|r| r.efforts.is_some())
            .unwrap_or(true),
        _ => true,
    }
}

/// After the harness changes, any pin the new harness does not offer clears.
fn clear_unoffered_pins(draft: &mut LaunchDraft, catalog: &Option<CatalogOutcome>) {
    let harness = draft.harness();
    let Some(CatalogOutcome::Ok(rows, _)) = catalog else {
        return;
    };
    let Some(row) = rows.iter().find(|r| r.name == harness) else {
        return;
    };
    if let Some(name) = &draft.model_row {
        if !row.models.iter().any(|m| &m.name == name) {
            draft.model.clear();
            draft.model_row = None;
            draft.bump();
        }
    }
    // A missing axis (None) clears any pin outright: the new harness has no
    // surface for it, so the value is stale by definition. Some([]) keeps
    // free text; a filled list keeps only its own values.
    match &row.efforts {
        Some(efforts) if !efforts.is_empty() && !efforts.iter().any(|e| *e == draft.effort) => {
            draft.effort.clear();
            draft.bump();
        }
        None => {
            draft.effort.clear();
            draft.bump();
        }
        _ => {}
    }
    match &row.permission_modes {
        Some(modes) if !modes.is_empty() && !modes.iter().any(|m| *m == draft.permission) => {
            draft.permission.clear();
            draft.bump();
        }
        None => {
            draft.permission.clear();
            draft.bump();
        }
        _ => {}
    }
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

/// The next free portal index in the active layout, smallest first. A
/// thread placement opens its new portal here; a stale read costs one
/// refusal the door renders verbatim, never a wrong lane.
fn next_free_portal(view: &View) -> u8 {
    let used: Vec<u8> = view.layout.agents.iter().filter_map(|a| a.portal).collect();
    (0..=u8::MAX).find(|p| !used.contains(p)).unwrap_or(0)
}

/// The catalog read: a compiled-in table plus PATH stats, then one bounded
/// read of the routing inventory for the model lists. Delivered through the
/// probe channel so the dock's render flow has one shape for "not yet read"
/// and "read"; async because the inventory read is a subprocess.
pub(crate) async fn load_catalog() -> CatalogOutcome {
    let Ok(parsed) = toml::from_str::<toml::Value>(CAPABILITY_TOML) else {
        return CatalogOutcome::Degraded("harness catalog: capability table unparseable".into());
    };
    let Some(table) = parsed.get("harness").and_then(|h| h.as_table()) else {
        return CatalogOutcome::Degraded("harness catalog: no [harness] table".into());
    };
    let mut rows: Vec<HarnessChoice> = table
        .iter()
        .map(|(name, caps)| {
            let efforts = caps.get("efforts").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            });
            let permission_modes =
                caps.get("permission_modes")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    });
            HarnessChoice {
                name: name.clone(),
                native: true,
                installed: on_path(name),
                models: Vec::new(),
                efforts,
                permission_modes,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    if rows.is_empty() {
        return CatalogOutcome::Degraded("harness catalog: empty capability table".into());
    }
    // Models: one bounded inventory read (the same door `fno` resolves),
    // never a per-harness probe. A failure degrades the MODEL lists only:
    // the harness rows stand, and each model picker names the failure with
    // free text still available.
    let bin = crate::server::fno_bin().to_string_lossy().into_owned();
    let argv = [bin.as_str(), "config", "route", "inventory", "--json"];
    let inv = crate::dispatch_launch::run_fno_captured(
        &argv,
        std::time::Duration::from_secs(5),
        tokio::time::Instant::now() + std::time::Duration::from_secs(5),
    )
    .await;
    let mut models_err = Some("routing inventory unavailable".to_string());
    let by_harness: std::collections::HashMap<String, Vec<ModelChoice>> = match inv {
        Some((true, stdout, _)) => {
            let parsed: Option<serde_json::Value> = stdout
                .lines()
                .rev()
                .find_map(|l| serde_json::from_str(l).ok());
            match parsed
                .as_ref()
                .and_then(|v| v.get("models"))
                .and_then(|m| m.as_array())
            {
                Some(items) => {
                    let mut map: std::collections::HashMap<String, Vec<ModelChoice>> =
                        std::collections::HashMap::new();
                    for row in items {
                        let (Some(name), Some(harness), Some(model)) = (
                            row.get("name").and_then(|x| x.as_str()),
                            row.get("harness").and_then(|x| x.as_str()),
                            row.get("model").and_then(|x| x.as_str()),
                        ) else {
                            continue;
                        };
                        let verdict = row
                            .get("verdict")
                            .and_then(|x| x.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        map.entry(harness.to_string())
                            .or_default()
                            .push(ModelChoice {
                                name: name.to_string(),
                                model: model.to_string(),
                                verdict,
                            });
                    }
                    models_err = None;
                    map
                }
                // The read succeeded but carried no model list: name the
                // shape, not the transport.
                None => {
                    models_err = Some("routing inventory response unreadable".to_string());
                    std::collections::HashMap::new()
                }
            }
        }
        _ => std::collections::HashMap::new(),
    };
    for row in &mut rows {
        if let Some(list) = by_harness.get(&row.name) {
            row.models = list.clone();
        }
    }
    CatalogOutcome::Ok(rows, models_err)
}

// -- picker ------------------------------------------------------------------

/// Open the popover for the focused chip. Refuses the chips with no
/// popover: Project (Left/Right cycles it), an effort with no surface, and
/// every non-picker control. The `&View` convenience for tests; the key
/// folder precomputes the anchor instead.
#[cfg(test)]
pub(crate) fn open_picker(l: &mut Launcher, view: &View) -> bool {
    if !is_picker_chip(l.focus) || l.focus == Focus::Project {
        return false;
    }
    let harness = l.draft.harness();
    if l.focus == Focus::Effort && !effort_offered(&harness, &view.launcher_catalog) {
        return false;
    }
    let Some(anchor) = picker_anchor(l, view) else {
        return false;
    };
    open_picker_at(
        l,
        &view.launcher_catalog,
        &view.backlog,
        Some(anchor),
        l.focus,
    )
}

/// Open a picker on a precomputed anchor. The catalog and backlog ride as
/// borrows so the key folder (holding `view.launcher.as_mut`) can reach
/// them through their own, disjoint fields.
fn open_picker_at(
    l: &mut Launcher,
    catalog: &Option<CatalogOutcome>,
    backlog: &[crate::proto::BacklogCard],
    anchor: Option<(u16, u16)>,
    field: Focus,
) -> bool {
    let Some((row, col)) = anchor else {
        return false;
    };
    let (rows, actions) = picker_rows(l, catalog, backlog);
    let (all_rows, all_actions) = (rows.clone(), actions.clone());
    l.picker = Some(Picker {
        popup: Popup::new(rows, Anchor::At { row, col })
            .footer("up/down move \u{b7} type to filter \u{b7} enter pick \u{b7} esc close"),
        actions,
        all_rows,
        all_actions,
        field,
        anchor: Anchor::At { row, col },
        filter: String::new(),
    });
    true
}

/// Rebuild the popover's rows around the live filter: substring match on
/// the entry labels, case-insensitive; the query rides a header so the
/// filter state is visible, not guessed. Works off the picker's own
/// captured row set, so it needs no View access.
fn rebuild_picker(l: &mut Launcher, mut picker: Picker) {
    let q = picker.filter.to_lowercase();
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    if !q.is_empty() {
        rows.push(PopupRow::Header(format!("filter: {}", picker.filter)));
        actions.push(None);
    }
    for (row, action) in picker
        .all_rows
        .iter()
        .cloned()
        .zip(picker.all_actions.iter().cloned())
    {
        let keep = match &row {
            PopupRow::Entry { label, .. } => q.is_empty() || label.to_lowercase().contains(&q),
            _ => true,
        };
        if keep {
            rows.push(row);
            actions.push(action);
        }
    }
    picker.popup = Popup::new(rows, picker.anchor)
        .footer("up/down move \u{b7} type to filter \u{b7} enter pick \u{b7} esc close");
    picker.popup.sel = 0;
    l.picker = Some(picker);
}

/// The chip's on-screen cell, in the same geometry `launcher_mouse` maps
/// clicks with: the sideline's slice offset plus the dock area, then the
/// chip rect. The popover drops below the chip; at the screen bottom edge
/// the shared `origin` flips it above.
fn picker_anchor(l: &Launcher, view: &View) -> Option<(u16, u16)> {
    let pw = if view.sideline_full {
        view.term.1 as usize
    } else {
        view.panel_w() as usize
    };
    let text_w = pw.checked_sub(1)?;
    let chrome = view.sideline_top() + view.bottom_row_is_chrome() as usize;
    let body_rows = (view.term.0 as usize).checked_sub(chrome)?;
    let (total, _) = l.dock_layout(body_rows, text_w);
    let top = body_rows.checked_sub(total)?;
    let area = RtRect::new(0, top as u16, text_w as u16, total as u16);
    let rects = l.dock_layout_rects(view, area);
    if l.focus == Focus::Message {
        // The `@` picker anchors at the editor, not a chip.
        return Some((
            (view.sideline_top() + top + rects.message.y as usize + 1) as u16,
            rects.message.x,
        ));
    }
    let (_, _, r) = rects.chips.iter().find(|(f, _, _)| *f == l.focus)?;
    Some(((view.sideline_top() + top + r.y as usize + 1) as u16, r.x))
}

/// The popover's rows and their commit actions for `field`, read off the
/// catalog and the live draft. An unavailable choice renders as a disabled
/// entry carrying its reason (the popup's greyed-with-reason grammar).
fn picker_rows(
    l: &Launcher,
    catalog: &Option<CatalogOutcome>,
    backlog: &[crate::proto::BacklogCard],
) -> (Vec<PopupRow>, Vec<Option<PickerAction>>) {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    let mut push =
        |glyph: &str, label: &str, hint: &str, enabled: bool, action: Option<PickerAction>| {
            rows.push(PopupRow::Entry {
                glyph: glyph.to_string(),
                label: label.to_string(),
                hint: hint.to_string(),
                enabled,
            });
            actions.push(action);
        };
    let harness = l.draft.harness();
    let decides = if harness.is_empty() {
        "harness decides".to_string()
    } else {
        format!("{harness} decides")
    };
    match l.focus {
        Focus::Harness => {
            if let Some(CatalogOutcome::Ok(catalog_rows, _)) = catalog {
                for row in catalog_rows {
                    if row.selectable() {
                        push(
                            "\u{2022}",
                            &row.name,
                            "",
                            true,
                            Some(PickerAction::Set(row.name.clone())),
                        );
                    } else {
                        push("\u{2022}", &row.name, &row.reason(), false, None);
                    }
                }
            } else {
                push(
                    "\u{2022}",
                    "catalog unavailable",
                    catalog
                        .as_ref()
                        .map(|c| match c {
                            CatalogOutcome::Degraded(e) => e.as_str(),
                            _ => "",
                        })
                        .unwrap_or("reading..."),
                    false,
                    None,
                );
            }
        }
        Focus::Model => {
            push("\u{2022}", &decides, "", true, Some(PickerAction::Clear));
            if let Some(CatalogOutcome::Ok(catalog_rows, models_err)) = catalog {
                if let Some(row) = catalog_rows.iter().find(|r| r.name == harness) {
                    for m in &row.models {
                        if m.verdict == "ok" {
                            push(
                                "\u{2022}",
                                &m.name,
                                &m.model,
                                true,
                                Some(PickerAction::SetModel {
                                    name: m.name.clone(),
                                    model: m.model.clone(),
                                }),
                            );
                        } else {
                            push(
                                "\u{2022}",
                                &m.name,
                                &format!("{} ({})", m.model, m.verdict),
                                false,
                                None,
                            );
                        }
                    }
                    if let Some(err) = models_err {
                        push("\u{2022}", "model list unavailable", err, false, None);
                    }
                }
            }
            push(
                "\u{2022}",
                "type a model...",
                "",
                true,
                Some(PickerAction::TypeIn),
            );
        }
        Focus::Effort => {
            push("\u{2022}", &decides, "", true, Some(PickerAction::Clear));
            if let Some(CatalogOutcome::Ok(catalog_rows, _)) = catalog {
                if let Some(row) = catalog_rows.iter().find(|r| r.name == harness) {
                    if let Some(efforts) = &row.efforts {
                        if efforts.is_empty() {
                            push(
                                "\u{2022}",
                                "type an effort...",
                                "",
                                true,
                                Some(PickerAction::TypeIn),
                            );
                        } else {
                            for e in efforts {
                                push("\u{2022}", e, "", true, Some(PickerAction::Set(e.clone())));
                            }
                        }
                    }
                }
            }
        }
        Focus::Permission => {
            push("\u{2022}", &decides, "", true, Some(PickerAction::Clear));
            if let Some(CatalogOutcome::Ok(catalog_rows, _)) = catalog {
                if let Some(row) = catalog_rows.iter().find(|r| r.name == harness) {
                    if let Some(modes) = &row.permission_modes {
                        if modes.is_empty() {
                            push(
                                "\u{2022}",
                                "type a permission...",
                                "",
                                true,
                                Some(PickerAction::TypeIn),
                            );
                        } else {
                            for m in modes {
                                push("\u{2022}", m, "", true, Some(PickerAction::Set(m.clone())));
                            }
                        }
                    }
                }
            }
        }
        Focus::Message => {
            // The `@` node picker: the live layout's backlog cards, the
            // freshest list the client already holds - no second read.
            for card in backlog {
                push(
                    "\u{2022}",
                    &format!("{} {}", card.id, card.slug),
                    &card.priority,
                    true,
                    Some(PickerAction::InsertNode(card.id.clone())),
                );
            }
            if backlog.is_empty() {
                push("\u{2022}", "no backlog cards", "", false, None);
            }
        }
        Focus::Placement => {
            push(
                "\u{2022}",
                Placement::Thread.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::Thread)),
            );
            push(
                "\u{2022}",
                Placement::ThreadSplitBeside.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::ThreadSplitBeside)),
            );
            push(
                "\u{2022}",
                Placement::ThreadNewTab.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::ThreadNewTab)),
            );
            push(
                "\u{2022}",
                Placement::PaneActiveTab.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::PaneActiveTab)),
            );
        }
        _ => {}
    }
    (rows, actions)
}

/// Commit a picked row. `portal` was resolved before the picker borrow; a
/// stale index costs one verbatim door refusal, never a wrong lane.
pub(crate) fn apply_picker_action(
    l: &mut Launcher,
    catalog: &Option<CatalogOutcome>,
    action: PickerAction,
    portal: u8,
) {
    l.picker = None;
    match action {
        PickerAction::Set(name) => match l.focus {
            Focus::Harness => {
                if let Some(idx) = l.draft.harnesses.iter().position(|h| h == &name) {
                    l.draft.harness_idx = idx;
                    l.draft.bump();
                    clear_unoffered_pins(&mut l.draft, catalog);
                }
            }
            Focus::Effort => {
                l.draft.effort = name;
                l.draft.bump();
            }
            Focus::Permission => {
                l.draft.permission = name;
                l.draft.bump();
            }
            _ => {}
        },
        PickerAction::SetModel { name, model } => {
            l.draft.model = model;
            l.draft.model_row = Some(name);
            l.draft.bump();
        }
        PickerAction::Clear => match l.focus {
            Focus::Model => {
                l.draft.model.clear();
                l.draft.model_row = None;
                l.draft.bump();
            }
            Focus::Effort => {
                l.draft.effort.clear();
                l.draft.bump();
            }
            Focus::Permission => {
                l.draft.permission.clear();
                l.draft.bump();
            }
            _ => {}
        },
        PickerAction::TypeIn => match l.focus {
            Focus::Model => {
                l.draft.model.clear();
                l.draft.model_row = None;
                l.draft.bump();
            }
            Focus::Effort | Focus::Permission => {
                // The pin clears; the next typed chars are the value.
                if l.focus == Focus::Effort {
                    l.draft.effort.clear();
                } else {
                    l.draft.permission.clear();
                }
                l.draft.bump();
            }
            _ => {}
        },
        PickerAction::InsertNode(id) => {
            // The node id lands at the cursor with a trailing space; the
            // `@` that opened the picker never entered the draft.
            for c in id.chars().chain(std::iter::once(' ')) {
                insert_char(&mut l.draft, c);
            }
        }
        PickerAction::Place(p) => {
            l.draft.placement = p;
            l.draft.placement_portal = portal;
            l.draft.bump();
        }
    }
}

// -- render ------------------------------------------------------------------

impl Launcher {
    /// The chip row's labels, left to right, minus the expanded-row pins:
    /// harness, project basename, model, effort, the expand chip, the phase
    /// controls pinned right (dismiss while an attempt is in flight or
    /// unresolved, then launch). One table feeds the layout, the paint and
    /// the mouse hit-test.
    pub(crate) fn chip_texts(&self, view: &View) -> Vec<(Focus, String)> {
        let d = &self.draft;
        // Harness: the selected name, or the catalog's live state when no
        // names have synced yet.
        let harness = if d.harnesses.is_empty() {
            match &view.launcher_catalog {
                None => "<catalog...>".to_string(),
                Some(CatalogOutcome::Degraded(e)) => format!("unavailable ({e})"),
                Some(CatalogOutcome::Ok(rows, _)) if rows.iter().all(|r| !r.selectable()) => {
                    "none available".to_string()
                }
                Some(CatalogOutcome::Ok(rows, _)) => rows
                    .first()
                    .map(|r| r.name.clone())
                    .unwrap_or_else(|| "none available".to_string()),
            }
        } else {
            d.harness()
        };
        let project = match d.cwd().rsplit('/').find(|s| !s.is_empty()) {
            Some(base) => base.to_string(),
            None => "<none>".to_string(),
        };
        // Model label: the picked routing ROW's name, else the typed id,
        // else who resolves it. Names the provenance a model-only pin has.
        let model_label = if let Some(row) = &d.model_row {
            row.clone()
        } else if !d.model.is_empty() {
            d.model.clone()
        } else if d.harness().is_empty() {
            "harness decides".to_string()
        } else {
            format!("{} decides", d.harness())
        };
        let effort_label = if !d.effort.is_empty() {
            d.effort.clone()
        } else if !effort_offered(&d.harness(), &view.launcher_catalog) {
            "not offered".to_string()
        } else if d.harness().is_empty() {
            "harness decides".to_string()
        } else {
            format!("{} decides", d.harness())
        };
        let permission_label = if !d.permission.is_empty() {
            d.permission.clone()
        } else if d.harness().is_empty() {
            "harness decides".to_string()
        } else {
            format!("{} decides", d.harness())
        };
        // Field chips name themselves; the expanded pins join the primary
        // list here so one table feeds layout, paint and mouse. The primary
        // row filter keeps the pins off row one.
        let mut chips = vec![
            (Focus::Harness, format!("harness: {harness}")),
            (Focus::Project, format!("project: {project}")),
            (Focus::Model, format!("model: {model_label}")),
            (Focus::Effort, format!("effort: {effort_label}")),
            (
                Focus::More,
                if d.expanded { "- less" } else { "+ more" }.to_string(),
            ),
        ];
        if matches!(self.phase, Phase::Unknown { .. } | Phase::Submitting { .. }) {
            chips.push((Focus::Dismiss, "[dismiss]".to_string()));
        }
        chips.push((Focus::Launch, "Launch".to_string()));
        if d.expanded {
            chips.push((Focus::Permission, format!("permission: {permission_label}")));
            chips.push((Focus::Placement, format!("where: {}", d.placement.label())));
        }
        chips
    }

    /// The dock's rects inside `area` (its own rows at the sideline bottom).
    /// The painter and [`launcher_mouse`] both call this, so a click always
    /// lands on the chip that was drawn.
    pub(crate) fn dock_layout_rects(&self, view: &View, area: RtRect) -> DockRects {
        let text_w = area.width.max(1) as usize;
        // `area` IS the reserved dock (the caller ran dock_layout against the
        // panel), so the editor window is what remains after the chip rows
        // and the footer - re-running dock_layout here would re-cap against
        // the dock's own height and blank most of the reserved rows.
        let chip_rows = 1 + usize::from(self.draft.expanded);
        let editor_rows = (area.height as usize).saturating_sub(chip_rows + 1).max(1);
        let expanded_row = u16::from(self.draft.expanded);
        // Primary chip row: the field chips absorb the slack (Min), the
        // expand chip and the phase controls are fixed, Launch pinned right.
        let primary: Vec<(Focus, String)> = self
            .chip_texts(view)
            .into_iter()
            .filter(|(f, _)| !matches!(f, Focus::Permission | Focus::Placement))
            .collect();
        let constraints = chip_constraints(primary.iter().map(|(f, _)| match f {
            Focus::More => Constraint::Length(7),
            Focus::Launch => Constraint::Length(7),
            Focus::Dismiss => Constraint::Length(9),
            Focus::Harness => Constraint::Min(6),
            _ => Constraint::Min(4),
        }));
        let row =
            RtLayout::horizontal(constraints).split(RtRect::new(area.x, area.y, area.width, 1));
        let mut chips: Vec<(Focus, String, RtRect)> = primary
            .iter()
            .zip(row.iter().step_by(2))
            .map(|((f, label), r)| (*f, label.clone(), *r))
            .collect();
        if self.draft.expanded {
            // The second chip row: the two pins, same Min treatment. Their
            // labels come off the same table as the primary row.
            let pins: Vec<(Focus, String)> = self
                .chip_texts(view)
                .into_iter()
                .filter(|(f, _)| matches!(f, Focus::Permission | Focus::Placement))
                .collect();
            let pc = chip_constraints(pins.iter().map(|_| Constraint::Min(4)));
            let row2 =
                RtLayout::horizontal(pc).split(RtRect::new(area.x, area.y + 1, area.width, 1));
            for ((f, label), r) in pins.iter().zip(row2.iter().step_by(2)) {
                chips.push((*f, label.clone(), *r));
            }
        }
        // The editor window: bottom-anchored, then pulled up just enough to
        // keep the cursor row visible. The prompt gutter narrows the wrap.
        let wrap_w = text_w.saturating_sub(PROMPT_GUTTER);
        let chunks = wrap_message(&self.draft.message, wrap_w);
        let (cur_row, _) = wrapped_cursor(&self.draft.message, self.draft.cursor_chars, wrap_w);
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
    /// outcome footer. Everything renders into the sideline's ratatui Buffer;
    /// the single blit in `draw_sideline` carries it to the frame.
    pub(crate) fn paint(&self, view: &View, buf: &mut RtBuffer, area: RtRect) {
        let rects = self.dock_layout_rects(view, area);
        for (focus, label, r) in &rects.chips {
            // The popup's control vocabulary, not raw DIM (a dim word reads
            // as a caption): an unfocused chip is a filled Body block, the
            // focused chip the BodySel cut-out, Launch the esc-chip accent.
            // An effort with no surface on this harness is greyed with its
            // state (BodyDim, no caret, never focusable).
            let (role, caret) = match focus {
                Focus::Launch => (Role::Chip, false),
                Focus::Effort if !effort_offered(&self.draft.harness(), &view.launcher_catalog) => {
                    (Role::BodyDim, false)
                }
                f if *f == self.focus => (Role::BodySel, is_picker_chip(*f)),
                f => (Role::Body, is_picker_chip(*f)),
            };
            paint_chip(buf, *r, label, role_style(role, &view.theme), caret);
        }
        // The editor: a prompt gutter (a glyph before the first message row)
        // and, on an empty draft, dim placeholder text naming the shape.
        let text_w = rects.message.width.max(1) as usize;
        let wrap_w = text_w.saturating_sub(PROMPT_GUTTER);
        let chunks = wrap_message(&self.draft.message, wrap_w);
        let (cur_row, cur_col) =
            wrapped_cursor(&self.draft.message, self.draft.cursor_chars, wrap_w);
        for k in 0..rects.editor_rows {
            let Some((_, text)) = chunks.get(rects.start_chunk + k) else {
                break;
            };
            let y = rects.message.y + k as u16;
            buf.set_string(
                rects.message.x + PROMPT_GUTTER as u16,
                y,
                text,
                RtStyle::new(),
            );
            if self.focus == Focus::Message && cur_row == rects.start_chunk + k {
                // The cursor offset is a CHAR index; the mark lands on a
                // TERMINAL column, so wide glyphs before the cursor shift
                // it right.
                let disp_col: usize = text
                    .chars()
                    .take(cur_col)
                    .map(|c| usize::from(UnicodeWidthChar::width(c).unwrap_or(0)))
                    .sum();
                if (disp_col as u16) + (PROMPT_GUTTER as u16) < rects.message.width {
                    buf[(rects.message.x + disp_col as u16 + PROMPT_GUTTER as u16, y)]
                        .set_char('\u{258f}');
                }
            }
        }
        if rects.editor_rows > 0 {
            buf.set_string(
                rects.message.x,
                rects.message.y,
                "\u{276f} ",
                role_style(Role::BodyDim, &view.theme),
            );
            if self.draft.message.is_empty() {
                buf.set_string(
                    rects.message.x + PROMPT_GUTTER as u16,
                    rects.message.y,
                    "/fno:target <node> or a task",
                    role_style(Role::BodyDim, &view.theme),
                );
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
            Phase::Editing => {
                "tab: next field \u{b7} enter: pick/launch \u{b7} esc: close, draft kept"
                    .to_string()
            }
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
        // The prompt gutter narrows the wrap, the same width the painter
        // uses, so height, paint and cursor can never disagree.
        let editor = wrap_message(&self.draft.message, text_w.saturating_sub(PROMPT_GUTTER))
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

/// One theme role as a ratatui style - the conversion the sideline already
/// uses for its table cells.
fn role_style(role: Role, t: &Theme) -> RtStyle {
    let (fg, bg, flags) = theme::cell_style(role, t);
    RtStyle::new()
        .fg(rt_color(fg))
        .bg(rt_color(bg))
        .add_modifier(rt_modifier(flags))
}

/// The chips a popover drops from (and so end in the dropdown caret).
/// Project keeps Left/Right cycling; it gains the label and caret only.
pub(crate) fn is_picker_chip(f: Focus) -> bool {
    matches!(
        f,
        Focus::Harness
            | Focus::Project
            | Focus::Model
            | Focus::Effort
            | Focus::Permission
            | Focus::Placement
    )
}

/// Per-chip constraints with a one-column gap before every chip but the
/// first: each chip reads as its own box, and the gap cells stay
/// default-styled (never painted). Chip i sits at layout index 2i.
fn chip_constraints(widths: impl Iterator<Item = Constraint>) -> Vec<Constraint> {
    let mut out = Vec::new();
    for (i, c) in widths.enumerate() {
        if i > 0 {
            out.push(Constraint::Length(1));
        }
        out.push(c);
    }
    out
}

/// One chip's paint: fill the rect (so the block covers the whole chip, not
/// just the glyphs), then the label truncated to the rect with an ellipsis.
/// A picker chip reserves its last column for the dropdown caret: the label
/// ellipsizes first, the caret never truncates.
pub(crate) fn paint_chip(buf: &mut RtBuffer, r: RtRect, label: &str, style: RtStyle, caret: bool) {
    if r.width == 0 {
        return;
    }
    buf.set_style(r, style);
    let body_w = (r.width as usize).saturating_sub(usize::from(caret));
    let mut text = String::new();
    let mut w = 0usize;
    let mut truncated = false;
    for c in label.chars() {
        let cw = usize::from(UnicodeWidthChar::width(c).unwrap_or(1));
        if w + cw > body_w {
            truncated = true;
            break;
        }
        text.push(c);
        w += cw;
    }
    if truncated {
        // Reserve one column for the ellipsis over the last kept glyph.
        while w >= body_w.max(1) {
            let Some(last) = text.chars().next_back() else {
                break;
            };
            w -= usize::from(UnicodeWidthChar::width(last).unwrap_or(1));
            text.pop();
        }
        text.push('\u{2026}');
    }
    buf.set_string(r.x, r.y, &text, style);
    if caret {
        buf[(r.x + r.width - 1, r.y)].set_char('\u{25be}');
    }
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
    // The open picker swallows the press first, wherever it lands: a click
    // on a target selects and commits it, a click inside the block but off
    // a target is swallowed (the shared popup grammar), a click outside
    // dismisses the picker.
    if view.launcher.as_ref().is_some_and(|l| l.picker.is_some()) {
        let hit = view.launcher.as_ref().and_then(|l| {
            let pk = l.picker.as_ref()?;
            let r = pk.popup.render(view.term);
            if !r.contains(rep.row, rep.col) {
                return None;
            }
            let (or, oc) = r.origin;
            let line = r.lines.get((rep.row as usize).checked_sub(or)?)?;
            let col = (rep.col as usize).saturating_sub(oc);
            line.hits
                .iter()
                .find(|(_, off, len)| col >= *off && col < off + len)
                .map(|(t, _, _)| *t)
        });
        let portal = next_free_portal(view);
        // Field-disjoint snapshot for the commit path.
        let catalog = view.launcher_catalog.clone();
        if let Some(l) = view.launcher.as_mut() {
            let Some(mut picker) = l.picker.take() else {
                unreachable!("checked Some above");
            };
            match hit {
                Some(target) => {
                    picker.popup.select(target);
                    let action = picker.actions.get(target).cloned().flatten();
                    l.picker = Some(picker);
                    if let Some(action) = action {
                        apply_picker_action(l, &catalog, action, portal);
                    }
                }
                None if picker.popup.render(view.term).contains(rep.row, rep.col) => {
                    // Inside the block, off a target: swallowed, stays open.
                    l.picker = Some(picker);
                }
                None => {
                    // Outside: dismiss the picker, consume the click.
                }
            }
        }
        return Ok(true);
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
    // The SAME usable height the painter computes with (the tab strip in
    // full-screen mode, the bottom chrome row always), so a click maps onto
    // the dock that was drawn.
    let chrome = view.sideline_top() + view.bottom_row_is_chrome() as usize;
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
    if is_picker_chip(focus) && focus != Focus::Project {
        let anchor = view.launcher.as_ref().and_then(|l| picker_anchor(l, view));
        if let Some(l) = view.launcher.as_mut() {
            open_picker_at(
                l,
                &view.launcher_catalog,
                &view.backlog,
                anchor,
                focus,
            );
        }
    }
    if focus == Focus::Launch {
        submit(view, sock_w).await?;
    }
    Ok(true)
}
