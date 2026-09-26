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
use ratatui_core::layout::Rect as RtRect;
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
const MAX_LAUNCH_FLAGS_CHARS: usize = 1024;
const LAUNCH_EXTRA_AXES_PROTO: u32 = 91;

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
    /// This harness's configured model choices. OpenCode uses its own model
    /// list because its model IDs carry `provider/model`.
    pub models: Vec<ModelChoice>,
    /// A harness-specific model catalog failure, such as OpenCode's own list.
    pub models_error: Option<String>,
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

/// One configured model choice: `name` is what the model chip shows, `model`
/// is the launch id, and `route`/`provider` preserve its configured route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub name: String,
    pub model: String,
    pub route: String,
    /// Derived from the configured account route; no provider list is baked in.
    pub provider: Option<String>,
    pub verdict: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentModelChoice {
    pub harness: String,
    pub choice: ModelChoice,
}

/// The catalog read's outcome (the update-probe shape): the dock opens
/// instantly on whatever is in hand and refreshes when the probe lands. The
/// second field of `Ok` carries the account-record read's failure when model
/// lists could not be fetched; harness rows still stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogOutcome {
    Ok(Vec<HarnessChoice>, Option<String>),
    Degraded(String),
}

/// Which tab owns the keyboard. The tabs are the composer's axes; the
/// active tab's body renders the axis's full choice list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Harness,
    Provider,
    Model,
    Project,
    Permission,
    Placement,
    ExtraFlags,
    Message,
}

impl Focus {
    fn tab_order(launcher: &Launcher, catalog: &Option<CatalogOutcome>) -> Vec<Focus> {
        let mut order = vec![Focus::Harness];
        if has_multiple_providers(catalog, &launcher.draft.harness()) {
            order.push(Focus::Provider);
        }
        order.extend([
            Focus::Model,
            Focus::Project,
            Focus::Permission,
            Focus::Placement,
            Focus::ExtraFlags,
            Focus::Message,
        ]);
        order
    }

    /// The tab bar's label for this axis.
    fn tab_title(self) -> &'static str {
        match self {
            Self::Harness => "Harness",
            Self::Provider => "Provider",
            Self::Model => "Model",
            Self::Project => "Project",
            Self::Permission => "Mode",
            Self::Placement => "Where",
            Self::ExtraFlags => "Flags",
            Self::Message => "Message",
        }
    }

    /// A list tab (every tab but the two text editors).
    fn is_list(self) -> bool {
        !matches!(self, Self::Message | Self::ExtraFlags)
    }

    fn next(self, launcher: &Launcher, catalog: &Option<CatalogOutcome>) -> Focus {
        let order = Self::tab_order(launcher, catalog);
        let pos = order.iter().position(|f| *f == self).unwrap_or(0);
        order[(pos + 1) % order.len()]
    }

    fn prev(self, launcher: &Launcher, catalog: &Option<CatalogOutcome>) -> Focus {
        let order = Self::tab_order(launcher, catalog);
        let pos = order.iter().position(|f| *f == self).unwrap_or(0);
        order[(pos + order.len() - 1) % order.len()]
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
/// the sheet hidden (Esc); nothing is dropped except by an explicit terminal
/// resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launcher {
    pub draft: LaunchDraft,
    pub recent_models: Vec<RecentModelChoice>,
    /// The active tab.
    pub focus: Focus,
    pub phase: Phase,
    /// The request id this sheet's launch armed, if a launch is owned.
    pub armed: Option<u64>,
    pub next_request_id: u64,
    /// The active tab body's selection: an index into the FILTERED row list.
    pub sel: usize,
    /// The active tab body's type-to-filter query. Reset on a tab switch.
    pub filter: String,
    /// The `@` node picker over the message: the one remaining popover, a
    /// transient insert list rather than an axis editor. Keyed inside
    /// `launcher_keys` (never through `view.aux`: the aux route is raw-fed
    /// and holds Esc for the next key).
    pub picker: Option<Picker>,
}

/// What committing the highlighted row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerAction {
    /// Set the pin to this exact value (an effort, a permission mode).
    Set(String),
    /// The "<harness> decides" first row: clear the pin.
    Clear,
    ClearModel,
    SetProvider(String),
    ClearProvider,
    /// A configured routing row: pin its harness, provider and model together.
    PickRow {
        harness: String,
        name: String,
        model: String,
        route: String,
        provider: Option<String>,
    },
    /// A harness option: move to that harness
    /// and drop any model pin.
    SetHarness(String),
    /// A row of the project dropdown: pin that candidate cwd.
    SetProject(usize),
    /// Switch the pin to typed free text (effort and permission only, where
    /// the capability table declares an empty choice list).
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
    /// The backlog node a board prefill bound to this draft. The request
    /// carries it only while the message's second word still names it, so
    /// an edit that retargets or erases the message drops the binding.
    pub node: Option<String>,
    /// The model id the launch carries; empty = harness default.
    pub model: String,
    /// The configured routing row picked on the model chip.
    pub model_row: Option<String>,
    /// The provider of the selected configured route, when present.
    pub provider: String,
    pub effort: String,
    pub permission: String,
    pub placement: Placement,
    /// The portal index a thread placement opens through, resolved from the
    /// live layout when the placement is picked (next free index).
    pub placement_portal: u8,
    /// Additional spawn argv, edited directly in the flags chip.
    pub extra_flags: String,
    pub extra_flags_cursor_chars: usize,
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

    pub(crate) fn request(&self, request_id: u64) -> AgentLaunchRequest {
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
            provider: if self.harness() == "opencode" && self.model_row.is_some() {
                None
            } else {
                non_empty(&self.provider)
            },
            // The composer owns all three axes; keep the selected harness on
            // the request even when a model row also names a provider.
            model_names_harness: false,
            effort: non_empty(&self.effort),
            permission_mode: non_empty(&self.permission),
            placement: placement.map(str::to_string),
            portal,
            split: split.map(str::to_string),
            // A board prefill's binding rides only while the message's
            // second whitespace word (trailing sentence punctuation
            // trimmed, the same charset as node_seed's) is still that id.
            node: self.node.clone().filter(|id| {
                self.message
                    .split_whitespace()
                    .nth(1)
                    .map(|w| w.trim_end_matches(['.', ',', ';', ':', '!', '?']))
                    == Some(id.as_str())
            }),
            message: self.message.clone(),
            extra_flags: Vec::new(),
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

/// Split the flags field into argv without invoking a shell. Quotes and
/// backslashes group values; expansion and command substitution never run.
fn parse_extra_flags(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for c in input.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            started = true;
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => escaped = true,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\' => {
                escaped = true;
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(c);
                started = true;
            }
        }
    }
    if escaped {
        return Err("extra launch flags end with an unfinished escape".to_string());
    }
    if quote.is_some() {
        return Err("extra launch flags contain an unclosed quote".to_string());
    }
    if started {
        args.push(word);
    }
    Ok(args)
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
            recent_models: Vec::new(),
            focus: Focus::Harness,
            phase: Phase::Editing,
            armed: None,
            next_request_id: 1,
            sel: 0,
            filter: String::new(),
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
    // A partial model read re-probes too: defaults still launch, but the next
    // open retries any failed routing or harness-specific model list.
    if !matches!(
        &view.launcher_catalog,
        Some(CatalogOutcome::Ok(rows, None)) if rows.iter().all(|row| row.models_error.is_none())
    ) {
        view.catalog_want = true;
    }
}

/// Offer every catalog name as a harness choice; an unavailable one refuses
/// AT SUBMIT with its own reason (visible inline), never by disappearing.
pub(crate) fn sync_harness_names(l: &mut Launcher, catalog: &Option<CatalogOutcome>) {
    if l.draft.harnesses.is_empty() {
        if let Some(CatalogOutcome::Ok(rows, _)) = catalog {
            l.draft.harnesses = rows.iter().map(|r| r.name.clone()).collect();
            if l.draft.harness_idx >= rows.len() {
                l.draft.harness_idx = 0;
            }
        }
    }
    if l.focus == Focus::Provider && !has_multiple_providers(catalog, &l.draft.harness()) {
        l.focus = Focus::Model;
        l.picker = None;
    }
}

/// Open the launcher with a board prefill: the target message, the node's
/// project and the node binding. Assumes the dock is visible (`show_composer`
/// ran) and reuses [`open`]'s retained-draft reveal when it is not. A kept
/// non-empty draft is never overwritten: the error names the way out, and
/// the operator's harness, model and effort picks stay the session's
/// remembered ones - only message, project and node bind.
pub(crate) fn open_with(
    view: &mut View,
    message: String,
    cwd: Option<&str>,
    node: String,
) -> Result<(), String> {
    // A dock already on screen keeps its state: open() only ever ran on a
    // closed dock before this seam, and re-running it would wipe a held
    // draft with a fresh one.
    if view.launcher.is_none() {
        open(view);
    }
    let launcher = view
        .launcher
        .as_mut()
        .expect("the dock is open after open()");
    // An empty draft always yields. Otherwise only a terminal `Launched`
    // attempt makes way: a refused or unknown one keeps its reason on
    // screen, and an in-flight one must never be replaced.
    let attempt = view.launch_attempt.as_ref().map(|a| &a.state);
    let replaceable = !matches!(attempt, Some(AttemptState::Starting))
        && (launcher.draft.message.is_empty()
            || matches!(attempt, Some(AttemptState::Launched { .. })));
    if !replaceable {
        return Err("the launcher holds a draft; empty it and press t again".to_string());
    }
    launcher.draft.message = message.clone();
    launcher.draft.cursor_chars = message.chars().count();
    launcher.draft.node = Some(node);
    if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
        let idx = launcher
            .draft
            .projects
            .iter()
            .position(|p| p == dir)
            .unwrap_or_else(|| {
                launcher.draft.projects.push(dir.to_string());
                launcher.draft.projects.len() - 1
            });
        launcher.draft.project_idx = idx;
    }
    launcher.phase = Phase::Editing;
    launcher.focus = Focus::Harness;
    launcher.draft.bump();
    Ok(())
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
        node: None,
        model: String::new(),
        model_row: None,
        provider: String::new(),
        effort: String::new(),
        permission: String::new(),
        placement: Placement::default(),
        placement_portal: 0,
        extra_flags: String::new(),
        extra_flags_cursor_chars: 0,
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
    if !l.draft.provider.is_empty() && l.draft.model.is_empty() {
        l.phase = Phase::Refused {
            request_id,
            reason: "choose a model for the selected provider".to_string(),
        };
        return Ok(());
    }
    let extra_flags = match parse_extra_flags(&l.draft.extra_flags)
        .and_then(|flags| crate::dispatch_launch::validate_extra_flags(&flags).map(|_| flags))
    {
        Ok(flags) => flags,
        Err(reason) => {
            l.phase = Phase::Refused { request_id, reason };
            return Ok(());
        }
    };
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
    let mut request = l.draft.request(request_id);
    request.extra_flags = extra_flags;
    if (request.provider.is_some() || !request.extra_flags.is_empty())
        && !supports_launch_extra_axes(&view.session)
    {
        l.phase = Phase::Refused {
            request_id,
            reason: "the connected mux server does not support provider pins or extra launch flags; reconnect to a server running current fno".to_string(),
        };
        return Ok(());
    }
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

fn supports_launch_extra_axes(session: &str) -> bool {
    let version = crate::proto::socket_path(session)
        .ok()
        .and_then(|socket| crate::mux_rows::read_wire_version(&socket));
    launch_extra_axes_supported(version)
}

pub(crate) fn launch_extra_axes_supported(version: Option<u32>) -> bool {
    version.is_some_and(|version| version >= LAUNCH_EXTRA_AXES_PROTO)
}

// -- input folding -----------------------------------------------------------

/// One folded key. The launcher folds its OWN bytes (not the shared
/// selector folds) because the message field needs a newline byte told
/// apart from Enter, Tab navigation, and bracketed paste as data -
/// semantics the selector folds would misread as commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LKey {
    Esc,
    /// Enter (`\r`): opens a list, picks, or launches.
    Enter,
    /// Ctrl-J (`\n`): inserts a newline in the message, never a launch.
    CtrlJ,
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
            // Escape sequences via a small CSI/SS3 accumulator: Esc, Tab,
            // Shift-Tab, arrows. The whole-sequence swallow rule is the one
            // shared helper (`input_folds::esc_step`): a sequence ends at its
            // final byte (0x40-0x7e) or the carry ceiling, and an unknown
            // sequence (a focus report ESC [ I, a modified arrow) is dropped
            // WHOLE - no byte of it ever lands as text.
            match self.esc.last().copied() {
                None if b == 0x1b => {
                    self.esc.push(b);
                    continue;
                }
                Some(0x1b) => {
                    if (b == b'[' || b == b'O') && self.esc.len() == 1 {
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
                Some(b'[') | Some(b'O') => {
                    let mut reprocess = false;
                    match super::input_folds::esc_step(&mut self.esc, b) {
                        super::input_folds::EscStep::Carried => {}
                        super::input_folds::EscStep::Reprocess => {
                            // Malformed mid-sequence byte: abandon the
                            // sequence and let the plain-byte pass below
                            // handle this byte.
                            self.esc.clear();
                            reprocess = true;
                        }
                        super::input_folds::EscStep::Final => {
                            let seq: Vec<u8> = self.esc[1..].to_vec();
                            self.esc.clear();
                            if seq == b"[200~" {
                                self.paste = Some(Vec::new());
                                continue;
                            }
                            if let Some(k) = arrow_key(&seq) {
                                keys.push(k);
                            }
                        }
                    }
                    // Carried and Final consumed the byte; only a
                    // malformed-sequence byte falls through to the plain
                    // pass below.
                    if !reprocess {
                        continue;
                    }
                }
                _ => {}
            }
            // Plain bytes.
            match b {
                b'\r' => keys.push(LKey::Enter),
                b'\n' => keys.push(LKey::CtrlJ),
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

/// A completed CSI/SS3 sequence's arrow mapping: bare and modified (`[1;5B`)
/// CSI arrows and SS3 `O A..D` application-mode arrows become the launcher's
/// arrow keys; everything else (a focus report, a function key) is dropped
/// whole. `seq` is the sequence text between the ESC introducer's follower
/// ([ or O) and the final byte, both included.
fn arrow_key(seq: &[u8]) -> Option<LKey> {
    if seq == b"[Z" {
        return Some(LKey::BackTab);
    }
    let arrow = match *seq.last()? {
        b'A' => LKey::Up,
        b'B' => LKey::Down,
        b'C' => LKey::Right,
        b'D' => LKey::Left,
        _ => return None,
    };
    let ok = match seq[0] {
        b'[' => {
            seq.len() == 2
                || seq[1..seq.len() - 1]
                    .iter()
                    .all(|b| (0x20..=0x3f).contains(b))
        }
        b'O' => seq.len() == 2,
        _ => false,
    };
    ok.then_some(arrow)
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

fn insert_extra_flag_char(draft: &mut LaunchDraft, c: char) {
    if draft.extra_flags.chars().count() >= MAX_LAUNCH_FLAGS_CHARS {
        return;
    }
    let byte = char_byte(&draft.extra_flags, draft.extra_flags_cursor_chars);
    draft.extra_flags.insert(byte, c);
    draft.extra_flags_cursor_chars += 1;
    draft.bump();
}

fn backspace_extra_flag(draft: &mut LaunchDraft) {
    if draft.extra_flags_cursor_chars == 0 {
        return;
    }
    let cur = char_byte(&draft.extra_flags, draft.extra_flags_cursor_chars);
    let prev = char_byte(&draft.extra_flags, draft.extra_flags_cursor_chars - 1);
    draft.extra_flags.replace_range(prev..cur, "");
    draft.extra_flags_cursor_chars -= 1;
    draft.bump();
}

fn move_extra_flag_cursor(draft: &mut LaunchDraft, delta: i32) {
    let len = draft.extra_flags.chars().count();
    draft.extra_flags_cursor_chars = if delta < 0 {
        draft.extra_flags_cursor_chars.saturating_sub(1)
    } else {
        (draft.extra_flags_cursor_chars + 1).min(len)
    };
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
        // The open picker consumes the keys first: Up/Down move, Left/Right
        // cycle effort in the model picker, Enter commits, Esc closes the
        // PICKER only (a second Esc closes the composer). Everything else -
        // Tab included - falls through to the dock.
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
                    LKey::Left | LKey::Right if picker.field == Focus::Model => {
                        // In the model list the arrows cycle the current
                        // harness's effort (default first); the list rebuilds
                        // so the chip's value stays the one truth.
                        let delta = if matches!(key, LKey::Left) { -1 } else { 1 };
                        cycle_effort(l, delta, &catalog);
                        rebuild_picker(l, picker);
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
                // While an attempt is pending the first Esc is the explicit
                // cancel: an Unknown resolves back to editing, a Starting
                // attempt disarms (a fresh launch arms a new id, one attempt
                // each). The NEXT Esc closes. Otherwise: hide, retain the
                // draft; a submitted launch keeps running, and the
                // full-screen sideline leaves with it so keys never reach a
                // pane that is not painted.
                let pending = view.launcher.as_ref().is_some_and(|l| {
                    matches!(l.phase, Phase::Unknown { .. } | Phase::Submitting { .. })
                });
                if pending {
                    if let Some(l) = view.launcher.as_mut() {
                        l.phase = Phase::Editing;
                        l.armed = None;
                    }
                } else {
                    if view.sideline_full {
                        view.sideline_full = false;
                    }
                    close(view);
                    break;
                }
            }
            LKey::Tab | LKey::BackTab => {
                let delta = if matches!(key, LKey::Tab) { 1 } else { -1 };
                if let Some(l) = view.launcher.as_mut() {
                    let focus = if delta > 0 {
                        l.focus.next(l, &view.launcher_catalog)
                    } else {
                        l.focus.prev(l, &view.launcher_catalog)
                    };
                    if focus != l.focus {
                        // A tab switch resets the body: each tab's list
                        // starts unfiltered at the top.
                        l.sel = 0;
                        l.filter.clear();
                    }
                    l.focus = focus;
                }
            }
            LKey::Up | LKey::Down => {
                let delta = if matches!(key, LKey::Up) { -1 } else { 1 };
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => move_up_down(&mut l.draft, delta),
                        f if f.is_list() => {
                            // Move the selection; header rows are captions,
                            // never targets, so they are stepped over.
                            let (rows, _) = tab_body_rows(l, &view.launcher_catalog, &view.backlog);
                            l.sel = step_selection(&rows, l.sel, delta);
                        }
                        _ => {}
                    }
                }
            }
            LKey::Left | LKey::Right => {
                let delta = if matches!(key, LKey::Left) { -1 } else { 1 };
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => {
                            if delta < 0 {
                                move_left(&mut l.draft);
                            } else {
                                move_right(&mut l.draft);
                            }
                        }
                        Focus::ExtraFlags => move_extra_flag_cursor(&mut l.draft, delta),
                        f if f.is_list() => {
                            // The tab switch: the arrows walk the tab bar.
                            let focus = if delta > 0 {
                                l.focus.next(l, &view.launcher_catalog)
                            } else {
                                l.focus.prev(l, &view.launcher_catalog)
                            };
                            if focus != l.focus {
                                l.sel = 0;
                                l.filter.clear();
                            }
                            l.focus = focus;
                        }
                        _ => {}
                    }
                }
            }
            LKey::Backspace => {
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        Focus::Message => backspace(&mut l.draft),
                        Focus::ExtraFlags => backspace_extra_flag(&mut l.draft),
                        Focus::Permission => {
                            l.draft.permission.pop();
                            l.draft.bump();
                        }
                        f if f.is_list() => {
                            l.filter.pop();
                            l.sel = 0;
                        }
                        _ => {}
                    }
                }
            }
            LKey::CtrlJ => {
                // Message: a newline, never a launch. List tabs: the
                // launch-from-anywhere key.
                let in_message = view
                    .launcher
                    .as_ref()
                    .is_some_and(|l| l.focus == Focus::Message);
                if in_message {
                    if let Some(l) = view.launcher.as_mut() {
                        insert_char(&mut l.draft, '\n');
                    }
                } else if launcher_can_launch(view) {
                    submit(view, sock_w).await?;
                }
            }
            LKey::Enter => {
                let (focus, pending) = view
                    .launcher
                    .as_ref()
                    .map(|l| {
                        (
                            l.focus,
                            matches!(l.phase, Phase::Unknown { .. } | Phase::Submitting { .. }),
                        )
                    })
                    .unwrap_or((Focus::Message, false));
                match focus {
                    Focus::Message if !pending => submit(view, sock_w).await?,
                    f if f.is_list() && !pending => {
                        // Commit the selected row; a header or a disabled
                        // row carries no action and the sheet stays as it is.
                        let portal = next_free_portal(view);
                        let catalog = view.launcher_catalog.clone();
                        let action = view.launcher.as_ref().and_then(|l| {
                            let (rows, actions) =
                                tab_body_rows(l, &view.launcher_catalog, &view.backlog);
                            let sel_eff = effective_sel(&rows, l.sel);
                            actions.get(sel_eff).cloned().flatten()
                        });
                        if let Some(action) = action {
                            if let Some(l) = view.launcher.as_mut() {
                                apply_picker_action(l, &catalog, action, portal);
                            }
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
                        Focus::ExtraFlags => insert_extra_flag_char(&mut l.draft, c),
                        Focus::Permission => {
                            // Free text only where the capability table
                            // declares an empty choice list; the TypeIn row
                            // is the only door here.
                            if l.draft.permission.chars().count() < 64 {
                                l.draft.permission.push(c);
                                l.draft.bump();
                            }
                        }
                        f if f.is_list() => {
                            // Type-to-filter: the query narrows the body in
                            // place; the visible list is the feedback.
                            l.filter.push(c);
                            l.sel = 0;
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
                    } else if l.focus == Focus::ExtraFlags {
                        let room = MAX_LAUNCH_FLAGS_CHARS
                            .saturating_sub(l.draft.extra_flags.chars().count());
                        for c in text.chars().take(room) {
                            let c = if c.is_whitespace() { ' ' } else { c };
                            insert_extra_flag_char(&mut l.draft, c);
                        }
                    }
                    // A paste while another tab is focused: the bytes are
                    // data and are dropped, never forwarded to a pane.
                }
            }
        }
    }
    Ok(StdinFlow::Continue)
}

/// Cycle the current harness's effort one step (`delta` -1/+1) through the
/// capability table's list, the empty pin (the harness default) first. A
/// harness with no effort list keeps its pin untouched.
fn cycle_effort(l: &mut Launcher, delta: i32, catalog: &Option<CatalogOutcome>) {
    let harness = l.draft.harness();
    let Some(CatalogOutcome::Ok(rows, _)) = catalog else {
        return;
    };
    let Some(row) = rows.iter().find(|r| r.name == harness) else {
        return;
    };
    let Some(efforts) = row.efforts.as_ref().filter(|e| !e.is_empty()) else {
        return;
    };
    // "" (the default) is slot 0, the declared efforts follow.
    let mut order: Vec<&str> = Vec::with_capacity(efforts.len() + 1);
    order.push("");
    order.extend(efforts.iter().map(String::as_str));
    let cur = order.iter().position(|e| *e == l.draft.effort).unwrap_or(0);
    let n = order.len() as i32;
    let next = ((cur as i32 + delta).rem_euclid(n)) as usize;
    l.draft.effort = order[next].to_string();
    l.draft.bump();
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
        if !row
            .models
            .iter()
            .any(|m| &m.name == name && m.provider.as_deref().unwrap_or_default() == draft.provider)
        {
            draft.model.clear();
            draft.model_row = None;
            draft.bump();
        }
    }
    if !draft.provider.is_empty()
        && !row
            .models
            .iter()
            .any(|m| m.provider.as_deref() == Some(draft.provider.as_str()))
    {
        draft.provider.clear();
        draft.bump();
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

fn catalog_models<'a>(catalog: &'a Option<CatalogOutcome>, harness: &str) -> Vec<&'a ModelChoice> {
    let Some(CatalogOutcome::Ok(rows, _)) = catalog else {
        return Vec::new();
    };
    rows.iter()
        .find(|row| row.name == harness)
        .map(|row| row.models.iter().collect())
        .unwrap_or_default()
}

fn providers_for_harness(catalog: &Option<CatalogOutcome>, harness: &str) -> Vec<String> {
    let mut providers: Vec<String> = catalog_models(catalog, harness)
        .into_iter()
        .filter_map(|model| model.provider.clone())
        .collect();
    providers.sort();
    providers.dedup();
    providers
}

fn has_multiple_providers(catalog: &Option<CatalogOutcome>, harness: &str) -> bool {
    let models = catalog_models(catalog, harness);
    let explicit = providers_for_harness(catalog, harness).len();
    let default = usize::from(models.iter().any(|model| model.provider.is_none()));
    explicit + default > 1
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

/// The catalog read: a compiled-in table plus PATH stats, then bounded reads
/// for configured routing rows and OpenCode's installed model list. Delivered
/// through the probe channel so the dock has one "not yet read" and "read"
/// shape; async because both reads are subprocesses.
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
                models_error: None,
                efforts,
                permission_modes,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    if rows.is_empty() {
        return CatalogOutcome::Degraded("harness catalog: empty capability table".into());
    }
    // Non-OpenCode model choices come from configured account records.
    // OpenCode owns its provider/model IDs and supplies them through its model
    // list command. Both reads are bounded and run off the UI loop.
    let bin = crate::server::fno_bin().to_string_lossy().into_owned();
    let account_argv = [bin.as_str(), "config", "get", "accounts.records", "-J"];
    let opencode_argv = ["opencode", "models", "--pure"];
    let opencode_installed = rows
        .iter()
        .any(|row| row.name == "opencode" && row.selectable());
    let timeout = std::time::Duration::from_secs(30);
    let deadline = tokio::time::Instant::now() + timeout;
    let (accounts, opencode) = tokio::join!(
        crate::dispatch_launch::run_fno_captured(&account_argv, timeout, deadline),
        async {
            if opencode_installed {
                crate::dispatch_launch::run_fno_captured(&opencode_argv, timeout, deadline).await
            } else {
                None
            }
        }
    );
    let (by_harness, models_err) = match accounts {
        Some((true, stdout, _)) => match parse_configured_account_models(&stdout) {
            Ok(rows) => (rows, None),
            Err(error) => (std::collections::HashMap::new(), Some(error)),
        },
        Some((false, _, stderr)) => (
            std::collections::HashMap::new(),
            Some(format!("account records unavailable: {}", stderr.trim())),
        ),
        None => (
            std::collections::HashMap::new(),
            Some("account records could not be read".to_string()),
        ),
    };
    let (opencode_models, opencode_error) = if opencode_installed {
        match opencode {
            Some((true, stdout, _)) => {
                let models = parse_opencode_models(&stdout);
                if models.is_empty() {
                    (
                        models,
                        Some("opencode models returned no provider/model rows".into()),
                    )
                } else {
                    (models, None)
                }
            }
            Some((false, _, stderr)) => (
                Vec::new(),
                Some(format!("opencode models failed: {}", stderr.trim())),
            ),
            None => (Vec::new(), Some("opencode models could not be read".into())),
        }
    } else {
        (Vec::new(), None)
    };
    for row in &mut rows {
        if row.name == "opencode" {
            row.models = opencode_models.clone();
            row.models_error = opencode_error.clone();
        } else if let Some(list) = by_harness.get(&row.name) {
            row.models = list.clone();
        }
    }
    CatalogOutcome::Ok(rows, models_err)
}

pub(crate) fn provider_from_route(route: &str) -> Option<String> {
    route
        .split_once('/')
        .map(|(provider, _)| provider.trim())
        .filter(|provider| !provider.is_empty())
        .map(str::to_string)
}

pub(crate) fn parse_configured_account_models(
    stdout: &str,
) -> Result<std::collections::HashMap<String, Vec<ModelChoice>>, String> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|_| "account records response was unreadable".to_string())?;
    let records_value = value
        .get("value")
        .ok_or_else(|| "account records response had no value list".to_string())?;
    if records_value.is_null() {
        return Ok(std::collections::HashMap::new());
    }
    let records = records_value
        .as_array()
        .ok_or_else(|| "account records response had no value list".to_string())?;
    let mut by_harness: std::collections::HashMap<String, Vec<ModelChoice>> =
        std::collections::HashMap::new();
    for record in records {
        let Some(harness) = record.get("harness").and_then(|v| v.as_str()) else {
            continue;
        };
        let route = record
            .get("route")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        let declared_provider = record
            .get("route_provider_id")
            .or_else(|| record.get("provider"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|provider| !provider.is_empty());
        let declared_model = record
            .get("model_name")
            .or_else(|| record.get("model"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let route_provider = provider_from_route(route);
        let route_model = route.split_once('/').map(|(_, model)| model.trim());
        let provider = declared_provider
            .map(str::to_string)
            .or(route_provider)
            .or_else(|| declared_model.and_then(provider_from_route));
        let model_source = declared_model.or(route_model);
        let Some(model_source) = model_source.filter(|model| !model.is_empty()) else {
            continue;
        };
        let model_id = provider
            .as_ref()
            .and_then(|provider| {
                model_source
                    .strip_prefix(provider)
                    .and_then(|rest| rest.strip_prefix('/'))
            })
            .unwrap_or(model_source)
            .to_string();
        let route = if route.is_empty() {
            provider
                .as_ref()
                .map(|provider| format!("{provider}/{model_id}"))
                .unwrap_or_default()
        } else {
            route.to_string()
        };
        let name = if model_id.is_empty() {
            continue;
        } else {
            model_id.clone()
        };
        let choices = by_harness.entry(harness.to_string()).or_default();
        if choices
            .iter()
            .any(|choice| choice.model == model_id && choice.provider == provider)
        {
            continue;
        }
        choices.push(ModelChoice {
            name,
            model: model_id,
            route,
            provider,
            verdict: "ok".to_string(),
        });
    }
    Ok(by_harness)
}

pub(crate) fn parse_opencode_models(stdout: &str) -> Vec<ModelChoice> {
    let mut models = Vec::new();
    for id in stdout.lines().map(str::trim).filter(|id| !id.is_empty()) {
        let Some(provider) = provider_from_route(id) else {
            continue;
        };
        if models.iter().any(|model: &ModelChoice| model.name == id) {
            continue;
        }
        models.push(ModelChoice {
            name: id.to_string(),
            model: id.to_string(),
            route: String::new(),
            provider: Some(provider),
            verdict: "ok".to_string(),
        });
    }
    models
}

// -- picker ------------------------------------------------------------------
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
    if field == Focus::Provider && !has_multiple_providers(catalog, &l.draft.harness()) {
        return false;
    }
    let (rows, actions) = picker_rows(l, catalog, backlog);
    let (all_rows, all_actions) = (rows.clone(), actions.clone());
    // The model picker names its effort-cycling grammar in the footer.
    let footer = if field == Focus::Model {
        "up/down move \u{b7} left/right effort \u{b7} type to filter \u{b7} enter pick \u{b7} esc back"
    } else {
        "up/down move \u{b7} type to filter \u{b7} enter pick \u{b7} esc close"
    };
    let title = title_for(field);
    let mut popup = Popup::new(rows, Anchor::At { row, col })
        .footer(footer)
        .full_chrome()
        .full_width_selection();
    if !title.is_empty() {
        popup = popup.title(title);
    }
    l.picker = Some(Picker {
        popup,
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
/// the entry labels, case-insensitive. The query rides the TITLE (never a
/// selectable header row, never a value); a header whose rows all filtered
/// away hides with them. Works off the picker's own captured row set, so it
/// needs no View access.
fn rebuild_picker(l: &mut Launcher, mut picker: Picker) {
    let q = picker.filter.to_lowercase();
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    let mut last_header: Option<(PopupRow, Option<PickerAction>)> = None;
    for (row, action) in picker
        .all_rows
        .iter()
        .cloned()
        .zip(picker.all_actions.iter().cloned())
    {
        match &row {
            PopupRow::Header(_) => last_header = Some((row, action)),
            PopupRow::Entry { label, .. } => {
                if !q.is_empty() && !label.to_lowercase().contains(&q) {
                    continue;
                }
                if let Some((hr, ha)) = last_header.take() {
                    rows.push(hr);
                    actions.push(ha);
                }
                rows.push(row);
                actions.push(action);
            }
            _ => {
                rows.push(row);
                actions.push(action);
            }
        }
    }
    let footer = if picker.field == Focus::Model {
        "up/down move \u{b7} left/right effort \u{b7} type to filter \u{b7} enter pick \u{b7} esc back"
    } else {
        "up/down move \u{b7} type to filter \u{b7} enter pick \u{b7} esc close"
    };
    let mut popup = Popup::new(rows, picker.anchor)
        .footer(footer)
        .full_chrome()
        .full_width_selection();
    if !picker.filter.is_empty() {
        popup = popup.title(format!(
            "{} \u{b7} filter: {}",
            title_for(picker.field),
            picker.filter
        ));
    } else {
        let t = title_for(picker.field);
        if !t.is_empty() {
            popup = popup.title(t);
        }
    }
    picker.popup = popup;
    picker.popup.sel = 0;
    picker.actions = actions;
    l.picker = Some(picker);
}

/// The picker's base title, shared by open and rebuild so the filter state
/// can never split the two spellings apart.
fn title_for(field: Focus) -> String {
    match field {
        Focus::Harness => "harness".to_string(),
        Focus::Provider => "provider".to_string(),
        Focus::Model => "model".to_string(),
        Focus::Project => "project".to_string(),
        _ => String::new(),
    }
}

/// The `@` node picker's anchor: the editor's cell inside the sheet, in the
/// same geometry `launcher_mouse` maps with. The axis pickers are gone; the
/// `@` insert list is the one remaining popover.
fn picker_anchor(l: &Launcher, view: &View) -> Option<(u16, u16)> {
    if l.focus != Focus::Message {
        return None;
    }
    let sl = l.sheet_layout(view)?;
    let (oy, ox) = (sl.origin.0 as usize + 1, sl.origin.1 as usize + 1);
    Some((
        (oy + sl.message.y as usize + 1) as u16,
        (ox + sl.message.x as usize) as u16,
    ))
}

/// The popover's rows and their commit actions for `field`, read off the
/// catalog and the live draft. An unavailable choice renders as a disabled
/// One picker row push: a free fn over the two vecs, so no closure has to
/// hold a mutable borrow across the row builder's direct header pushes.
fn push_entry(
    rows: &mut Vec<PopupRow>,
    actions: &mut Vec<Option<PickerAction>>,
    glyph: &str,
    label: &str,
    hint: &str,
    enabled: bool,
    action: Option<PickerAction>,
) {
    rows.push(PopupRow::Entry {
        glyph: glyph.to_string(),
        label: label.to_string(),
        hint: hint.to_string(),
        enabled,
    });
    actions.push(action);
}

/// entry carrying its reason (the popup's greyed-with-reason grammar).
fn picker_rows(
    l: &Launcher,
    catalog: &Option<CatalogOutcome>,
    backlog: &[crate::proto::BacklogCard],
) -> (Vec<PopupRow>, Vec<Option<PickerAction>>) {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    let harness = l.draft.harness();
    let decides = if harness.is_empty() {
        "harness decides".to_string()
    } else {
        format!("{harness} decides")
    };
    match l.focus {
        Focus::Harness => match catalog {
            Some(CatalogOutcome::Ok(rows_found, _)) => {
                for h in rows_found.iter().filter(|h| h.selectable()) {
                    push_entry(
                        &mut rows,
                        &mut actions,
                        if h.name == harness {
                            "\u{2713}"
                        } else {
                            "\u{2022}"
                        },
                        &h.name,
                        "",
                        true,
                        Some(PickerAction::SetHarness(h.name.clone())),
                    );
                }
                if rows.is_empty() {
                    push_entry(
                        &mut rows,
                        &mut actions,
                        "\u{2022}",
                        "no harness installed",
                        "install a harness, then reopen",
                        false,
                        None,
                    );
                }
            }
            None => push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                "reading harnesses...",
                "",
                false,
                None,
            ),
            Some(CatalogOutcome::Degraded(error)) => push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                "harness list unavailable",
                error,
                false,
                None,
            ),
        },
        Focus::Provider => {
            push_entry(
                &mut rows,
                &mut actions,
                if l.draft.provider.is_empty() {
                    "\u{2713}"
                } else {
                    "\u{2022}"
                },
                "any provider",
                "",
                true,
                Some(PickerAction::ClearProvider),
            );
            let providers = providers_for_harness(catalog, &harness);
            for provider in providers {
                push_entry(
                    &mut rows,
                    &mut actions,
                    if provider == l.draft.provider {
                        "\u{2713}"
                    } else {
                        "\u{2022}"
                    },
                    &provider,
                    "",
                    true,
                    Some(PickerAction::SetProvider(provider.clone())),
                );
            }
            if rows.len() == 1 {
                push_entry(
                    &mut rows,
                    &mut actions,
                    "\u{2022}",
                    "no configured providers",
                    "from this harness",
                    false,
                    None,
                );
            }
        }
        Focus::Model => match catalog {
            Some(CatalogOutcome::Ok(rows_found, models_err)) => {
                let models = rows_found
                    .iter()
                    .find(|row| row.name == harness)
                    .map(|row| &row.models);
                let recent: Vec<&RecentModelChoice> = l
                    .recent_models
                    .iter()
                    .filter(|recent| {
                        recent.harness == harness
                            && models.is_some_and(|models| {
                                models.iter().any(|model| {
                                    model.name == recent.choice.name
                                        && model.model == recent.choice.model
                                })
                            })
                            && (l.draft.provider.is_empty()
                                || recent.choice.provider.as_deref()
                                    == Some(l.draft.provider.as_str()))
                    })
                    .take(5)
                    .collect();
                if !recent.is_empty() {
                    rows.push(PopupRow::Header("recent".to_string()));
                    actions.push(None);
                    for recent in recent {
                        push_entry(
                            &mut rows,
                            &mut actions,
                            "\u{2022}",
                            &recent.choice.name,
                            &recent.choice.route,
                            true,
                            Some(PickerAction::PickRow {
                                harness: recent.harness.clone(),
                                name: recent.choice.name.clone(),
                                model: recent.choice.model.clone(),
                                route: recent.choice.route.clone(),
                                provider: recent.choice.provider.clone(),
                            }),
                        );
                    }
                }
                rows.push(PopupRow::Header("configured".to_string()));
                actions.push(None);
                push_entry(
                    &mut rows,
                    &mut actions,
                    if l.draft.model.is_empty() {
                        "\u{2713}"
                    } else {
                        "\u{2022}"
                    },
                    "harness default",
                    "",
                    true,
                    Some(PickerAction::ClearModel),
                );
                if let Some(models) = models {
                    for m in models.iter().filter(|m| {
                        l.draft.provider.is_empty()
                            || m.provider.as_deref() == Some(l.draft.provider.as_str())
                    }) {
                        let check = Some(&m.name) == l.draft.model_row.as_ref()
                            && !l.draft.model.is_empty();
                        let hint = if m.route.is_empty() && m.model != m.name {
                            m.model.clone()
                        } else if m.route == m.model {
                            String::new()
                        } else {
                            m.route.clone()
                        };
                        push_entry(
                            &mut rows,
                            &mut actions,
                            if check { "\u{2713}" } else { "\u{2022}" },
                            &m.name,
                            &hint,
                            m.verdict == "ok",
                            (m.verdict == "ok").then(|| PickerAction::PickRow {
                                harness: harness.clone(),
                                name: m.name.clone(),
                                model: m.model.clone(),
                                route: m.route.clone(),
                                provider: m.provider.clone(),
                            }),
                        );
                    }
                }
                let harness_error = rows_found
                    .iter()
                    .find(|row| row.name == harness)
                    .and_then(|row| row.models_error.as_deref());
                if let Some(error) = harness_error {
                    push_entry(
                        &mut rows,
                        &mut actions,
                        "\u{2022}",
                        "model list unavailable",
                        error,
                        false,
                        None,
                    );
                } else if harness != "opencode" {
                    if let Some(error) = models_err {
                        push_entry(
                            &mut rows,
                            &mut actions,
                            "\u{2022}",
                            "model list unavailable",
                            error,
                            false,
                            None,
                        );
                    }
                }
            }
            None => push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                "reading models...",
                "",
                false,
                None,
            ),
            Some(CatalogOutcome::Degraded(error)) => push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                "model list unavailable",
                error,
                false,
                None,
            ),
        },
        Focus::Project => {
            // The project dropdown: the draft's candidate cwds, basename as
            // the label, the full path as the hint. Enter on the chip opens
            // this; it never launches.
            if l.draft.projects.is_empty() {
                push_entry(
                    &mut rows,
                    &mut actions,
                    "\u{2022}",
                    "no project candidates",
                    "",
                    false,
                    None,
                );
            }
            for (i, p) in l.draft.projects.iter().enumerate() {
                let base = p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p);
                let glyph = if i == l.draft.project_idx {
                    "\u{2713}"
                } else {
                    "\u{2022}"
                };
                push_entry(
                    &mut rows,
                    &mut actions,
                    glyph,
                    base,
                    p,
                    true,
                    Some(PickerAction::SetProject(i)),
                );
            }
        }
        Focus::Permission => {
            push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                &decides,
                "",
                true,
                Some(PickerAction::Clear),
            );
            if let Some(CatalogOutcome::Ok(catalog_rows, _)) = catalog {
                if let Some(row) = catalog_rows.iter().find(|r| r.name == harness) {
                    if let Some(modes) = &row.permission_modes {
                        if modes.is_empty() {
                            push_entry(
                                &mut rows,
                                &mut actions,
                                "\u{2022}",
                                "type a permission...",
                                "",
                                true,
                                Some(PickerAction::TypeIn),
                            );
                        } else {
                            for m in modes {
                                push_entry(
                                    &mut rows,
                                    &mut actions,
                                    "\u{2022}",
                                    m,
                                    "",
                                    true,
                                    Some(PickerAction::Set(m.clone())),
                                );
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
                push_entry(
                    &mut rows,
                    &mut actions,
                    "\u{2022}",
                    &format!("{} {}", card.id, card.slug),
                    &card.priority,
                    true,
                    Some(PickerAction::InsertNode(card.id.clone())),
                );
            }
            if backlog.is_empty() {
                push_entry(
                    &mut rows,
                    &mut actions,
                    "\u{2022}",
                    "no backlog cards",
                    "",
                    false,
                    None,
                );
            }
        }
        Focus::Placement => {
            push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                Placement::Thread.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::Thread)),
            );
            push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                Placement::ThreadSplitBeside.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::ThreadSplitBeside)),
            );
            push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                Placement::ThreadNewTab.label(),
                "",
                true,
                Some(PickerAction::Place(Placement::ThreadNewTab)),
            );
            push_entry(
                &mut rows,
                &mut actions,
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
            Focus::Permission => {
                l.draft.permission = name;
                l.draft.bump();
            }
            _ => {}
        },
        PickerAction::PickRow {
            harness,
            name,
            model,
            route,
            provider,
        } => {
            // The row's harness moves first so the pin the pick carries is
            // judged against the RIGHT harness's rows; the picked model
            // survives that judgment by construction.
            if let Some(idx) = l.draft.harnesses.iter().position(|h| *h == harness) {
                l.draft.harness_idx = idx;
            }
            l.draft.model = model;
            l.draft.model_row = Some(name.clone());
            l.draft.provider = provider.unwrap_or_default();
            let recent_model = ModelChoice {
                name: name.clone(),
                model: l.draft.model.clone(),
                route,
                provider: non_empty(&l.draft.provider),
                verdict: "ok".to_string(),
            };
            l.recent_models
                .retain(|recent| recent.harness != harness || recent.choice.name != name);
            l.recent_models.insert(
                0,
                RecentModelChoice {
                    harness: harness.clone(),
                    choice: recent_model,
                },
            );
            l.recent_models.truncate(5);
            l.draft.bump();
            clear_unoffered_pins(&mut l.draft, catalog);
        }
        PickerAction::SetProvider(provider) => {
            l.draft.provider = provider;
            if let Some(name) = &l.draft.model_row {
                if !catalog_models(catalog, &l.draft.harness()).iter().any(|m| {
                    &m.name == name && m.provider.as_deref() == Some(l.draft.provider.as_str())
                }) {
                    l.draft.model.clear();
                    l.draft.model_row = None;
                }
            }
            l.draft.bump();
        }
        PickerAction::ClearProvider => {
            l.draft.provider.clear();
            l.draft.model.clear();
            l.draft.model_row = None;
            l.draft.bump();
        }
        PickerAction::ClearModel => {
            l.draft.model.clear();
            l.draft.model_row = None;
            l.draft.provider.clear();
            l.draft.bump();
        }
        PickerAction::SetHarness(name) => {
            if let Some(idx) = l.draft.harnesses.iter().position(|h| *h == name) {
                l.draft.harness_idx = idx;
                l.draft.bump();
                // The default row drops a model pin the new harness's rows
                // cannot name; its own effort/permission judgment follows.
                l.draft.model.clear();
                l.draft.model_row = None;
                clear_unoffered_pins(&mut l.draft, catalog);
            }
        }
        PickerAction::SetProject(i) => {
            if i < l.draft.projects.len() {
                l.draft.project_idx = i;
                l.draft.bump();
            }
        }
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
        PickerAction::Clear => {
            // The permission chip's "<harness> decides" row: the pin
            // clears and the harness default takes over.
            if l.focus == Focus::Permission {
                l.draft.permission.clear();
                l.draft.bump();
            }
        }
        PickerAction::TypeIn => {
            // Permission keeps free text only where the capability table
            // declares an empty choice list; the pin clears and the next
            // typed chars are the value.
            if l.focus == Focus::Permission {
                l.draft.permission.clear();
                l.draft.bump();
            }
        }
    }
}

// -- render ------------------------------------------------------------------

/// The model chip shows the selected configured row or explicit model id.
fn model_label(d: &LaunchDraft) -> String {
    let base = d
        .model_row
        .clone()
        .or_else(|| non_empty(&d.model))
        .unwrap_or_else(|| "default".to_string());
    let label = if d.effort.is_empty() {
        base
    } else {
        format!("{base} \u{b7} {}", d.effort)
    };
    compact_chip_value(&label, 28)
}

fn compact_chip_value(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut preview: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        preview.pop();
        preview.push('\u{2026}');
    }
    preview
}

fn paint_extra_flags_cursor(buf: &mut RtBuffer, r: RtRect, flags: &str, cursor_chars: usize) {
    if r.width == 0 {
        return;
    }
    let char_count = flags.chars().count();
    let visible_count = if char_count > 28 { 27 } else { char_count };
    let cursor = cursor_chars.min(visible_count);
    let prefix = if flags.is_empty() { "flags" } else { "flags " };
    let prefix_width = crate::chrome::str_cols(prefix);
    let cursor_width: usize = flags
        .chars()
        .take(cursor)
        .map(|c| usize::from(UnicodeWidthChar::width(c).unwrap_or(0)))
        .sum();
    let col = prefix_width
        .saturating_add(cursor_width)
        .min(r.width.saturating_sub(1) as usize);
    buf[(r.x + col as u16, r.y)].set_char('\u{2502}');
}

impl Launcher {
    /// Every axis's current choice, one joined strip for the sheet header.
    /// The values are the STATE the tab bodies edit; the strip is read-only.
    pub(crate) fn values_strip(&self) -> String {
        let d = &self.draft;
        let harness = non_empty(&d.harness()).unwrap_or_else(|| "harness".to_string());
        let model = model_label(d);
        let project = match d.cwd().rsplit('/').find(|s| !s.is_empty()) {
            Some(base) => base.to_string(),
            None => "project".to_string(),
        };
        let permission = if !d.permission.is_empty() {
            d.permission.clone()
        } else if d.harness().is_empty() {
            "harness decides".to_string()
        } else {
            format!("{} decides", d.harness())
        };
        let mut parts = vec![
            harness,
            model,
            project,
            permission,
            d.placement.label().to_string(),
        ];
        if !d.extra_flags.is_empty() {
            parts.push(compact_chip_value(&d.extra_flags, 28));
        }
        parts.join(" \u{b7} ")
    }

    /// The lifecycle line: refusal reasons, unknown-evidence, seed doubt,
    /// or blank in Editing (the hint row above carries the key rule).
    pub(crate) fn footer(&self) -> String {
        match &self.phase {
            Phase::Editing => String::new(),
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

/// The sheet's geometry: the framed sheet's on-screen `origin`, and body
/// rects in BODY coordinates (0,0 at the body's top-left). Paint maps the
/// body to `origin + 1` (past the chrome border); mouse adds the same
/// offset, so paint and hit-test can never disagree.
pub(crate) struct SheetLayout {
    pub origin: (u16, u16),
    pub framed_w: usize,
    pub framed_h: usize,
    /// The tab bar rects in body coordinates, in paint order.
    pub tabs: Vec<(Focus, RtRect)>,
    /// The body: the active tab's list or editor window in body coords.
    pub body: RtRect,
    /// One rect per VISIBLE list row: (index into the filtered row list,
    /// full-width rect in body coordinates). Empty on the editor tabs.
    pub row_rects: Vec<(usize, RtRect)>,
    /// The editor window when the Message tab is active.
    pub message: RtRect,
    pub start_chunk: usize,
    pub editor_rows: usize,
    /// The [cancel] footer rect while an attempt is pending.
    pub cancel: Option<RtRect>,
    /// The selected list row's full-width rect, when a list is shown.
    pub selected: Option<RtRect>,
}

impl Launcher {
    /// The centered sheet's layout, `None` when the terminal cannot admit
    /// the sheet's minimum (12 rows): the caller refuses with a notice, the
    /// same refusal a too-narrow sidebar used to give.
    pub(crate) fn sheet_layout(&self, view: &View) -> Option<SheetLayout> {
        let (rows, cols) = (view.term.0 as usize, view.term.1 as usize);
        if rows < 12 {
            return None;
        }
        let framed_w = (cols.saturating_sub(8)).min(96).max(3);
        let inner_w = framed_w.saturating_sub(2).max(1);
        // The tab bar: greedy wrap of the axis tabs across `inner_w`. Every
        // tab cell is its label plus two padding columns, one gap between.
        let order = Focus::tab_order(self, &view.launcher_catalog);
        let mut tab_rows_n = 1usize;
        let mut row_w = 0usize;
        for f in &order {
            let w = label_width(f.tab_title()) as usize + 2;
            if row_w + w > inner_w && row_w > 0 {
                tab_rows_n += 1;
                row_w = 0;
            }
            row_w += w + 1;
        }
        // Fixed rows: values strip, tab bar, keybar, lifecycle line.
        let other = 2 + tab_rows_n + 2;
        let body_cap = rows.saturating_sub(2 + other).max(4);
        let body_h = 10.min(body_cap).max(4);
        let framed_h = 2 + other + body_h;
        let origin = (
            ((rows.saturating_sub(framed_h)) / 2) as u16,
            ((cols.saturating_sub(framed_w)) / 2) as u16,
        );
        // Tab rects, per wrapped row, in body coordinates.
        let mut tabs: Vec<(Focus, RtRect)> = Vec::new();
        let mut ry = 1usize; // row 0 is the values strip
        let mut x = 0usize;
        for f in &order {
            let w = label_width(f.tab_title()) as usize + 2;
            if x + w > inner_w && x > 0 {
                ry += 1;
                x = 0;
            }
            tabs.push((*f, RtRect::new(x as u16, ry as u16, w as u16, 1)));
            x += w + 1;
        }
        let body_y = 1 + tab_rows_n;
        let body = RtRect::new(0, body_y as u16, inner_w as u16, body_h as u16);
        // List tabs: the visible window follows the selection; editor tabs
        // get the message-editor window follow instead.
        let (row_rects, message, start_chunk, editor_rows, selected) = if self.focus
            == Focus::Message
        {
            let wrap_w = inner_w.saturating_sub(PROMPT_GUTTER);
            let chunks = wrap_message(&self.draft.message, wrap_w);
            let (cur_row, _) = wrapped_cursor(&self.draft.message, self.draft.cursor_chars, wrap_w);
            let editor_rows = body_h;
            let mut start = chunks.len().saturating_sub(editor_rows);
            if cur_row < start {
                start = cur_row;
            } else if cur_row >= start + editor_rows {
                start = cur_row + 1 - editor_rows;
            }
            (
                Vec::new(),
                RtRect::new(0, body_y as u16, inner_w as u16, editor_rows as u16),
                start,
                editor_rows,
                None,
            )
        } else if self.focus.is_list() {
            let (rows_list, _) = tab_body_rows(self, &view.launcher_catalog, &view.backlog);
            let n = rows_list.len();
            let sel_eff = effective_sel(&rows_list, self.sel);
            let start = sel_eff.min(n.saturating_sub(1)).saturating_sub(body_h / 2);
            let mut selected = None;
            let row_rects = (0..body_h)
                .filter_map(|k| {
                    let idx = start + k;
                    rows_list.get(idx).map(|_| {
                        let r = RtRect::new(0, (body_y + k) as u16, inner_w as u16, 1);
                        if idx == sel_eff {
                            selected = Some(r);
                        }
                        (idx, r)
                    })
                })
                .collect();
            (row_rects, RtRect::new(0, 0, 0, 0), 0, 0, selected)
        } else {
            // The Flags tab: a one-line editor painted in the body.
            (Vec::new(), RtRect::new(0, 0, 0, 0), 0, 0, None)
        };
        // The [cancel] item rides the keybar row while an attempt is pending.
        let pending = matches!(self.phase, Phase::Unknown { .. } | Phase::Submitting { .. });
        let cancel = pending.then(|| RtRect::new(0, (body_y + body_h) as u16, 8, 1));
        Some(SheetLayout {
            origin,
            framed_w,
            framed_h,
            tabs,
            body,
            row_rects,
            message,
            start_chunk,
            editor_rows,
            cancel,
            selected,
        })
    }

    /// Paint the sheet: the chrome frame first, then the body Buffer
    /// (values strip, tab bar, the active tab's body, keybar, lifecycle)
    /// blitted over the frame's empty body rows via [`blit_area`].
    pub(crate) fn paint_sheet(
        &self,
        view: &View,
        cells: &mut [crate::proto::Cell],
        rows: usize,
        cols: usize,
        sl: &SheetLayout,
    ) {
        let inner_w = sl.framed_w.saturating_sub(2).max(1);
        let body_h = sl.framed_h.saturating_sub(2);
        let chrome = crate::chrome::Chrome::new("new agent", crate::popup::Anchor::Center);
        let body: Vec<crate::chrome::BodyLine> = (0..body_h)
            .map(|_| crate::chrome::BodyLine::plain(""))
            .collect();
        let framed = crate::chrome::frame(&body, &chrome, inner_w, None);
        crate::chrome::blit(
            cells,
            rows,
            cols,
            (sl.origin.0 as usize, sl.origin.1 as usize),
            &framed,
            &view.theme,
        );
        let (oy, ox) = (sl.origin.0 as usize + 1, sl.origin.1 as usize + 1);
        let mut buf = RtBuffer::empty(RtRect::new(0, 0, inner_w as u16, body_h as u16));
        // The values strip: every axis's current choice, read-only, dim.
        buf.set_string(
            0,
            0,
            compact_chip_value(&self.values_strip(), inner_w),
            role_style(Role::BodyDim, &view.theme),
        );
        // The tab bar; the active tab wears the selected block.
        for (f, r) in &sl.tabs {
            let role = if *f == self.focus {
                Role::BodySel
            } else {
                Role::Body
            };
            paint_chip(
                &mut buf,
                *r,
                f.tab_title(),
                role_style(role, &view.theme),
                false,
            );
        }
        // The body: the active tab.
        if self.focus == Focus::Message {
            // The editor: prompt gutter, wrapped rows, cursor mark, and on
            // an empty draft the dim placeholder naming the shape.
            let wrap_w = inner_w.saturating_sub(PROMPT_GUTTER);
            let chunks = wrap_message(&self.draft.message, wrap_w);
            let (cur_row, cur_col) =
                wrapped_cursor(&self.draft.message, self.draft.cursor_chars, wrap_w);
            for k in 0..sl.editor_rows {
                let Some((_, text)) = chunks.get(sl.start_chunk + k) else {
                    break;
                };
                let y = sl.message.y + k as u16;
                buf.set_string(sl.message.x + PROMPT_GUTTER as u16, y, text, RtStyle::new());
                if cur_row == sl.start_chunk + k {
                    let disp_col: usize = text
                        .chars()
                        .take(cur_col)
                        .map(|c| usize::from(UnicodeWidthChar::width(c).unwrap_or(0)))
                        .sum();
                    if (disp_col as u16) + (PROMPT_GUTTER as u16) < sl.message.width {
                        buf[(sl.message.x + disp_col as u16 + PROMPT_GUTTER as u16, y)]
                            .set_char('\u{258f}');
                    }
                }
            }
            if sl.editor_rows > 0 {
                buf.set_string(
                    sl.message.x,
                    sl.message.y,
                    "\u{276f} ",
                    role_style(Role::BodyDim, &view.theme),
                );
                if self.draft.message.is_empty() {
                    buf.set_string(
                        sl.message.x + PROMPT_GUTTER as u16,
                        sl.message.y,
                        "/fno:target <node> or a task",
                        role_style(Role::BodyDim, &view.theme),
                    );
                }
            }
        } else if self.focus == Focus::ExtraFlags {
            // The flags editor: the field with its cursor, and the parse
            // state under it - argv count, or the refusal verbatim.
            let r = RtRect::new(0, sl.body.y, inner_w as u16, 1);
            buf.set_string(
                0,
                sl.body.y,
                "flags ",
                role_style(Role::BodyDim, &view.theme),
            );
            buf.set_string(
                6,
                sl.body.y,
                compact_chip_value(&self.draft.extra_flags, inner_w.saturating_sub(6)),
                RtStyle::new(),
            );
            paint_extra_flags_cursor(
                &mut buf,
                r,
                &self.draft.extra_flags,
                self.draft.extra_flags_cursor_chars,
            );
            let state = match parse_extra_flags(&self.draft.extra_flags) {
                Ok(argv) => {
                    let n = argv.len();
                    format!("{n} argv")
                }
                Err(e) => e,
            };
            buf.set_string(
                0,
                sl.body.y + 1,
                compact_chip_value(&state, inner_w),
                role_style(Role::BodyDim, &view.theme),
            );
        } else {
            // A list tab: full-width rows, the selection painted across the
            // WHOLE inner width. The label is never ellipsized to make room
            // for the hint - the hint truncates instead, so a refusal's
            // label ("model list unavailable") always reads whole.
            let (list, _) = tab_body_rows(self, &view.launcher_catalog, &view.backlog);
            for (idx, r) in &sl.row_rects {
                let Some(row) = list.get(*idx) else {
                    continue;
                };
                let selected = sl.selected == Some(*r);
                match row {
                    PopupRow::Header(text) => {
                        buf.set_string(r.x, r.y, text, role_style(Role::BodyDim, &view.theme));
                    }
                    PopupRow::Entry {
                        glyph,
                        label,
                        hint,
                        enabled,
                    } => {
                        let style = if selected {
                            role_style(Role::BodySel, &view.theme)
                        } else if !enabled {
                            role_style(Role::BodyDim, &view.theme)
                        } else {
                            RtStyle::new()
                        };
                        buf.set_style(*r, style);
                        let mut text = String::new();
                        if !glyph.is_empty() {
                            text.push_str(glyph);
                            text.push(' ');
                        }
                        text.push_str(label);
                        buf.set_string(r.x, r.y, &text, style);
                        if !hint.is_empty() {
                            let used = label_width(&text) as usize;
                            let room = (r.width as usize).saturating_sub(used + 1);
                            if room > 2 {
                                let hint_text = compact_chip_value(hint, room);
                                let hw = label_width(&hint_text) as usize;
                                buf.set_string(
                                    r.x + (r.width as usize - hw) as u16,
                                    r.y,
                                    &hint_text,
                                    role_style(Role::BodyDim, &view.theme),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Keybar row: [cancel] while pending, then the key rule; the
        // lifecycle line under it, dim.
        let keybar_y = sl.body.y + sl.body.height;
        if let Some(cr) = sl.cancel {
            paint_chip(
                &mut buf,
                cr,
                "[cancel]",
                role_style(Role::Body, &view.theme),
                false,
            );
        }
        buf.set_string(
            0,
            keybar_y.min((body_h as u16).saturating_sub(1)),
            compact_chip_value(&self.keybar(), inner_w),
            RtStyle::new(),
        );
        buf.set_string(
            0,
            (keybar_y + 1).min((body_h as u16).saturating_sub(1)),
            self.footer(),
            RtStyle::new().add_modifier(Modifier::DIM),
        );
        crate::ratatui_blit::blit_area(&buf, oy, ox, cells, cols);
    }
}

/// The footer key rule, per state: the whole grammar, one line, never
/// replaced by the lifecycle line under it.
impl Launcher {
    fn keybar(&self) -> String {
        if matches!(self.phase, Phase::Unknown { .. } | Phase::Submitting { .. }) {
            return "esc cancel".to_string();
        }
        match self.focus {
            Focus::Message => {
                "\u{21b5} launch \u{b7} ^j newline \u{b7} type message \u{b7} esc close".to_string()
            }
            Focus::ExtraFlags => {
                "type flags \u{b7} \u{2190}\u{2192} tab \u{b7} esc close".to_string()
            }
            _ => "\u{2191}\u{2193} choose \u{b7} \u{21b5} pick \u{b7} \u{2190}\u{2192} tab \u{b7} ^j launch \u{b7} esc close".to_string(),
        }
    }
}

/// A launch is offered while the sheet edits: a pending Submitting or an
/// Unknown attempt must be cancelled explicitly (Esc) before a new one arms.
fn launcher_can_launch(view: &View) -> bool {
    view.launcher
        .as_ref()
        .is_some_and(|l| !matches!(l.phase, Phase::Submitting { .. } | Phase::Unknown { .. }))
}

/// The next selectable index stepping `delta` from `sel`: header rows are
/// captions, never targets, so they are stepped over; the ends stop the
/// walk (no wrap - the list's ends are visible in the body).
fn step_selection(rows: &[PopupRow], sel: usize, delta: i32) -> usize {
    let last = rows.len().saturating_sub(1);
    let mut idx = sel.min(last) as i64;
    loop {
        idx += delta as i64;
        if idx < 0 {
            return 0;
        }
        if idx as usize > last {
            return last;
        }
        if !matches!(rows.get(idx as usize), Some(PopupRow::Header(_))) {
            return idx as usize;
        }
    }
}

/// The effective selection: a body never RESTS on a header caption. If the
/// stored index points at one (the common case: a fresh body at 0), the
/// first entry after it is the selection for paint, layout and commit.
fn effective_sel(rows: &[PopupRow], sel: usize) -> usize {
    if matches!(rows.get(sel), Some(PopupRow::Header(_))) {
        step_selection(rows, sel, 1)
    } else {
        sel
    }
}

/// The active tab body's rows with their commit actions: the axis's full
/// row set from [`picker_rows`], narrowed by the live filter. Derived at
/// layout and paint time, so a catalog landing re-renders the body with no
/// refresh step.
pub(crate) fn tab_body_rows(
    l: &Launcher,
    catalog: &Option<CatalogOutcome>,
    backlog: &[crate::proto::BacklogCard],
) -> (Vec<PopupRow>, Vec<Option<PickerAction>>) {
    let (all_rows, all_actions) = picker_rows(l, catalog, backlog);
    let q = l.filter.to_lowercase();
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    let mut last_header: Option<(PopupRow, Option<PickerAction>)> = None;
    for (row, action) in all_rows.into_iter().zip(all_actions.into_iter()) {
        match &row {
            PopupRow::Header(_) => last_header = Some((row, action)),
            PopupRow::Entry { label, .. } => {
                if !q.is_empty() && !label.to_lowercase().contains(&q) {
                    continue;
                }
                if let Some((hr, ha)) = last_header.take() {
                    rows.push(hr);
                    actions.push(ha);
                }
                rows.push(row);
                actions.push(action);
            }
            other => {
                rows.push(other.clone());
                actions.push(action);
            }
        }
    }
    (rows, actions)
}

/// The one draw arm the client's overlay chain calls: the sheet, then the
/// `@` node picker (the one remaining popover), in that order.
pub(crate) fn draw_overlay(
    view: &View,
    cells: &mut [crate::proto::Cell],
    rows: usize,
    cols: usize,
) -> bool {
    let Some(l) = view.launcher.as_ref() else {
        return false;
    };
    let mut drew = false;
    if let Some(sl) = l.sheet_layout(view) {
        l.paint_sheet(view, cells, rows, cols, &sl);
        drew = true;
    }
    if let Some(pk) = l.picker.as_ref() {
        crate::popup::draw(cells, rows, cols, &pk.popup.render(view.term), &view.theme);
        drew = true;
    }
    drew
}

/// One chip label's terminal column width.
fn label_width(label: &str) -> u16 {
    label
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1).max(1) as u16)
        .sum()
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

/// Mouse: the open list is swallowed FIRST, whatever the report kind - a
/// wheel or drag over it must never fall through to the pane under the
/// popup. In sheet mode the sheet owns every report; a left press inside it
/// focuses the chip it hit (Launch submits). In bottom mode a press inside
/// the dock focuses the chip it hit; anything outside the form and the list
/// still falls through, as today.
pub(crate) async fn launcher_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<bool, String> {
    use crate::proto::{MouseButton, MouseKind};
    // The open picker: a report OVER it is consumed whatever its kind (a
    // wheel there used to reach the pane under the popup); a press outside
    // dismisses the picker; any other report outside falls through.
    if view.launcher.as_ref().is_some_and(|l| l.picker.is_some()) {
        let over = view
            .launcher
            .as_ref()
            .and_then(|l| {
                let pk = l.picker.as_ref()?;
                Some(pk.popup.render(view.term).contains(rep.row, rep.col))
            })
            .unwrap_or(false);
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
        if matches!(rep.kind, MouseKind::Move) {
            if over {
                if let (Some(target), Some(l)) = (
                    hit.filter(|target| *target != crate::chrome::ESC_CLOSE_HIT),
                    view.launcher.as_mut(),
                ) {
                    if let Some(picker) = l.picker.as_mut() {
                        picker.popup.select(target);
                    }
                }
                return Ok(true);
            }
            return Ok(false);
        }
        if !matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
            return Ok(over);
        }
        let portal = next_free_portal(view);
        // Field-disjoint snapshot for the commit path.
        let catalog = view.launcher_catalog.clone();
        if let Some(l) = view.launcher.as_mut() {
            let Some(mut picker) = l.picker.take() else {
                unreachable!("checked Some above");
            };
            if !over {
                // A press outside: dismiss the picker, consume the click.
                return Ok(true);
            }
            match hit {
                Some(target) => {
                    picker.popup.select(target);
                    let action = picker
                        .popup
                        .targets()
                        .get(target)
                        .and_then(|(row, _)| picker.actions.get(*row))
                        .cloned()
                        .flatten();
                    l.picker = Some(picker);
                    if let Some(action) = action {
                        apply_picker_action(l, &catalog, action, portal);
                    }
                }
                _ => {
                    // Inside the block, off a target: swallowed, stays open.
                    l.picker = Some(picker);
                }
            }
        }
        return Ok(true);
    }
    // The sheet owns every report, the way the settings modal does: the
    // axis lists live IN the sheet now, so a wheel or a motion over it is
    // consumed, never forwarded to a pane. A left press switches to the
    // tab it hit, selects (a second press commits) a body row, or presses
    // [cancel].
    let Some(l) = view.launcher.as_ref() else {
        return Ok(true);
    };
    let Some(sl) = l.sheet_layout(view) else {
        // The sheet cannot fit (under 12 rows): consumed, nothing painted.
        return Ok(true);
    };
    let (oy, ox) = (sl.origin.0 as usize + 1, sl.origin.1 as usize + 1);
    let row = rep.row as usize;
    let col = rep.col as usize;
    let hit = |r: RtRect| {
        col >= ox + r.x as usize
            && col < ox + (r.x + r.width) as usize
            && row >= oy + r.y as usize
            && row < oy + (r.y + r.height) as usize
    };
    let hit_row = sl
        .row_rects
        .iter()
        .find(|(_, r)| hit(*r))
        .map(|(idx, _)| *idx);
    let hit_tab = sl.tabs.iter().find(|(_, r)| hit(*r)).map(|(f, _)| *f);
    let hit_cancel = sl.cancel.is_some_and(|r| hit(r));
    if let MouseKind::Move = rep.kind {
        // Motion over a body row selects it and never closes the sheet;
        // motion anywhere else over the sheet is consumed silently.
        if let Some(idx) = hit_row {
            if let Some(l) = view.launcher.as_mut() {
                l.sel = idx;
            }
        }
        return Ok(true);
    }
    if !matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
        return Ok(true);
    }
    let portal = next_free_portal(view);
    let catalog = view.launcher_catalog.clone();
    if let Some(tab) = hit_tab {
        if let Some(l) = view.launcher.as_mut() {
            if l.focus != tab {
                l.sel = 0;
                l.filter.clear();
            }
            l.focus = tab;
        }
        return Ok(true);
    }
    if hit_cancel {
        if let Some(l) = view.launcher.as_mut() {
            l.phase = Phase::Editing;
            l.armed = None;
        }
        return Ok(true);
    }
    let Some(idx) = hit_row else {
        return Ok(true);
    };
    if let Some(l) = view.launcher.as_mut() {
        if l.sel != idx {
            // First press on a row: select it. A press on the ALREADY
            // selected row commits it, matching Enter.
            l.sel = idx;
            return Ok(true);
        }
    }
    let action = view.launcher.as_ref().and_then(|l| {
        let (_, actions) = tab_body_rows(l, &view.launcher_catalog, &view.backlog);
        actions.get(idx).cloned().flatten()
    });
    if let Some(action) = action {
        if let Some(l) = view.launcher.as_mut() {
            apply_picker_action(l, &catalog, action, portal);
        }
    }
    Ok(true)
}
