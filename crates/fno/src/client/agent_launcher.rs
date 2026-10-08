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

mod bang;

/// Ceiling on an open bracketed paste's carried bytes. The submit gate
/// refuses an over-cap message anyway; this only stops a close-marker-less
/// paste from growing the carry forever.
const MAX_PASTE_CARRY: usize = 16 * 1024;
const MAX_LAUNCH_FLAGS_CHARS: usize = 1024;
pub(crate) const LAUNCH_EXTRA_AXES_PROTO: u32 = 91;
pub(crate) const LAUNCH_WORKTREE_PROTO: u32 = 95;

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
    /// Launchable catalog rows beyond the main list (searchable by group 2's
    /// more row). Never rendered in the main body.
    pub more: Vec<ModelChoice>,
    /// The models.dev cache failure, when the catalog could not be read.
    /// Never degrades the harness rows.
    pub catalog_error: Option<String>,
    /// A harness-specific model catalog failure, such as OpenCode's own list.
    pub models_error: Option<String>,
    /// See harness_capabilities.toml `efforts`: `None` = no surface at all,
    /// `Some([])` = the axis exists with provider passthrough (free text),
    /// a filled list = the enumerable choices.
    pub efforts: Option<Vec<String>>,
    /// Same three states as `efforts`, for the permission axis.
    pub permission_modes: Option<Vec<String>>,
    /// The composer's `--` flag picker list: `None` = no capture (a harness
    /// not installed on the capturing machine); a list of `--flag <value>`
    /// spellings.
    pub launch_flags: Option<Vec<String>>,
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

pub(crate) use crate::model_catalog::{
    codex_models_cache_path, merge_model_choices, parse_codex_models,
    parse_configured_account_models, parse_opencode_models, ModelChoice, ModelState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentModelChoice {
    pub harness: String,
    pub choice: ModelChoice,
}

/// The catalog read's outcome (the update-probe shape): the dock opens
/// instantly on whatever is in hand and refreshes when the probe lands. The
/// second field of `Ok` carries the account-record read's failure when model
/// lists could not be fetched; harness rows still stand. The third carries
/// one [`ProjectFacts`] row per probed project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogOutcome {
    Ok(Vec<HarnessChoice>, Option<String>, Vec<ProjectFacts>),
    Degraded(String),
}

/// One project's launch facts, read during the catalog probe: the checkout's
/// current branch, its local branches (committer-date freshest first), and
/// the resolved worktree policy word. A non-git cwd carries no branch facts;
/// a failed policy read lands the named reason in `policy` so the launch
/// stays honest (`worktree ?`) instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectFacts {
    pub cwd: String,
    pub current: Option<String>,
    pub branches: Vec<String>,
    pub policy: Result<String, String>,
}

/// Which control owns the keyboard. The chips carry the axes; a chip's
/// picker renders the axis's full choice list. `Message` is the editor.
/// `Plus` paints no chip: it is only the flags picker's field, opened by
/// typing `--` at a word start in the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Where,
    Project,
    Branch,
    Worktree,
    Message,
    Plus,
    Permission,
    Harness,
    Model,
    Effort,
}

impl Focus {
    /// The chip's spoken name, for the keybar's Enter hint: the hint names
    /// the chip Enter opens instead of a generic "open/launch".
    fn name(self) -> &'static str {
        match self {
            Self::Where => "where",
            Self::Project => "project",
            Self::Branch => "branch",
            Self::Worktree => "worktree",
            Self::Message => "message",
            Self::Plus => "flags",
            Self::Permission => "permission",
            Self::Harness => "harness",
            Self::Model => "model",
            Self::Effort => "effort",
        }
    }

    /// The chip-row cycle, in paint order. The Branch chip and worktree box
    /// join only when the draft project's facts admit branching (a non-git
    /// project has neither); the effort chip joins only when the catalog
    /// says the harness has an effort surface.
    fn tab_order(launcher: &Launcher, catalog: &Option<CatalogOutcome>) -> Vec<Focus> {
        let mut order = vec![Self::Where, Self::Project];
        if branch_offered(launcher, catalog) {
            order.extend([Self::Branch, Self::Worktree]);
        }
        order.extend([Self::Message, Self::Permission, Self::Harness, Self::Model]);
        if effort_offered(launcher, catalog) {
            order.push(Self::Effort);
        }
        order
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

/// The effort chip paints only when the harness's catalog row declares an
/// effort surface at all (`None` = no axis).
pub(crate) fn effort_offered(launcher: &Launcher, catalog: &Option<CatalogOutcome>) -> bool {
    let harness = launcher.draft.harness();
    let Some(CatalogOutcome::Ok(rows, _, _)) = catalog else {
        return false;
    };
    rows.iter()
        .find(|r| r.name == harness)
        .is_some_and(|r| r.efforts.is_some())
}

/// The Branch chip and worktree box paint when the draft project can take a
/// branch at all. Facts unread (`None`) still offers: the box paints
/// `worktree ?` until the probe lands (AC6-EDGE). Only a read non-git
/// project (no current branch, no branches) hides both.
pub(crate) fn branch_offered(launcher: &Launcher, catalog: &Option<CatalogOutcome>) -> bool {
    match launcher.draft.facts(catalog) {
        None => true,
        Some(f) => f.current.is_some() || !f.branches.is_empty(),
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
    /// Shell mode: a leading `!` on an empty input turned the composer
    /// into a one-line shell prompt (`bang`); Backspace on an empty line
    /// leaves it again. Not a draft field: shell mode never rides a
    /// retained draft across an Esc.
    pub shell: bool,
    /// A refused or unknown footer shows the full raw reason instead of its
    /// one-sentence head. Toggled with Ctrl+O; a fresh attempt clears it.
    pub show_detail: bool,
    /// The selected harness's `--help` flag rows (entry spelling + one-line
    /// description), captured once per binary version and cached on disk.
    /// Empty or harness-mismatched means the toml capture shows instead.
    pub runtime_flags: Vec<crate::client::harness_flags::FlagRow>,
    /// The harness `runtime_flags` was captured for.
    pub runtime_flags_harness: String,
    /// The harness-flags capture is in flight (the same one-in-flight
    /// discipline as the catalog probe). Dies with the launcher: a capture
    /// landing after a close is dropped, and the next open probes fresh.
    pub flags_inflight: bool,
    /// The mouse rests on the Project chip: the cwd facts line shows.
    pub project_hover: bool,
    /// A chip-owned pill (`--model`) awaiting its value: the flag rides the
    /// launcher until value capture finalizes onto the chip.
    pending_chip_pin: Option<String>,

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
    /// A branch pick: the worktree launch checks this branch out. A pick
    /// other than the project's current branch checks the worktree box.
    SetBranch(String),
    /// The `--` flags picker: commit a typed launch flag as a pill (a
    /// chip-owned flag pins its chip instead).
    AddPill {
        flag: String,
        picks_value: bool,
    },
    /// The fno section's `--force` row: arms the per-request force override
    /// for the next launch. Shows as a `--force` pill; the flag itself
    /// never rides extra argv (the request field carries it, journaled).
    Force,
    /// The "<harness> decides" first row of the mode or effort picker:
    /// clear the pin.
    Clear,
    ClearModel,
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
    /// The Model list's `more` row: rebuild the picker over the harness's
    /// full catalog tail, Esc stepping back to the main list.
    OpenMore,
    /// A NoKey or Unreachable more-row: never picks. Rebuilds the picker as
    /// the connect steps (NoKey) or the protocol gap (Unreachable); Esc
    /// steps back to the more list.
    ShowSteps {
        title: String,
        lines: Vec<String>,
    },
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
    /// Which Model row set is showing; Esc walks the ladder back.
    pub mode: PickerMode,
}

/// Which Model row set an open picker shows: the main list, the harness's
/// `more` catalog tail, the connect-steps sheet a more row opened, or the
/// composer help sheet `?` opens. Esc steps back down: Help -> close,
/// Steps -> More -> Main -> close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerMode {
    Main,
    More,
    Steps { title: String },
    Help,
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
    /// The worktree box: `None` = follow the project's resolved policy,
    /// `Some(v)` = an explicit pick (Space/click or a branch pick). A `never`
    /// policy overrides both.
    pub worktree: Option<bool>,
    /// The branch a worktree launch checks out; `None` = ensure's default
    /// (`main`, a fresh branch off origin/main).
    pub branch: Option<String>,
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
    /// The picked row's `vendor/model` route, riding alone at submit: a
    /// route owns the model, so the request never pairs it with `model`.
    pub route: String,
    /// The provider of the selected configured route, when present.
    pub provider: String,
    pub effort: String,
    pub permission: String,
    pub placement: Placement,
    /// The portal index a thread placement opens through, resolved from the
    /// live layout when the placement is picked (next free index).
    pub placement_portal: u8,
    /// Typed launch flags as pills: each is one flag plus its value, joined
    /// into `extra_flags` argv in order at submit.
    pub pills: Vec<(String, Option<String>)>,
    /// When true, the next Space-delimited word the user types lands as the
    /// last pill's value instead of message text.
    pub pill_value_capture: bool,
    /// The value text typed while [`Self::pill_value_capture`] holds, painted
    /// inside the capturing pill.
    pub pill_value_draft: String,
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

    /// The draft cwd's facts row, when the catalog probe landed one.
    fn facts<'a>(&self, catalog: &'a Option<CatalogOutcome>) -> Option<&'a ProjectFacts> {
        match catalog {
            Some(CatalogOutcome::Ok(_, _, facts)) => facts.iter().find(|f| f.cwd == self.cwd()),
            _ => None,
        }
    }

    /// The box's effective state: `Some(v)` when the launch would take a
    /// worktree, `None` when the policy is unread or failed AND no explicit
    /// pick stands (the `worktree ?` state). A `never` policy always answers
    /// `Some(false)`; a read policy answers the checked default.
    fn worktree_state(&self, catalog: &Option<CatalogOutcome>) -> Option<bool> {
        if self.policy_never(catalog) {
            return Some(false);
        }
        self.worktree
            .or_else(|| self.policy_known(catalog).then_some(true))
    }

    fn policy_never(&self, catalog: &Option<CatalogOutcome>) -> bool {
        matches!(
            self.facts(catalog).map(|f| f.policy.as_deref()),
            Some(Ok("never"))
        )
    }

    fn policy_known(&self, catalog: &Option<CatalogOutcome>) -> bool {
        self.facts(catalog).is_some_and(|f| f.policy.is_ok())
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
        // A routing-row pick rides its ROUTE pin: a route owns the model, so
        // model and provider never join it. codex and opencode are the
        // carve-outs: their rows are slugs/ids their own CLI consumes, so the
        // id rides as --model (a derived openai/... route is claude-only at
        // the door).
        let native_model_pin = matches!(self.harness().as_str(), "codex" | "opencode");
        AgentLaunchRequest {
            request_id,
            revision: self.revision,
            cwd: self.cwd(),
            harness: self.harness(),
            substrate: substrate.to_string(),
            model: if !self.route.is_empty() && !native_model_pin {
                None
            } else {
                non_empty(&self.model)
            },
            provider: None,
            route: if native_model_pin {
                None
            } else {
                non_empty(&self.route)
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
            // The worktree choice lands in `submit`, where the project facts
            // (and the `worktree ?` refusal) are reachable; a bare request
            // keeps the in-place default.
            worktree: false,
            branch: None,
            // Force is armed in `submit` too; a bare request never forces.
            force: false,
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

/// The flags a chip or the launch door owns: they never become pills, they
/// pin their chip instead (`--model`), or the launch refuses them exactly
/// as today (`--cwd`).
fn pill_is_chip_owned(flag: &str) -> bool {
    matches!(flag, "--model" | "--effort" | "--harness")
}

/// Commit a `--word` the user typed in the message: remove the word, add
/// the pill, and open value capture so the next word lands as its value.
/// A chip-owned flag never stores a pill; its value pins the chip.
fn commit_verbatim_pill(l: &mut Launcher) {
    let Some(word) = trailing_word(&l.draft.message).map(str::to_string) else {
        return;
    };
    if !word.starts_with("--") || word == "--" {
        return;
    }
    cut_trailing_word(l);
    add_pill(l, word, true);
}

/// Remove the typed `--word` from the message (the picker-pick commit
/// path); unlike the verbatim Space commit it never commits on its own.
fn commit_typed_flag_word(l: &mut Launcher) {
    let has_flag_word = trailing_word(&l.draft.message)
        .map(|w| w.starts_with("--"))
        .unwrap_or(false);
    if has_flag_word {
        cut_trailing_word(l);
    }
}

/// Drop the message's trailing `--word` and park the cursor there.
fn cut_trailing_word(l: &mut Launcher) {
    let Some(word) = trailing_word(&l.draft.message) else {
        return;
    };
    let len = word.chars().count();
    let cut = l.draft.message.chars().count() - len;
    l.draft.message = l.draft.message.chars().take(cut).collect();
    l.draft.cursor_chars = cut;
}

/// Add a pill (or pin its chip). `capture` opens value capture.
fn add_pill(l: &mut Launcher, flag: String, capture: bool) {
    if pill_is_chip_owned(&flag) {
        // A chip-owned flag pins its chip: no pill stores, and the flag
        // name rides the launcher until the value lands on it.
        l.pending_chip_pin = Some(flag);
        l.draft.pill_value_capture = capture;
        l.draft.pill_value_draft.clear();
        l.draft.bump();
        return;
    }
    l.draft.pills.push((flag, None));
    l.draft.pill_value_capture = capture;
    l.draft.pill_value_draft.clear();
    l.draft.bump();
}

/// Finalize value capture: the captured word lands as the last pill's
/// value, or pins a chip-owned flag's chip. An empty capture leaves the
/// pill valueless.
fn finalize_pill_value(l: &mut Launcher) {
    let word = std::mem::take(&mut l.draft.pill_value_draft);
    let capturing = std::mem::take(&mut l.draft.pill_value_capture);
    if !capturing {
        return;
    }
    if let Some(pin) = l.pending_chip_pin.take() {
        if !word.is_empty() {
            apply_chip_pin(l, &pin, &word);
        }
        return;
    }
    if !word.is_empty() {
        if let Some(last) = l.draft.pills.last_mut() {
            last.1 = Some(word);
            l.draft.bump();
        }
    }
}

/// Apply a chip pin: the value the user typed lands on the chip.
fn apply_chip_pin(l: &mut Launcher, flag: &str, value: &str) {
    match flag {
        "--model" => {
            l.draft.model = value.to_string();
            l.draft.model_row = None;
            l.draft.route.clear();
        }
        "--effort" => l.draft.effort = value.to_string(),
        "--harness" => {
            if let Some(idx) = l.draft.harnesses.iter().position(|h| h == value) {
                l.draft.harness_idx = idx;
            }
        }
        _ => {}
    }
    l.draft.bump();
}

/// The message's trailing whitespace-delimited word, for the `--` gestures.
fn trailing_word(message: &str) -> Option<&str> {
    message
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .filter(|w| !w.is_empty())
}

/// The pills flattened to argv: one element per flag, one per value, in
/// pill order.
fn pill_argv(pills: &[(String, Option<String>)]) -> Result<Vec<String>, String> {
    let mut flags = Vec::new();
    for (flag, value) in pills {
        // The force pill is visual only: the request's force field carries
        // the override (journaled server-side), never a bare door flag.
        if flag == "--force" {
            continue;
        }
        flags.push(flag.clone());
        if let Some(value) = value {
            flags.push(value.clone());
        }
    }
    Ok(flags)
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
            focus: Focus::Message,
            phase: Phase::Editing,
            armed: None,
            next_request_id: 1,
            shell: false,
            show_detail: false,
            runtime_flags: Vec::new(),
            runtime_flags_harness: String::new(),
            flags_inflight: false,
            project_hover: false,
            pending_chip_pin: None,
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
    // open retries any failed routing or harness-specific model list. A draft
    // project with no facts row re-probes as well, so a project that joins
    // the list after the last probe gets its facts on the next read.
    let facts_missing = view.launcher.as_ref().is_some_and(|l| {
        let cwd = l.draft.cwd();
        match &view.launcher_catalog {
            Some(CatalogOutcome::Ok(_, _, facts)) => !facts.iter().any(|f| f.cwd == cwd),
            _ => false,
        }
    });
    if facts_missing
        || !matches!(
            &view.launcher_catalog,
            Some(CatalogOutcome::Ok(rows, None, _)) if rows.iter().all(|row| row.models_error.is_none())
        )
    {
        view.catalog_want = true;
    }
}

/// Offer every catalog name as a harness choice; an unavailable one refuses
/// AT SUBMIT with its own reason (visible inline), never by disappearing.
/// The FIRST sync also picks the preselect: the last harness this session
/// launched (the retained draft carries it), else claude, else codex - the
/// alphabetical first row never wins.
pub(crate) fn sync_harness_names(l: &mut Launcher, catalog: &Option<CatalogOutcome>) {
    if l.draft.harnesses.is_empty() {
        if let Some(CatalogOutcome::Ok(rows, _, _)) = catalog {
            l.draft.harnesses = rows.iter().map(|r| r.name.clone()).collect();
            if l.draft.harness_idx == 0 {
                l.draft.harness_idx =
                    default_harness_idx(&l.draft.harnesses, remembered_harness().as_deref());
            }
        }
    }
}

/// The preselect ladder over the catalog's names: the harness the last
/// launch used (the mux-dir store), then claude, then codex, then the first
/// row (a catalog with none of those still shows a value).
fn default_harness_idx(names: &[String], remembered: Option<&str>) -> usize {
    if let Some(want) = remembered {
        if let Some(i) = names.iter().position(|n| n == want) {
            return i;
        }
    }
    for want in ["claude", "codex"] {
        if let Some(i) = names.iter().position(|n| n == want) {
            return i;
        }
    }
    0
}

/// The last harness a launch used: one name in the mux dir, so the
/// preselect survives the client session.
fn last_harness_path() -> std::path::PathBuf {
    crate::proto::mux_dir().join("composer-last-harness")
}

fn remember_harness(name: &str) {
    if let Some(parent) = last_harness_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(last_harness_path(), format!("{name}\n"));
}

fn remembered_harness() -> Option<String> {
    std::fs::read_to_string(last_harness_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    launcher.focus = Focus::Message;
    launcher.draft.bump();
    Ok(())
}

/// The project candidates a fresh draft offers: the session's own launch cwd
/// first, then the workspace squads' cwds, deduped, most-recent first.
fn candidate_projects(view: &View) -> Vec<String> {
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
    projects
}

/// The project list the catalog probe reads facts for: the open draft's
/// candidates, or the fresh-draft list when the dock is closed.
pub(crate) fn probe_projects(view: &View) -> Vec<String> {
    view.launcher
        .as_ref()
        .map(|l| l.draft.projects.clone())
        .unwrap_or_else(|| candidate_projects(view))
}

fn fresh_draft(view: &View) -> LaunchDraft {
    // Project candidates: the active workspace's squads, cwd-most-recent is
    // the session's own launch cwd. The exact cwd sent is the one shown.
    let projects = candidate_projects(view);
    LaunchDraft {
        harnesses: Vec::new(),
        harness_idx: 0,
        projects,
        project_idx: 0,
        worktree: None,
        branch: None,
        message: String::new(),
        cursor_chars: 0,
        node: None,
        model: String::new(),
        model_row: None,
        route: String::new(),
        provider: String::new(),
        effort: String::new(),
        permission: String::new(),
        placement: Placement::default(),
        placement_portal: 0,
        pills: Vec::new(),
        pill_value_capture: false,
        pill_value_draft: String::new(),
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
    // A launch that actually rode the override spent it: the force pill
    // clears on the terminal Launched state. A refusal or an Unknown keeps
    // it armed for the operator's retry.
    if let LaunchState::Launched { .. } = update.state {
        let mut spent = false;
        if let Some(l) = view.launcher.as_mut() {
            if let Some(at) = l.draft.pills.iter().position(|(f, _)| f == "--force") {
                l.draft.pills.remove(at);
                l.draft.bump();
                spent = true;
            }
        }
        if !spent {
            if let Some(l) = view.launcher_closed.as_mut() {
                if let Some(at) = l.draft.pills.iter().position(|(f, _)| f == "--force") {
                    l.draft.pills.remove(at);
                    l.draft.bump();
                }
            }
        }
    }
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
    // The pills join extra_flags in order, one argv element per flag and
    // one per value, through the same validate the wire door runs.
    let extra_flags = match pill_argv(&l.draft.pills)
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
        Some(CatalogOutcome::Ok(rows, _, _)) => match rows.iter().find(|r| r.name == selected) {
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
    // The launch check: a provider-pinned pick whose key does not resolve
    // (env var, then api_key_file) refuses pre-wire naming the env var.
    // The draft stays intact; rows with no key_env are not checked, and
    // the spawn door stays the final gate.
    if !l.draft.provider.is_empty() {
        let needle = l
            .draft
            .model_row
            .clone()
            .unwrap_or_else(|| l.draft.model.clone());
        if let Some(CatalogOutcome::Ok(rows, _, _)) = &view.launcher_catalog {
            let row = rows.iter().find(|r| r.name == selected).and_then(|h| {
                h.models.iter().chain(h.more.iter()).find(|m| {
                    m.name == needle && m.provider.as_deref() == Some(l.draft.provider.as_str())
                })
            });
            if let Some(row) = row {
                if let Some(key_env) = row.key_env.as_deref().filter(|k| !k.is_empty()) {
                    if !crate::provider_key::key_present(key_env, row.key_file.as_deref()) {
                        let checked = row
                            .key_file
                            .as_deref()
                            .map(|f| format!(" (checked the env and {f})"))
                            .unwrap_or_default();
                        let provider = l.draft.provider.clone();
                        l.phase = Phase::Refused {
                            request_id,
                            reason: format!(
                                "{provider}: {key_env} is not set{checked}; nothing launched"
                            ),
                        };
                        return Ok(());
                    }
                }
            }
        }
    }
    // The worktree choice resolves against the project facts: a read policy
    // answers its default, an explicit box pick wins, a `never` project
    // always launches in place, and an unread policy with no pick refuses -
    // the composer never guesses (AC6-EDGE).
    let cwd = l.draft.cwd();
    let explicit = l.draft.worktree;
    let picked_branch = l.draft.branch.clone();
    let policy = match &view.launcher_catalog {
        Some(CatalogOutcome::Ok(_, _, facts)) => facts
            .iter()
            .find(|f| f.cwd == cwd)
            .map(|f| f.policy.clone()),
        _ => None,
    };
    let worktree = if matches!(&policy, Some(Ok(w)) if w == "never") {
        Some(false)
    } else {
        // A failed policy read is as good as an unread one: the default
        // needs a READ policy, never a guess (AC6-EDGE).
        explicit.or_else(|| policy.as_ref().filter(|r| r.is_ok()).map(|_| true))
    };
    let Some(worktree) = worktree else {
        l.phase = Phase::Refused {
            request_id,
            reason: "worktree policy unread; pick the box explicitly".to_string(),
        };
        return Ok(());
    };
    let branch = if worktree {
        picked_branch.filter(|b| b != "main")
    } else {
        None
    };
    let mut request = l.draft.request(request_id);
    request.extra_flags = extra_flags;
    request.worktree = worktree;
    request.branch = branch;
    request.force = l.draft.pills.iter().any(|(f, _)| f == "--force");
    if (request.provider.is_some() || request.route.is_some() || !request.extra_flags.is_empty())
        && !supports_launch_extra_axes(&view.session)
    {
        l.phase = Phase::Refused {
            request_id,
            reason: "the connected mux server does not support provider pins or extra launch flags; reconnect to a server running current fno".to_string(),
        };
        return Ok(());
    }
    if request.worktree && !wire_at_least(&view.session, LAUNCH_WORKTREE_PROTO) {
        l.phase = Phase::Refused {
            request_id,
            reason: "the connected mux server does not support worktree launches; reconnect to a server running current fno".to_string(),
        };
        return Ok(());
    }
    l.armed = Some(request_id);
    l.phase = Phase::Submitting { request_id };
    l.show_detail = false;
    remember_harness(&selected);
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

pub(crate) fn version_at_least(version: Option<u32>, min: u32) -> bool {
    version.is_some_and(|version| version >= min)
}

fn wire_at_least(session: &str, min: u32) -> bool {
    let version = crate::proto::socket_path(session)
        .ok()
        .and_then(|socket| crate::mux_rows::read_wire_version(&socket));
    version_at_least(version, min)
}

fn supports_launch_extra_axes(session: &str) -> bool {
    wire_at_least(session, LAUNCH_EXTRA_AXES_PROTO)
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
    /// Ctrl+U: delete everything left of the cursor on the row. Kept apart
    /// from [`LKey::KillWord`]: Cmd+Backspace arrives as Ctrl+U on the
    /// emulators that spell it that way, and the line kill must survive.
    KillLeft,
    /// Option+Backspace: kill the word left of the cursor. Terminals that
    /// spell it ESC+DEL (`\x1b\x7f`) or the CSI-u alt form (`[127;3u`) both
    /// fold here; a terminal that spells Cmd+Backspace the same way gets the
    /// word kill where the line kill used to be - the bytes are not
    /// distinguishable, and word kill is the readline default for ESC+DEL.
    KillWord,
    /// Shift+Enter, when the terminal spells it (`CSI 13;2u`). Handled by
    /// the force launch; dropped where force means nothing.
    ShiftEnter,
    /// Ctrl+O toggles the raw refusal text in the footer.
    CtrlO,
    Left,
    Right,
    Up,
    Down,
    /// Home/Cmd+Left: the cursor jumps to its row's first column. macOS
    /// Cmd+Left/Right spell as Home/End on iTerm2 and Ghostty.
    Home,
    /// End/Cmd+Right: the cursor jumps to its row's last column.
    End,
    /// Option+Left: the cursor moves one word left (a run of
    /// alphanumeric-or-underscore chars; `\n` counts as whitespace).
    WordLeft,
    /// Option+Right: one word right, flat like [`LKey::WordLeft`].
    WordRight,
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
        self.fold_carry(bytes, true)
    }

    /// [`Self::fold`], but a raw-fed caller (no chord scanner in front)
    /// passes `release_lone_esc = false` on a real read: a trailing ESC there
    /// may be the first byte of a split arrow, so only the quiet-window flush
    /// (an empty read) releases it.
    pub(crate) fn fold_carry(&mut self, bytes: &[u8], release_lone_esc: bool) -> Vec<LKey> {
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
                    // ESC+DEL is Option+Backspace (word kill); Ctrl+U keeps
                    // the line kill for the emulators that spell Cmd+
                    // Backspace as Ctrl+U. The pair is one key, never Esc
                    // then delete.
                    if b == 0x7f {
                        self.esc.clear();
                        keys.push(LKey::KillWord);
                        continue;
                    }
                    // Alt-spelled word motion: ESC b/f are Option+Left and
                    // Option+Right on emulators with Option as meta.
                    if b == b'b' || b == b'f' {
                        self.esc.clear();
                        keys.push(if b == b'f' {
                            LKey::WordRight
                        } else {
                            LKey::WordLeft
                        });
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
                // Inside `ESC [` or `ESC O`: decided by the carry's length,
                // not its last byte, so a multi-byte sequence (`[200~`,
                // `[1;5B`) stays one sequence past its second byte.
                Some(_) if self.esc.len() >= 2 => {
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
                                continue;
                            }
                            // Shift+Enter as the kitty/CSI-u spell (`[13;2u`,
                            // or `[13;6u` for ctrl+shift): one key, never a
                            // dropped whole-sequence swallow.
                            if seq == b"[13;2u" || seq == b"[13;6u" {
                                keys.push(LKey::ShiftEnter);
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
                0x15 => keys.push(LKey::KillLeft),
                0x0f => keys.push(LKey::CtrlO),
                0x01..=0x0e | 0x10..=0x14 | 0x16..=0x1a | 0x1c..=0x1f => {
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
        if release_lone_esc && self.paste.is_none() && crate::keys::take_lone_esc(&mut self.esc) {
            keys.push(LKey::Esc);
        }
        keys
    }
}

/// A completed CSI/SS3 sequence's key mapping: bare and modified (`[1;5B`)
/// CSI arrows and SS3 `O A..D` application-mode arrows become the launcher's
/// arrow keys; Home/End (macOS Cmd+Left/Right spell as `[H`/`[F`/`OH`/`OF`)
/// fold to their line jumps; an alt-modified left/right (`[1;3C`/`[1;3D`,
/// also `;4` for shift+alt) is Option+Left/Right word motion; the CSI-u
/// alt+backspace (`[127;3u`) is Option+Backspace word kill; everything else
/// (a focus report, a function key) is dropped whole. `seq` is the sequence
/// text between the ESC introducer's follower ([ or O) and the final byte,
/// both included.
fn arrow_key(seq: &[u8]) -> Option<LKey> {
    if seq == b"[Z" {
        return Some(LKey::BackTab);
    }
    // Home/End: bare CSI and SS3 spellings, macOS Cmd+Left/Right among them.
    if seq == b"[H" || seq == b"OH" {
        return Some(LKey::Home);
    }
    if seq == b"[F" || seq == b"OF" {
        return Some(LKey::End);
    }
    // The CSI-u alt+backspace: Option+Backspace as Ghostty, iTerm2 and kitty
    // spell it when the kitty keyboard protocol is on.
    if seq == b"[127;3u" {
        return Some(LKey::KillWord);
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
    // An alt modifier in the params (`1;3`, `1;4` = shift+alt) turns
    // Left/Right into word motion. Other modifiers keep the plain arrow.
    if ok && seq[0] == b'[' && matches!(seq[seq.len() - 1], b'C' | b'D') {
        let params = &seq[1..seq.len() - 1];
        if params.ends_with(b";3") || params.ends_with(b";4") {
            return Some(if seq[seq.len() - 1] == b'C' {
                LKey::WordRight
            } else {
                LKey::WordLeft
            });
        }
    }
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

/// KillLeft: everything left of the cursor on the cursor's own row. The
/// message wraps and holds newlines, so the kill stops at the line break
/// before the cursor, never at offset 0.
fn kill_to_line_start(draft: &mut LaunchDraft) {
    let cur = char_byte(&draft.message, draft.cursor_chars);
    let start = draft.message[..cur].rfind('\n').map_or(0, |b| b + 1);
    if start == cur {
        return;
    }
    draft.message.replace_range(start..cur, "");
    draft.cursor_chars = draft.message[..start].chars().count();
    draft.bump();
}

/// KillWord: the word left of the cursor, row-local like
/// [`kill_to_line_start`]. A word is a run of alphanumeric-or-underscore
/// chars; the kill also clears the separator chars between the words, in
/// the readline manner.
fn kill_to_word_start(draft: &mut LaunchDraft) {
    let cur = draft.cursor_chars;
    let before: Vec<char> = draft.message.chars().take(cur).collect();
    let drop = word_run(&before, true);
    if drop == 0 {
        return;
    }
    let at = char_byte(&draft.message, cur - drop);
    draft
        .message
        .replace_range(at..char_byte(&draft.message, cur), "");
    draft.cursor_chars -= drop;
    draft.bump();
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

/// One word step's width over `chars`: the whitespace run, then the word
/// run behind or after it. `rev` counts from the back (a leftward step),
/// otherwise from the front. Word motion is flat (a `\n` counts as
/// whitespace, so it crosses rows) while the kills stay row-local: a
/// motion undoes with one keystroke, a kill that ate a newline does not.
/// The settings input fields share it (one editing grammar).
pub(crate) fn word_run(chars: &[char], rev: bool) -> usize {
    let ws = if rev {
        chars.iter().rev().take_while(|c| c.is_whitespace()).count()
    } else {
        chars.iter().take_while(|c| c.is_whitespace()).count()
    };
    let rest = if rev {
        &chars[..chars.len() - ws]
    } else {
        &chars[ws..]
    };
    let word = if rev {
        rest.iter().rev().take_while(|c| is_word_char(**c)).count()
    } else {
        rest.iter().take_while(|c| is_word_char(**c)).count()
    };
    ws + word
}

fn move_word_left(draft: &mut LaunchDraft) {
    let cur = draft.cursor_chars;
    let before: Vec<char> = draft.message.chars().take(cur).collect();
    draft.cursor_chars = cur - word_run(&before, true);
}

fn move_word_right(draft: &mut LaunchDraft) {
    let cur = draft.cursor_chars;
    let after: Vec<char> = draft.message.chars().skip(cur).collect();
    draft.cursor_chars = cur + word_run(&after, false);
}

fn move_line_start(draft: &mut LaunchDraft) {
    let cur = draft.cursor_chars;
    let before: Vec<char> = draft.message.chars().take(cur).collect();
    let row = before.iter().rev().take_while(|c| **c != '\n').count();
    draft.cursor_chars = cur - row;
}

fn move_line_end(draft: &mut LaunchDraft) {
    let cur = draft.cursor_chars;
    let after: Vec<char> = draft.message.chars().skip(cur).collect();
    let rest = after.iter().take_while(|c| **c != '\n').count();
    draft.cursor_chars = cur + rest;
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
    // A stale chip picker commits through the rows the operator SEES, so
    // the snapshot refreshes before the first key is handled.
    refresh_stale_picker(view);
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
            // unoffered pins against the catalog, and a step-down rebuilds
            // rows from both sources).
            let catalog = view.launcher_catalog.clone();
            let backlog = view.backlog.clone();
            if let Some(l) = view.launcher.as_mut() {
                let Some(mut picker) = l.picker.take() else {
                    unreachable!("checked Some above");
                };
                match key {
                    LKey::Esc | LKey::Tab | LKey::BackTab => {
                        // Esc steps a drilled Model picker back down its
                        // ladder (More -> Main) and closes from the main
                        // list; the dock keeps the draft either way. Tab
                        // hands the keyboard back to the dock, which then
                        // moves focus normally. The help sheet has no ladder:
                        // Esc closes it outright.
                        if matches!(key, LKey::Esc) && picker.mode == PickerMode::Help {
                            l.picker = None;
                            break;
                        }
                        if matches!(key, LKey::Esc) && picker.mode != PickerMode::Main {
                            picker_step_down(l, &catalog, &backlog, picker);
                        }
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
                        picker.popup.follow_sel(view.term);
                        l.picker = Some(picker);
                    }
                    LKey::Down => {
                        picker.popup.nav(NavDir::Down);
                        picker.popup.follow_sel(view.term);
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
                            let anchor = picker.anchor;
                            commit_picker_action(l, &catalog, action, portal, picker.field, anchor);
                        }
                        // A disabled or header row: the picker stays open.
                    }
                    LKey::Char(' ') if picker.field == Focus::Plus => {
                        // Space commits the typed text as a verbatim flag:
                        // the filter is the word after the dashes the
                        // message still holds. An empty filter is no flag:
                        // the picker closes and the dashes stay typed.
                        let filter = std::mem::take(&mut picker.filter);
                        l.picker = None;
                        if !filter.is_empty() {
                            cut_trailing_word(l);
                            add_pill(l, format!("--{filter}"), true);
                        }
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
                    l.focus = if delta > 0 {
                        l.focus.next(l, &view.launcher_catalog)
                    } else {
                        l.focus.prev(l, &view.launcher_catalog)
                    };
                }
            }
            LKey::Up | LKey::Down => {
                let delta = if matches!(key, LKey::Up) { -1 } else { 1 };
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message {
                        move_up_down(&mut l.draft, delta);
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
                        _ => {}
                    }
                }
            }
            LKey::Backspace => {
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        // Shell mode with an empty line: Backspace leaves
                        // shell mode, back to the plain composer (AC2).
                        Focus::Message if l.shell && l.draft.message.is_empty() => {
                            l.shell = false;
                            l.draft.bump();
                        }
                        Focus::Message => {
                            if l.draft.pill_value_capture {
                                if l.draft.pill_value_draft.pop().is_none() {
                                    // An empty value draft: backspace backs
                                    // out of the capture itself.
                                    l.draft.pill_value_capture = false;
                                }
                            } else if l.draft.message.is_empty() {
                                l.draft.pills.pop();
                                l.draft.bump();
                            } else {
                                backspace(&mut l.draft);
                            }
                        }
                        Focus::Permission => {
                            l.draft.permission.pop();
                            l.draft.bump();
                        }
                        _ => {}
                    }
                }
            }
            LKey::KillLeft => {
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message && !l.draft.pill_value_capture {
                        kill_to_line_start(&mut l.draft);
                    }
                }
            }
            LKey::KillWord => {
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message && !l.draft.pill_value_capture {
                        kill_to_word_start(&mut l.draft);
                    }
                }
            }
            LKey::Home | LKey::End => {
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message && !l.draft.pill_value_capture {
                        if matches!(key, LKey::Home) {
                            move_line_start(&mut l.draft);
                        } else {
                            move_line_end(&mut l.draft);
                        }
                    }
                }
            }
            LKey::WordLeft | LKey::WordRight => {
                if let Some(l) = view.launcher.as_mut() {
                    if l.focus == Focus::Message && !l.draft.pill_value_capture {
                        if matches!(key, LKey::WordLeft) {
                            move_word_left(&mut l.draft);
                        } else {
                            move_word_right(&mut l.draft);
                        }
                    }
                }
            }
            LKey::CtrlO => {
                // The raw refusal text toggles only when there is one to
                // show; a refused/unknown footer carries it, editing does not.
                if let Some(l) = view.launcher.as_mut() {
                    if matches!(l.phase, Phase::Refused { .. } | Phase::Unknown { .. }) {
                        l.show_detail = !l.show_detail;
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
                if pending {
                    // Esc is the explicit cancel while an attempt pends.
                } else if focus == Focus::Message {
                    let shell = view.launcher.as_ref().is_some_and(|l| l.shell);
                    if shell {
                        // A shell line runs through the bang path, never
                        // the launch path: no harness, model or pill rides
                        // it.
                        bang::run(view, sock_w).await?;
                    } else {
                        // A value capture finalizes before the launch: the
                        // captured word lands on its pill or chip first.
                        if let Some(l) = view.launcher.as_mut() {
                            finalize_pill_value(l);
                        }
                        submit(view, sock_w).await?;
                    }
                } else if focus == Focus::Worktree {
                    if let Some(l) = view.launcher.as_mut() {
                        toggle_worktree(l, &view.launcher_catalog);
                    }
                } else if focus == Focus::Branch
                    && view
                        .launcher
                        .as_ref()
                        .is_some_and(|l| l.draft.policy_never(&view.launcher_catalog))
                {
                    // A `never` project runs in place, so the Branch chip is
                    // read-only there: it shows the current branch and opens
                    // no picker.
                } else {
                    // A chip: Enter opens its picker, anchored one row under
                    // the chip.
                    let anchor = view
                        .launcher
                        .as_ref()
                        .and_then(|l| chip_anchor(l, view, focus));
                    if let Some(l) = view.launcher.as_mut() {
                        open_picker_at(l, &view.launcher_catalog, &view.backlog, anchor, focus);
                    }
                }
            }
            LKey::ShiftEnter => {
                // The force launch: what Enter does, with the per-request
                // admission and spawn-gate override armed. Shell lines never
                // force (a `!` line is never refused for admission).
                let (focus, pending, shell) = view
                    .launcher
                    .as_ref()
                    .map(|l| {
                        (
                            l.focus,
                            matches!(l.phase, Phase::Unknown { .. } | Phase::Submitting { .. }),
                            l.shell,
                        )
                    })
                    .unwrap_or((Focus::Message, false, false));
                if !pending && focus == Focus::Message && !shell {
                    if let Some(l) = view.launcher.as_mut() {
                        // The pill IS the armed state: visible, removable,
                        // and it survives a refusal for the retry.
                        if !l.draft.pills.iter().any(|(f, _)| f == "--force") {
                            l.draft.pills.push(("--force".to_string(), None));
                            l.draft.bump();
                        }
                    }
                    if let Some(l) = view.launcher.as_mut() {
                        finalize_pill_value(l);
                    }
                    submit(view, sock_w).await?;
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
                let help_anchor = if c == '?' {
                    view.launcher.as_ref().and_then(|l| picker_anchor(l, view))
                } else {
                    None
                };
                let dash_anchor = if c == '-' {
                    view.launcher.as_ref().and_then(|l| pills_anchor(l, view))
                } else {
                    None
                };
                if let Some(l) = view.launcher.as_mut() {
                    match l.focus {
                        // `?` on an empty input opens the composer help
                        // sheet, in plain or shell mode: a glyph on an empty
                        // input is a mode switch, not text (the `!` rule).
                        Focus::Message
                            if c == '?'
                                && l.draft.message.is_empty()
                                && !l.draft.pill_value_capture =>
                        {
                            open_help(
                                l,
                                help_anchor
                                    .map(|(row, col)| Anchor::At { row, col })
                                    .unwrap_or(Anchor::Center),
                            );
                        }
                        // Shell mode: every character is command text; the
                        // launch gestures (@, --, space) stay literal (AC3).
                        Focus::Message if l.shell && !l.draft.pill_value_capture => {
                            insert_char(&mut l.draft, c);
                        }
                        // A `!` on an empty plain input is the mode switch:
                        // consumed, never text (AC1).
                        Focus::Message
                            if c == '!'
                                && !l.shell
                                && l.draft.message.is_empty()
                                && l.draft.pills.is_empty()
                                && !l.draft.pill_value_capture =>
                        {
                            l.shell = true;
                            l.draft.bump();
                        }
                        Focus::Message if c == '@' && !l.draft.pill_value_capture => {
                            open_picker_at(
                                l,
                                &view.launcher_catalog,
                                &view.backlog,
                                at_anchor,
                                Focus::Message,
                            );
                        }
                        Focus::Message if c == '-' && !l.draft.pill_value_capture => {
                            // The second dash of a word-start `--` opens the
                            // flags picker; the typed text stays in the
                            // message until a pick or a Space commits it.
                            let opens = trailing_word(&l.draft.message) == Some("-");
                            if opens {
                                open_picker_at(
                                    l,
                                    &view.launcher_catalog,
                                    &view.backlog,
                                    dash_anchor,
                                    Focus::Plus,
                                );
                            } else {
                                insert_char(&mut l.draft, c);
                            }
                        }
                        Focus::Message if c == ' ' && !l.draft.pill_value_capture => {
                            // Space while a `--word` trails commits it as a
                            // verbatim pill; value capture takes the next
                            // word. Otherwise the space is message text.
                            let word = trailing_word(&l.draft.message).unwrap_or("");
                            if word.starts_with("--") && word != "--" {
                                commit_verbatim_pill(l);
                            } else if l.draft.pill_value_capture {
                                finalize_pill_value(l);
                            } else {
                                insert_char(&mut l.draft, c);
                            }
                        }
                        Focus::Message => {
                            if l.draft.pill_value_capture {
                                if c == ' ' {
                                    // Space finalizes the captured value.
                                    finalize_pill_value(l);
                                } else {
                                    let room = MAX_LAUNCH_FLAGS_CHARS
                                        .saturating_sub(l.draft.pill_value_draft.chars().count());
                                    if room > 0 {
                                        l.draft.pill_value_draft.push(c);
                                        l.draft.bump();
                                    }
                                }
                            } else {
                                insert_char(&mut l.draft, c);
                            }
                        }
                        Focus::Worktree if c == ' ' => {
                            toggle_worktree(l, &view.launcher_catalog);
                        }
                        Focus::Permission => {
                            // Free text only where the capability table
                            // declares an empty choice list; the TypeIn row
                            // is the only door here.
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
                        if l.draft.pill_value_capture {
                            // A paste during value capture lands in the pill,
                            // whitespace flattened, under the same ceiling.
                            let room = MAX_LAUNCH_FLAGS_CHARS
                                .saturating_sub(l.draft.pill_value_draft.chars().count());
                            for c in text.chars().take(room) {
                                let c = if c.is_whitespace() { ' ' } else { c };
                                l.draft.pill_value_draft.push(c);
                            }
                            l.draft.bump();
                        } else {
                            // The draft never exceeds the submit ceiling: chars
                            // past it are dropped here, visibly at the next
                            // render, rather than being refused only at Launch.
                            let room =
                                MAX_MAIL_TEXT.saturating_sub(l.draft.message.chars().count());
                            for c in text.chars().take(room) {
                                insert_char(&mut l.draft, c);
                            }
                        }
                    }
                    // A paste while a chip is focused: the bytes are data
                    // and are dropped, never forwarded to a pane.
                }
            }
        }
    }
    Ok(StdinFlow::Continue)
}

/// Space/Enter/click on the worktree box: an explicit pick replaces the
/// policy default. A `never` project never toggles - in place is the only
/// legal launch there, so the box stays painted unchecked and greyed.
fn toggle_worktree(l: &mut Launcher, catalog: &Option<CatalogOutcome>) {
    if l.draft.policy_never(catalog) {
        return;
    }
    let current = l.draft.worktree_state(catalog).unwrap_or(false);
    l.draft.worktree = Some(!current);
    l.draft.bump();
}

/// Cycle the current harness's effort one step (`delta` -1/+1) through the
/// capability table's list, the empty pin (the harness default) first. A
/// harness with no effort list keeps its pin untouched.
fn cycle_effort(l: &mut Launcher, delta: i32, catalog: &Option<CatalogOutcome>) {
    let harness = l.draft.harness();
    let Some(CatalogOutcome::Ok(rows, _, _)) = catalog else {
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
    let Some(CatalogOutcome::Ok(rows, _, _)) = catalog else {
        return;
    };
    let Some(row) = rows.iter().find(|r| r.name == harness) else {
        return;
    };
    if let Some(name) = &draft.model_row {
        let offered = row.models.iter().any(|m| {
            &m.name == name && m.provider.as_deref().unwrap_or_default() == draft.provider
        }) || row.more.iter().any(|m| {
            // A pin on a Ready row under more survives: the row exists, it
            // just waits beyond the main list.
            matches!(m.state, ModelState::Ready)
                && &m.name == name
                && m.provider.as_deref().unwrap_or_default() == draft.provider
        });
        if !offered {
            draft.model.clear();
            draft.model_row = None;
            draft.route.clear();
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
// -- catalog -----------------------------------------------------------------

/// The harness capability contract the mux already ships and the spawn door
/// enforces: `include_str!`ed at compile time, regenerated from the
/// canonical copy by fno-agents' build.rs. The catalog is the set of
/// `[harness.<name>]` tables; there is no UI-only list to drift.
pub(crate) const CAPABILITY_TOML: &str = include_str!("../harness_capabilities.toml");

/// The capability table's first model slug per harness: the flagship the
/// Model picker lists right under `harness default`. The table is
/// compile-time, so the map computes once; a parse failure empties it and
/// the picker simply shows no flagship row.
fn flagship_slug(harness: &str) -> Option<String> {
    static FLAGSHIPS: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    let map = FLAGSHIPS.get_or_init(|| {
        toml::from_str::<toml::Value>(CAPABILITY_TOML)
            .ok()
            .and_then(|parsed| parsed.get("harness").and_then(|h| h.as_table()).cloned())
            .map(|table| {
                table
                    .into_iter()
                    .filter_map(|(name, caps)| {
                        let first = caps
                            .get("models")
                            .and_then(|v| v.as_array())
                            .and_then(|a| a.iter().find_map(|x| x.as_str().map(str::to_string)));
                        first.map(|first| (name, first))
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    map.get(harness).cloned()
}

/// The next free portal index in the active layout, smallest first. A
/// thread placement opens its new portal here; a stale read costs one
/// refusal the door renders verbatim, never a wrong lane.
fn next_free_portal(view: &View) -> u8 {
    let used: Vec<u8> = view.layout.agents.iter().filter_map(|a| a.portal).collect();
    (0..=u8::MAX).find(|p| !used.contains(p)).unwrap_or(0)
}

/// The catalog read: a compiled-in table (each harness's model floor
/// included) plus PATH stats, then bounded reads for configured routing
/// rows, codex's own model cache, and OpenCode's installed model list, and
/// one facts row per project in `projects` (git branch facts + the resolved
/// worktree policy word). Delivered through the probe channel so the dock
/// has one "not yet read" and "read" shape; async because the subprocess
/// reads are.
pub(crate) async fn load_catalog(projects: Vec<String>) -> CatalogOutcome {
    let Ok(parsed) = toml::from_str::<toml::Value>(CAPABILITY_TOML) else {
        return CatalogOutcome::Degraded("harness catalog: capability table unparseable".into());
    };
    let Some(table) = parsed.get("harness").and_then(|h| h.as_table()) else {
        return CatalogOutcome::Degraded("harness catalog: no [harness] table".into());
    };
    let mut rows: Vec<HarnessChoice> = table
        .iter()
        // The user retired gemini (upstream CLI deprecated): the composer
        // picker never lists it again. The capability table keeps the row
        // for its other consumers (resume argv, state grants).
        .filter(|(name, _)| *name != "gemini")
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
            let models = caps.get("models").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|slug| ModelChoice {
                        name: slug.to_string(),
                        model: slug.to_string(),
                        route: String::new(),
                        provider: None,
                        state: ModelState::Ready,
                        key_env: None,
                        key_file: None,
                    })
                    .collect()
            });
            let launch_flags = caps
                .get("launch_flags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                });
            HarnessChoice {
                name: name.clone(),
                native: true,
                installed: on_path(name),
                models: models.unwrap_or_default(),
                more: Vec::new(),
                catalog_error: None,
                models_error: None,
                efforts,
                permission_modes,
                launch_flags,
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
    let routing_argv = [bin.as_str(), "config", "get", "model_routing", "-J"];
    let opencode_argv = ["opencode", "models", "--pure"];
    let opencode_installed = rows
        .iter()
        .any(|row| row.name == "opencode" && row.selectable());
    let timeout = std::time::Duration::from_secs(30);
    let deadline = tokio::time::Instant::now() + timeout;
    let (accounts, routing, opencode) = tokio::join!(
        crate::dispatch_launch::run_fno_captured(&account_argv, timeout, deadline),
        crate::dispatch_launch::run_fno_captured(&routing_argv, timeout, deadline),
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
    // The models.dev catalog: read whatever cache exists now; the stale
    // check moved to `model_catalog::refresh_if_stale`, which the mux server
    // also runs hourly so pricing never depends on opening the composer.
    let state = crate::model_catalog::state_dir();
    crate::model_catalog::refresh_if_stale(&state);
    let (catalog, catalog_error) = match crate::model_catalog::load(&state) {
        Ok(catalog) => (Some(catalog), None),
        Err(reason) => (None, Some(reason)),
    };
    let routing_value: serde_json::Value = match routing {
        Some((true, stdout, _)) => serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::Null,
    };
    let reach = crate::model_catalog::parse_reach(crate::model_catalog::REACH_TOML);

    // The floor stands; codex tops up from its own models cache and every
    // harness merges its configured account rows over the floor. opencode
    // owns its list outright (above).
    let (codex_cache, codex_hidden) = if rows.iter().any(|row| row.name == "codex") {
        match codex_models_cache_path().map(|path| std::fs::read_to_string(path)) {
            Some(Ok(text)) => parse_codex_models(&text),
            _ => (Vec::new(), Vec::new()),
        }
    } else {
        (Vec::new(), Vec::new())
    };
    for row in &mut rows {
        if row.name == "opencode" {
            row.models = opencode_models.clone();
            row.models_error = opencode_error.clone();
        } else {
            if row.name == "codex" {
                // The live cache wins over the floor: a slug it hides is
                // retired even when the table still lists it. Configured
                // account rows merge after, so an explicit route survives.
                row.models
                    .retain(|m| !codex_hidden.iter().any(|slug| *slug == m.model));
                merge_model_choices(&mut row.models, &codex_cache);
            }
            if let Some(list) = by_harness.get(&row.name) {
                merge_model_choices(&mut row.models, list);
            }
        }
        // The reach rows: Ready provider models join the main list; the rest
        // wait under `more`. The native catalog's rows are Ready by
        // definition (they launch through the harness's own routing), but
        // they live under more so the floor and the configured rows lead.
        let (ready, mut more) = crate::model_catalog::reach_rows(
            &row.name,
            &reach,
            &routing_value,
            &by_harness,
            catalog.as_ref(),
            &|env, file| crate::provider_key::key_present(env, file),
        );
        let (ready, native_more) = match reach
            .harness
            .get(&row.name)
            .and_then(|h| h.native_catalog.clone())
        {
            Some(native_id) => {
                let floor_ids: Vec<&str> = row.models.iter().map(|m| m.model.as_str()).collect();
                let (native_rows, rest): (Vec<_>, Vec<_>) = ready
                    .into_iter()
                    .partition(|r| r.provider.as_deref() == Some(native_id.as_str()));
                let kept: Vec<ModelChoice> = native_rows
                    .into_iter()
                    .filter(|r| !floor_ids.contains(&r.model.as_str()))
                    .collect();
                (rest, kept)
            }
            None => (ready, Vec::new()),
        };
        more.extend(native_more);
        merge_model_choices(&mut row.models, &ready);
        row.more = more;
        row.catalog_error = catalog_error.clone();
    }
    let facts = probe_project_facts(projects, bin.as_str(), timeout, deadline).await;
    CatalogOutcome::Ok(rows, models_err, facts)
}

/// One project's facts read: current branch, local branches, and the
/// worktree policy word - three bounded subprocess reads run in parallel
/// under the probe's shared deadline. A non-git cwd yields no branch facts;
/// the policy verb's failure lands in `policy` as the named reason.
async fn probe_project_facts(
    projects: Vec<String>,
    fno: &str,
    timeout: std::time::Duration,
    deadline: tokio::time::Instant,
) -> Vec<ProjectFacts> {
    let mut facts = Vec::with_capacity(projects.len());
    for cwd in projects {
        let git_current = ["git", "-C", cwd.as_str(), "branch", "--show-current"];
        let git_branches = [
            "git",
            "-C",
            cwd.as_str(),
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ];
        let policy_argv = [
            fno,
            "agents",
            "workspace",
            "worktree",
            "policy",
            "--repo",
            cwd.as_str(),
        ];
        let (current, branches, policy) = tokio::join!(
            crate::dispatch_launch::run_fno_captured(&git_current, timeout, deadline),
            crate::dispatch_launch::run_fno_captured(&git_branches, timeout, deadline),
            crate::dispatch_launch::run_fno_captured(&policy_argv, timeout, deadline),
        );
        let current = current.filter(|(ok, _, _)| *ok).and_then(|(_, out, _)| {
            out.lines()
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
        let branches: Vec<String> = branches
            .filter(|(ok, _, _)| *ok)
            .map(|(_, out, _)| {
                out.lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let policy = match policy {
            Some((true, out, _)) => out
                .lines()
                .next()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .ok_or_else(|| "policy verb printed nothing".to_string()),
            Some((false, _, stderr)) => Err(stderr
                .lines()
                .next()
                .unwrap_or("policy verb failed")
                .trim()
                .to_string()),
            None => Err("policy read timed out".to_string()),
        };
        facts.push(ProjectFacts {
            cwd,
            current,
            branches,
            policy,
        });
    }
    facts
}

// -- picker ------------------------------------------------------------------
/// Open a picker on a precomputed anchor. The catalog and backlog ride as
/// borrows so the key folder (holding `view.launcher.as_mut`) can reach
/// them through their own, disjoint fields.
pub(crate) fn open_picker_at(
    l: &mut Launcher,
    catalog: &Option<CatalogOutcome>,
    backlog: &[crate::proto::BacklogCard],
    anchor: Option<(u16, u16)>,
    field: Focus,
) -> bool {
    let Some((row, col)) = anchor else {
        return false;
    };
    let (all_rows, all_actions) = picker_rows(l, field, catalog, backlog);
    // The open popup is the unfiltered walk of the same row set; one
    // builder answers open and rebuild, so the chrome can never split.
    let (popup, actions) =
        filtered_popup(field, &all_rows, &all_actions, "", Anchor::At { row, col });
    l.picker = Some(Picker {
        popup,
        actions,
        all_rows,
        all_actions,
        field,
        anchor: Anchor::At { row, col },
        filter: String::new(),
        mode: PickerMode::Main,
    });
    true
}

/// Rebuild the popover's rows around the live filter: substring match on
/// the entry labels, case-insensitive. The query rides the TITLE (never a
/// selectable header row, never a value); a header whose rows all filtered
/// away hides with them. Works off the picker's own captured row set, so it
/// needs no View access.
fn rebuild_picker(l: &mut Launcher, mut picker: Picker) {
    let (popup, actions) = filtered_popup(
        picker.field,
        &picker.all_rows,
        &picker.all_actions,
        &picker.filter,
        picker.anchor,
    );
    picker.popup = popup;
    picker.popup.sel = 0;
    picker.actions = actions;
    l.picker = Some(picker);
}

/// The filtered popup for a captured row set and live query: substring
/// match on the entry labels, headers whose rows all filtered away hidden.
/// Shared by [`rebuild_picker`] (which stores the result back) and the
/// draw-time refresh in [`draw_overlay`] (which does not), so a catalog
/// read landing mid-picker updates the open list live, the same way the
/// old tab bodies re-derived their rows every render.
pub(crate) fn filtered_popup(
    field: Focus,
    all_rows: &[PopupRow],
    all_actions: &[Option<PickerAction>],
    filter: &str,
    anchor: Anchor,
) -> (Popup, Vec<Option<PickerAction>>) {
    let q = filter.to_lowercase();
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    let mut last_header: Option<(PopupRow, Option<PickerAction>)> = None;
    for (row, action) in all_rows.iter().cloned().zip(all_actions.iter().cloned()) {
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
    let footer = if field == Focus::Model {
        "up/down move \u{b7} left/right effort \u{b7} type to filter \u{b7} enter pick \u{b7} esc back"
    } else {
        "up/down move \u{b7} type to filter \u{b7} enter pick \u{b7} esc close"
    };
    let mut popup = Popup::new(rows, anchor)
        .footer(footer)
        .full_chrome()
        .full_width_selection()
        .label_first();
    // The Branch and Model lists can run hundreds of rows; the popover never
    // takes the whole screen - the composer sheet stays readable behind it.
    if matches!(field, Focus::Branch | Focus::Model) {
        popup = popup.body_cap_pct(50);
    }
    if !filter.is_empty() {
        popup = popup.title(format!(
            "{title} \u{b7} filter: {filter}",
            title = title_for(field)
        ));
    } else {
        let t = title_for(field);
        if !t.is_empty() {
            popup = popup.title(t);
        }
    }
    (popup, actions)
}

/// The picker's base title, shared by open and rebuild so the filter state
/// can never split the two spellings apart.
fn title_for(field: Focus) -> String {
    match field {
        Focus::Harness => "harness".to_string(),
        Focus::Model => "model".to_string(),
        Focus::Project => "directory".to_string(),
        Focus::Plus => "flags".to_string(),
        Focus::Permission => "mode".to_string(),
        Focus::Where => "where".to_string(),
        Focus::Effort => "effort".to_string(),
        Focus::Branch => "branch".to_string(),
        _ => String::new(),
    }
}

/// Persist a chip picker whose axis's row source moved under it (a catalog
/// read landing mid-picker): the stored snapshot refreshes in place, filter
/// intact, so input commits through the rows the operator actually sees.
/// The draw path paints the same refresh immutably; this one runs before
/// the first key or click lands.
fn refresh_stale_picker(view: &mut View) {
    let fresh = view.launcher.as_ref().and_then(|l| {
        let pk = l.picker.as_ref()?;
        if pk.field == Focus::Message
            || matches!(pk.mode, PickerMode::Steps { .. } | PickerMode::Help)
        {
            return None;
        }
        let (rows, actions) = match pk.mode {
            PickerMode::More => more_rows(l, &view.launcher_catalog),
            _ => picker_rows(l, pk.field, &view.launcher_catalog, &view.backlog),
        };
        (rows != pk.all_rows).then_some((pk.field, rows, actions))
    });
    let Some((field, rows, actions)) = fresh else {
        return;
    };
    if let Some(l) = view.launcher.as_mut() {
        let Some(pk) = l.picker.as_mut() else {
            return;
        };
        if pk.field != field {
            return;
        }
        let filter = pk.filter.clone();
        pk.all_rows = rows;
        pk.all_actions = actions;
        let (popup, rebuilt) =
            filtered_popup(pk.field, &pk.all_rows, &pk.all_actions, &filter, pk.anchor);
        pk.popup = popup;
        pk.popup.sel = 0;
        pk.actions = rebuilt;
    }
}

/// The `@` node picker's anchor: the editor's cell inside the sheet, in the
/// same geometry `launcher_mouse` maps with.
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

/// A chip picker's anchor: one row under the chip, in the same geometry
/// paint and `launcher_mouse` map with. `None` when the chip is not painted
/// (the sheet cannot fit) or the field paints no chip.
fn chip_anchor(l: &Launcher, view: &View, field: Focus) -> Option<(u16, u16)> {
    let sl = l.sheet_layout(view)?;
    let (_, r) = sl.chips.iter().find(|(f, _)| *f == field)?;
    let (oy, ox) = (sl.origin.0 as usize + 1, sl.origin.1 as usize + 1);
    Some(((oy + r.y as usize + 1) as u16, (ox + r.x as usize) as u16))
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
pub(crate) fn picker_rows(
    l: &Launcher,
    field: Focus,
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
    match field {
        Focus::Harness => match catalog {
            Some(CatalogOutcome::Ok(rows_found, _, _)) => {
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
        Focus::Model => match catalog {
            Some(CatalogOutcome::Ok(rows_found, models_err, _)) => {
                let harness_row = rows_found.iter().find(|row| row.name == harness);
                let models = harness_row.map(|row| &row.models);
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
                // The flagship: the capability table's first model still on
                // the harness's list, one row under the default and not
                // repeated in its group.
                let flagship = flagship_slug(&harness).and_then(|slug| {
                    models.and_then(|ms| {
                        ms.iter()
                            .find(|m| m.provider.is_none() && m.model == slug)
                            .cloned()
                    })
                });
                if let Some(m) = &flagship {
                    let check =
                        Some(&m.name) == l.draft.model_row.as_ref() && !l.draft.model.is_empty();
                    push_entry(
                        &mut rows,
                        &mut actions,
                        if check { "\u{2713}" } else { "\u{25cf}" },
                        &m.name,
                        "flagship",
                        true,
                        Some(PickerAction::PickRow {
                            harness: harness.clone(),
                            name: m.name.clone(),
                            model: m.model.clone(),
                            route: m.route.clone(),
                            provider: m.provider.clone(),
                        }),
                    );
                }
                // A failed read names itself right under the default, so
                // the notice stays above the fold whatever the model list
                // fills in below.
                let harness_error = rows_found
                    .iter()
                    .find(|row| row.name == harness)
                    .and_then(|row| row.models_error.as_deref())
                    .or_else(|| {
                        (harness != "opencode")
                            .then_some(())
                            .and_then(|_| models_err.as_deref())
                    });
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
                }
                if let Some(models) = models {
                    // The Model body groups by provider (the opencode
                    // ruling): one header per connected provider with its
                    // provider/model ids under it; a model with no provider
                    // groups under the harness itself. A routed pin's group
                    // leads and the harness-own floor sorts last, so the
                    // configured rows stay above the fold on a short
                    // terminal.
                    let mut groups = group_by_provider(models, &harness);
                    groups.sort_by(|a, b| {
                        let harness_own = |g: &(String, Vec<&ModelChoice>)| g.0 == harness;
                        harness_own(a)
                            .cmp(&harness_own(b))
                            .then_with(|| a.0.cmp(&b.0))
                    });
                    for (key, list) in &groups {
                        rows.push(PopupRow::Header(key.clone()));
                        actions.push(None);
                        for m in list.iter() {
                            // The flagship lives right under the default row;
                            // its floor entry never repeats in the harness-own
                            // group.
                            if flagship
                                .as_ref()
                                .is_some_and(|f| m.provider.is_none() && m.model == f.model)
                            {
                                continue;
                            }
                            let check = Some(&m.name) == l.draft.model_row.as_ref()
                                && !l.draft.model.is_empty();
                            let hint = model_hint(m);
                            push_entry(
                                &mut rows,
                                &mut actions,
                                if check { "\u{2713}" } else { "\u{25cf}" },
                                &m.name,
                                &hint,
                                matches!(m.state, ModelState::Ready),
                                matches!(m.state, ModelState::Ready).then(|| {
                                    PickerAction::PickRow {
                                        harness: harness.clone(),
                                        name: m.name.clone(),
                                        model: m.model.clone(),
                                        route: m.route.clone(),
                                        provider: m.provider.clone(),
                                    }
                                }),
                            );
                        }
                    }
                }
                // The more row: the harness's full catalog tail, searchable
                // by typing. A catalog read failure disables it with the
                // reason; an empty tail disables it with the count.
                if let Some(hr) = harness_row {
                    let (enabled, hint) = match &hr.catalog_error {
                        Some(reason) => (false, reason.clone()),
                        None => (
                            !hr.more.is_empty(),
                            format!("{} models, type to search", hr.more.len()),
                        ),
                    };
                    push_entry(
                        &mut rows,
                        &mut actions,
                        "\u{2022}",
                        "more\u{2026}",
                        &hint,
                        enabled,
                        enabled.then_some(PickerAction::OpenMore),
                    );
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
            if let Some(CatalogOutcome::Ok(catalog_rows, _, _)) = catalog {
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
        Focus::Branch => {
            // `main` first (ensure's default: a fresh branch off
            // origin/main), then the project's local branches; type-to-filter
            // finds one among thousands. Picking a branch other than the
            // current one checks the worktree box (apply_picker_action).
            let facts = l.draft.facts(catalog);
            let current = facts.and_then(|f| f.current.clone());
            push_entry(
                &mut rows,
                &mut actions,
                if l.draft.branch.as_deref().unwrap_or("main") == "main" {
                    "\u{2713}"
                } else {
                    "\u{2022}"
                },
                "main",
                "fresh branch off origin/main",
                true,
                Some(PickerAction::SetBranch("main".to_string())),
            );
            if let Some(facts) = facts {
                for b in &facts.branches {
                    // The leading row already speaks for main (ensure's
                    // fresh-branch default); the checkout's own main entry
                    // would read as a second main.
                    if b == "main" {
                        continue;
                    }
                    push_entry(
                        &mut rows,
                        &mut actions,
                        if l.draft.branch.as_deref() == Some(b.as_str()) {
                            "\u{2713}"
                        } else {
                            "\u{2022}"
                        },
                        b,
                        if current.as_deref() == Some(b.as_str()) {
                            "current"
                        } else {
                            ""
                        },
                        true,
                        Some(PickerAction::SetBranch(b.clone())),
                    );
                }
            }
        }
        Focus::Where => {
            // Run on: today only Local. Cloud, Remote Control and SSH name
            // no substrate in the door yet, so there are no rows to offer.
            rows.push(PopupRow::Header("Run on".to_string()));
            actions.push(None);
            push_entry(&mut rows, &mut actions, "\u{2713}", "Local", "", true, None);
            // Open as: the placement (the view a thread shows through).
            rows.push(PopupRow::Header("Open as".to_string()));
            actions.push(None);
            for p in [
                Placement::Thread,
                Placement::ThreadSplitBeside,
                Placement::ThreadNewTab,
                Placement::PaneActiveTab,
            ] {
                push_entry(
                    &mut rows,
                    &mut actions,
                    if l.draft.placement == p {
                        "\u{2713}"
                    } else {
                        "\u{2022}"
                    },
                    p.label(),
                    "",
                    true,
                    Some(PickerAction::Place(p)),
                );
            }
        }
        Focus::Effort => {
            // "<harness> decides" clears the pin; the catalog's declared
            // efforts follow. A harness whose row declares no effort axis
            // paints no chip at all, so this arm only runs when it exists.
            push_entry(
                &mut rows,
                &mut actions,
                if l.draft.effort.is_empty() {
                    "\u{2713}"
                } else {
                    "\u{2022}"
                },
                &decides,
                "",
                true,
                Some(PickerAction::Clear),
            );
            if let Some(CatalogOutcome::Ok(catalog_rows, _, _)) = catalog {
                if let Some(row) = catalog_rows.iter().find(|r| r.name == harness) {
                    if let Some(efforts) = &row.efforts {
                        for e in efforts {
                            push_entry(
                                &mut rows,
                                &mut actions,
                                if l.draft.effort == *e {
                                    "\u{2713}"
                                } else {
                                    "\u{2022}"
                                },
                                e,
                                "",
                                true,
                                Some(PickerAction::Set(e.clone())),
                            );
                        }
                    }
                }
            }
        }
        Focus::Plus => {
            // The `--` flags picker: fno's own spawn options first (labelled
            // apart from the harness's), then the chosen harness's launch
            // flags off the capture, filtered by what the user typed after
            // the dashes. Each entry spells `--flag <value>` when the flag
            // takes one; the picker's type-to-filter narrows in place.
            rows.push(PopupRow::Header("fno spawn options".to_string()));
            actions.push(None);
            push_entry(
                &mut rows,
                &mut actions,
                if l.draft.pills.iter().any(|(f, _)| f == "--force") {
                    "\u{2713}"
                } else {
                    "\u{2022}"
                },
                "--force",
                "past the admission brake and the spawn gate; journaled",
                true,
                Some(PickerAction::Force),
            );
            push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                "--name <name>",
                "name the worker",
                true,
                Some(PickerAction::AddPill {
                    flag: "--name".to_string(),
                    picks_value: true,
                }),
            );
            push_entry(
                &mut rows,
                &mut actions,
                "\u{2022}",
                "--account <account>",
                "pick the provider account",
                true,
                Some(PickerAction::AddPill {
                    flag: "--account".to_string(),
                    picks_value: true,
                }),
            );
            let Some(CatalogOutcome::Ok(catalog_rows, _, _)) = catalog else {
                push_entry(
                    &mut rows,
                    &mut actions,
                    "\u{2022}",
                    "reading harnesses...",
                    "",
                    false,
                    None,
                );
                return (rows, actions);
            };
            let harness = l.draft.harness();
            // The runtime capture wins when it holds rows for this harness:
            // fresh from the installed binary's own --help, descriptions
            // beside each flag. An empty capture (binary absent, help
            // unreadable) falls back to the static toml capture.
            let runtime = (l.runtime_flags_harness == harness && !l.runtime_flags.is_empty())
                .then_some(&l.runtime_flags);
            if runtime.is_none() {
                let flags = catalog_rows
                    .iter()
                    .find(|r| r.name == harness)
                    .and_then(|r| r.launch_flags.as_deref())
                    .unwrap_or(&[]);
                if flags.is_empty() {
                    push_entry(
                        &mut rows,
                        &mut actions,
                        "\u{2022}",
                        "no captured flags; type --flag value and Space",
                        "",
                        true,
                        None,
                    );
                }
                for entry in flags {
                    let (flag, takes_value) = split_flag_entry(entry);
                    push_entry(
                        &mut rows,
                        &mut actions,
                        "\u{2022}",
                        entry,
                        "",
                        true,
                        Some(PickerAction::AddPill {
                            flag,
                            picks_value: takes_value,
                        }),
                    );
                }
            }
            if let Some(runtime) = runtime {
                for (entry, desc) in runtime {
                    let (flag, takes_value) = split_flag_entry(entry);
                    push_entry(
                        &mut rows,
                        &mut actions,
                        "\u{2022}",
                        entry,
                        desc,
                        true,
                        Some(PickerAction::AddPill {
                            flag,
                            picks_value: takes_value,
                        }),
                    );
                }
            }
        }
        _ => {}
    }
    (rows, actions)
}

/// Split a capability launch_flags entry (`--agent <agent>`) into the flag
/// spelling and whether it takes a value.
fn split_flag_entry(entry: &str) -> (String, bool) {
    match entry.split_once(" <") {
        Some((flag, _)) => (flag.to_string(), true),
        None => (entry.to_string(), false),
    }
}

/// The flags picker's anchor: one row under the editor block's last pills
/// row.
fn pills_anchor(l: &Launcher, view: &View) -> Option<(u16, u16)> {
    let sl = l.sheet_layout(view)?;
    let last_pill_y = sl.pills.last().map_or(sl.pills_y, |r| r.y);
    Some((
        (sl.origin.0 as usize + 1 + last_pill_y as usize + 1) as u16,
        (sl.origin.1 as usize + 1) as u16,
    ))
}

/// Group model rows by provider under one key: the provider, or the
/// harness itself for rows with no provider. Insertion order kept.
fn group_by_provider<'a>(
    models: &'a [ModelChoice],
    harness: &str,
) -> Vec<(String, Vec<&'a ModelChoice>)> {
    let mut groups: Vec<(String, Vec<&'a ModelChoice>)> = Vec::new();
    for m in models {
        let key = m.provider.clone().unwrap_or_else(|| harness.to_string());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(m),
            None => groups.push((key, vec![m])),
        }
    }
    groups
}

/// One Model row's hint: the launch id when it differs from the label and
/// no route spells it, else the route; empty when the route restates it.
fn model_hint(m: &ModelChoice) -> String {
    if m.route.is_empty() && m.model != m.name {
        m.model.clone()
    } else if m.route == m.model {
        String::new()
    } else {
        m.route.clone()
    }
}

/// The more list's rows: the harness's catalog tail (`HarnessChoice.more`),
/// grouped by provider. Every row is enabled so the cursor lands on it.
pub(crate) fn more_rows(
    l: &Launcher,
    catalog: &Option<CatalogOutcome>,
) -> (Vec<PopupRow>, Vec<Option<PickerAction>>) {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<Option<PickerAction>> = Vec::new();
    let Some(CatalogOutcome::Ok(rows_found, _, _)) = catalog else {
        return (rows, actions);
    };
    let harness = l.draft.harness();
    let Some(row) = rows_found.iter().find(|row| row.name == harness) else {
        return (rows, actions);
    };
    let groups = group_by_provider(&row.more, &harness);
    for (key, list) in &groups {
        rows.push(PopupRow::Header(key.clone()));
        actions.push(None);
        for m in list.iter() {
            let check = Some(&m.name) == l.draft.model_row.as_ref()
                && !l.draft.model.is_empty()
                && m.provider.as_deref() == Some(l.draft.provider.as_str());
            let (glyph, hint, action) = match &m.state {
                ModelState::Ready => (
                    if check { "\u{2713}" } else { "\u{25cf}" },
                    model_hint(m),
                    Some(PickerAction::PickRow {
                        harness: harness.clone(),
                        name: m.name.clone(),
                        model: m.model.clone(),
                        route: m.route.clone(),
                        provider: m.provider.clone(),
                    }),
                ),
                ModelState::NoKey { key_env, steps } => (
                    "\u{25cb}",
                    key_env.clone(),
                    Some(PickerAction::ShowSteps {
                        title: format!("connect {key}"),
                        lines: steps.clone(),
                    }),
                ),
                ModelState::Unreachable { reason } => (
                    "\u{2013}",
                    reason.clone(),
                    Some(PickerAction::ShowSteps {
                        title: format!("{key} on {harness}"),
                        lines: vec![reason.clone()],
                    }),
                ),
            };
            push_entry(&mut rows, &mut actions, glyph, &m.name, &hint, true, action);
        }
    }
    (rows, actions)
}

/// The drill helpers: the Model picker's deeper views over the same anchor.
/// OpenMore rebuilds the picker over the catalog tail; Esc steps back down
/// the ladder. Each rebuild clears the filter and resets the selection.
pub(crate) fn open_more(l: &mut Launcher, catalog: &Option<CatalogOutcome>, anchor: Anchor) {
    let (all_rows, all_actions) = more_rows(l, catalog);
    let (popup, actions) = filtered_popup(Focus::Model, &all_rows, &all_actions, "", anchor);
    l.picker = Some(Picker {
        popup,
        actions,
        all_rows,
        all_actions,
        field: Focus::Model,
        anchor,
        filter: String::new(),
        mode: PickerMode::More,
    });
}

/// A NoKey or Unreachable more-row's Enter: the picker becomes the steps
/// sheet, one header plus one disabled row per line, footer `esc back`.
pub(crate) fn show_steps(l: &mut Launcher, title: String, lines: Vec<String>, anchor: Anchor) {
    let mut all_rows = vec![PopupRow::Header(title.clone())];
    let mut all_actions: Vec<Option<PickerAction>> = vec![None];
    for line in &lines {
        push_entry(
            &mut all_rows,
            &mut all_actions,
            "\u{2022}",
            line,
            "",
            false,
            None,
        );
    }
    let (mut popup, actions) = filtered_popup(Focus::Model, &all_rows, &all_actions, "", anchor);
    popup = popup.footer("esc back");
    l.picker = Some(Picker {
        popup,
        actions,
        all_rows,
        all_actions,
        field: Focus::Model,
        anchor,
        filter: String::new(),
        mode: PickerMode::Steps { title },
    });
}

/// The composer help sheet `?` opens on an empty input: the key grammar and
/// the gestures, one line each. Esc closes; nothing here is selectable.
pub(crate) fn open_help(l: &mut Launcher, anchor: Anchor) {
    let lines = [
        "enter: launch \u{b7} ctrl+j: launch from any chip \u{b7} shift+enter: force launch",
        "--: flag picker \u{b7} type --flag value + space: free-hand pill",
        "--force: force past admission and the spawn gate (journaled)",
        "@: pick a backlog node into the prompt",
        "!: run a shell line in a pane",
        "ctrl+u or cmd+backspace: clear left of the cursor",
        "option+backspace: delete the word left of the cursor",
        "option+left / option+right: move by word",
        "cmd+left / cmd+right: jump to the line start / end",
        "ctrl+o: show a refusal's raw text",
        "tab: next chip \u{b7} esc: close",
    ];
    let mut all_rows = vec![PopupRow::Header("composer help".to_string())];
    let mut all_actions: Vec<Option<PickerAction>> = vec![None];
    for line in lines {
        push_entry(
            &mut all_rows,
            &mut all_actions,
            "\u{2022}",
            line,
            "",
            false,
            None,
        );
    }
    let (mut popup, actions) = filtered_popup(Focus::Model, &all_rows, &all_actions, "", anchor);
    popup = popup.footer("esc back");
    l.picker = Some(Picker {
        popup,
        actions,
        all_rows,
        all_actions,
        field: Focus::Model,
        anchor,
        filter: String::new(),
        mode: PickerMode::Help,
    });
}

/// Esc in a drilled view: More -> Main, the list the view came from, at
/// the same anchor.
pub(crate) fn picker_step_down(
    l: &mut Launcher,
    catalog: &Option<CatalogOutcome>,
    backlog: &[crate::proto::BacklogCard],
    mut picker: Picker,
) {
    let mode = match picker.mode {
        PickerMode::More => PickerMode::Main,
        PickerMode::Main => {
            l.picker = Some(picker);
            return;
        }
        PickerMode::Steps { .. } => PickerMode::More,
        // The help sheet never drills down to a row list; Esc closes it at
        // the key arm, so a Help picker only reaches here defensively.
        PickerMode::Help => {
            l.picker = Some(picker);
            return;
        }
    };
    let (all_rows, all_actions) = match mode {
        PickerMode::More => more_rows(l, catalog),
        _ => picker_rows(l, picker.field, catalog, backlog),
    };
    let (popup, actions) = filtered_popup(picker.field, &all_rows, &all_actions, "", picker.anchor);
    picker.mode = mode;
    picker.all_rows = all_rows;
    picker.all_actions = all_actions;
    picker.popup = popup;
    picker.popup.sel = 0;
    picker.actions = actions;
    picker.filter.clear();
    l.picker = Some(picker);
}

/// Commit a picked row. `portal` was resolved before the picker borrow; a
/// stale index costs one verbatim door refusal, never a wrong lane. `field`
/// is the picker's axis, so a commit lands on the axis it was picked from
/// whatever holds sheet focus.
pub(crate) fn apply_picker_action(
    l: &mut Launcher,
    catalog: &Option<CatalogOutcome>,
    action: PickerAction,
    portal: u8,
    field: Focus,
) {
    l.picker = None;
    match action {
        // The force arm commits in commit_picker_action; nothing to place here.
        PickerAction::Force => {}
        PickerAction::Set(name) => match field {
            Focus::Permission => {
                l.draft.permission = name;
                l.draft.bump();
            }
            Focus::Effort => {
                l.draft.effort = name;
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
            l.draft.route = route.clone();
            l.draft.provider = provider.unwrap_or_default();
            let recent_model = ModelChoice {
                name: name.clone(),
                model: l.draft.model.clone(),
                route,
                provider: non_empty(&l.draft.provider),
                state: ModelState::Ready,
                key_env: None,
                key_file: None,
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
        PickerAction::ClearModel => {
            l.draft.model.clear();
            l.draft.model_row = None;
            l.draft.route.clear();
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
                l.draft.route.clear();
                clear_unoffered_pins(&mut l.draft, catalog);
            }
        }
        PickerAction::SetProject(i) => {
            if i < l.draft.projects.len() {
                l.draft.project_idx = i;
                l.draft.bump();
            }
        }
        PickerAction::SetBranch(branch) => {
            // The composer never moves the project checkout's branch in
            // place: a pick other than the current branch is a worktree
            // branch, so the box turns on with it.
            let current = l.draft.facts(catalog).and_then(|f| f.current.clone());
            l.draft.branch = Some(branch.clone());
            if current.as_deref() != Some(branch.as_str()) {
                l.draft.worktree = Some(true);
            }
            l.draft.bump();
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
            // The "<harness> decides" row of the mode or effort picker: the
            // pin clears and the harness default takes over.
            match field {
                Focus::Permission => {
                    l.draft.permission.clear();
                    l.draft.bump();
                }
                Focus::Effort => {
                    l.draft.effort.clear();
                    l.draft.bump();
                }
                _ => {}
            }
        }
        PickerAction::TypeIn => {
            // Permission keeps free text only where the capability table
            // declares an empty choice list; the pin clears and the next
            // typed chars are the value.
            if field == Focus::Permission {
                l.draft.permission.clear();
                l.draft.bump();
            }
        }
        PickerAction::AddPill { flag, picks_value } => {
            // A flag pick removes the typed `--word` from the message and
            // adds the pill; a value-taking flag opens value capture. The
            // flag spelling loses any ` <value>` suffix the picker showed.
            commit_typed_flag_word(l);
            add_pill(l, flag, picks_value);
        }
        // Drilled back into by commit_picker_action; a direct commit here
        // (a stale row action) should be unreachable from a fresh picker.
        PickerAction::OpenMore => {}
        PickerAction::ShowSteps { .. } => {}
    }
}

/// Commit a picked row, drilling first: OpenMore rebuilds the picker over
/// the catalog tail at the same anchor; every other action commits through
/// apply_picker_action.
fn commit_picker_action(
    l: &mut Launcher,
    catalog: &Option<CatalogOutcome>,
    action: PickerAction,
    portal: u8,
    field: Focus,
    anchor: Anchor,
) {
    match action {
        PickerAction::OpenMore => open_more(l, catalog, anchor),
        PickerAction::ShowSteps { title, lines } => show_steps(l, title, lines, anchor),
        PickerAction::Force => {
            // The pill is the armed state: visible, removable, and it
            // survives a refusal. Picking twice stays one arm.
            if !l.draft.pills.iter().any(|(f, _)| f == "--force") {
                l.draft.pills.push(("--force".to_string(), None));
                l.draft.bump();
            }
        }
        action => apply_picker_action(l, catalog, action, portal, field),
    }
}

// -- render ------------------------------------------------------------------

/// The model chip shows the selected configured row or explicit model id.
/// The effort pin rides the effort chip now, never the model chip.
fn model_label(d: &LaunchDraft) -> String {
    d.model_row
        .clone()
        .or_else(|| non_empty(&d.model))
        .unwrap_or_else(|| "default".to_string())
}

/// Where the facts value starts, past its label.
const FACTS_VALUE_X: usize = 18;

/// The keybar wrapped to `w` columns, and where its trailing esc word
/// (`esc close` / `esc cancel`) sits as `(row, col, width)`. The esc word
/// never splits across rows.
fn keybar_lines(kb: &str, w: usize) -> (Vec<String>, Option<(usize, usize, usize)>) {
    let esc = ["esc close", "esc cancel"]
        .into_iter()
        .find(|e| kb.ends_with(e));
    let head = esc.map_or(kb, |e| kb[..kb.len() - e.len()].trim_end());
    let mut lines = Vec::new();
    if !head.is_empty() {
        crate::client::wrap_line(head, w, &mut lines);
    }
    let Some(e) = esc else {
        return (lines, None);
    };
    let last_w = lines.last().map_or(0, |l| label_width(l) as usize);
    if !lines.is_empty() && last_w + 1 + e.len() <= w {
        let row = lines.len() - 1;
        lines[row].push(' ');
        lines[row].push_str(e);
        (lines, Some((row, last_w + 1, e.len())))
    } else {
        lines.push(e.to_string());
        let row = lines.len() - 1;
        (lines, Some((row, 0, e.len())))
    }
}

impl Launcher {
    /// The project's launch facts, for the facts line under Branch focus.
    fn branch_facts(&self, view: &View) -> String {
        match self
            .draft
            .facts(&view.launcher_catalog)
            .map(|f| (f.policy.as_deref(), f.current.as_deref()))
        {
            Some((Ok("never"), _)) => "policy never: runs in place".to_string(),
            Some((Ok(word), Some(current))) => format!("policy {word} \u{b7} branch {current}"),
            Some((Ok(word), None)) => format!("policy {word} \u{b7} branch ?"),
            Some((Err(e), _)) => format!("policy unread: {e}"),
            None => "reading project facts...".to_string(),
        }
    }

    /// One pill's text: the flag and its value, or the typed value draft
    /// while capture holds on it.
    fn pill_spell(&self, idx: usize) -> String {
        let (flag, value) = &self.draft.pills[idx];
        if self.capturing_pill_index() == Some(idx) {
            if self.draft.pill_value_draft.is_empty() {
                format!("{flag} <value>")
            } else {
                format!("{flag} {}", self.draft.pill_value_draft)
            }
        } else {
            match value {
                Some(v) => format!("{flag} {v}"),
                None => flag.clone(),
            }
        }
    }

    /// The pills flowed onto as many rows as they need: each pill's text
    /// rows, each pill's rect on its last row (text plus the x cell), and
    /// the row count. A pill wider than the row wraps onto rows of its own.
    #[allow(clippy::type_complexity)]
    fn pill_rows(&self, w: usize) -> (Vec<(u16, u16, String)>, Vec<RtRect>, usize) {
        let mut text = Vec::new();
        let mut rects = Vec::new();
        let (mut x, mut row) = (0usize, 0usize);
        for idx in 0..self.draft.pills.len() {
            let chunks: Vec<String> =
                wrap_message(&self.pill_spell(idx), w.saturating_sub(2).max(1))
                    .into_iter()
                    .map(|(_, c)| c)
                    .collect();
            let last_w = chunks.last().map_or(0, |c| label_width(c) as usize) + 2;
            if x > 0 && (chunks.len() > 1 || x + last_w > w) {
                row += 1;
                x = 0;
            }
            for (k, c) in chunks.into_iter().enumerate() {
                if k > 0 {
                    row += 1;
                }
                text.push((x as u16, row as u16, c));
            }
            rects.push(RtRect::new(x as u16, row as u16, last_w.min(w) as u16, 1));
            x += last_w + 2;
        }
        (text, rects, row + 1)
    }
}

impl Launcher {
    /// The pill index whose value capture holds, `None` when none does. A
    /// chip-owned pin captures onto the chip, never a pill.
    fn capturing_pill_index(&self) -> Option<usize> {
        if self.draft.pill_value_capture && self.pending_chip_pin.is_none() {
            self.draft.pills.len().checked_sub(1)
        } else {
            None
        }
    }

    /// The lifecycle line: refusal reasons, unknown-evidence, seed doubt,
    /// or blank in Editing (the hint row above carries the key rule). A
    /// refusal shows its one-sentence head; Ctrl+O swaps in the raw text.
    pub(crate) fn footer(&self) -> String {
        match &self.phase {
            Phase::Editing => String::new(),
            Phase::Submitting { .. } => "starting...".to_string(),
            Phase::Refused { reason, .. } => format!("refused: {}", self.detail_text(reason)),
            Phase::Unknown { reason, .. } => {
                format!("outcome unknown: {}", self.detail_text(reason))
            }
            Phase::Launched {
                name, seed_note, ..
            } => {
                let note = seed_note.map(|n| format!(" ({n})")).unwrap_or_default();
                format!("launched {name}{note}")
            }
        }
    }

    /// The refusal text the footer shows: the raw reason under Ctrl+O,
    /// otherwise its first sentence. Gate output is multi-line and
    /// machine-shaped; the head names the limit, the detail keeps the
    /// evidence and the remedy the gate printed.
    fn detail_text(&self, reason: &str) -> String {
        if self.show_detail {
            return reason.to_string();
        }
        let short = first_sentence(reason);
        if short == reason {
            short
        } else {
            format!("{short} (^o raw)")
        }
    }

    /// One chip's label: the axis's current VALUE, never the axis name.
    pub(crate) fn chip_label(&self, f: Focus, catalog: &Option<CatalogOutcome>) -> String {
        let d = &self.draft;
        match f {
            Focus::Where => {
                if d.placement == Placement::default() {
                    "Local".to_string()
                } else {
                    format!("Local \u{b7} {}", d.placement.label())
                }
            }
            Focus::Project => d
                .cwd()
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or("directory")
                .to_string(),
            Focus::Branch => match d.worktree_state(catalog) {
                Some(true) => d.branch.clone().unwrap_or_else(|| "main".to_string()),
                Some(false) => d
                    .facts(catalog)
                    .and_then(|f| f.current.clone())
                    .unwrap_or_else(|| "?".to_string()),
                None => "branch ?".to_string(),
            },
            Focus::Worktree => match d.worktree_state(catalog) {
                Some(true) => "[x] worktree".to_string(),
                Some(false) => "[ ] worktree".to_string(),
                None => "worktree ?".to_string(),
            },
            Focus::Plus => "+".to_string(),
            Focus::Permission => {
                if d.permission.is_empty() {
                    "auto".to_string()
                } else {
                    d.permission.clone()
                }
            }
            Focus::Harness => non_empty(&d.harness()).unwrap_or_else(|| "harness".to_string()),
            Focus::Model => model_label(d),
            Focus::Effort => {
                if d.effort.is_empty() {
                    "default".to_string()
                } else {
                    d.effort.clone()
                }
            }
            _ => String::new(),
        }
    }
}

/// The first sentence of a refusal: the first non-empty line, cut at a
/// sentence end or a `;` past ten chars so the head stays one readable
/// clause. The raw text stays behind Ctrl+O and in the journal either way.
pub(crate) fn first_sentence(reason: &str) -> String {
    // An admission refusal is machine-shaped (`process admission refused:
    // count=unknown, ceiling=...`): the head is a canned line naming the
    // limit and the way out, never a cut-off mid-word string.
    if reason.contains("process admission refused") {
        if reason.contains("runaway") {
            return "process limit: machine runaway brake on; run it in a terminal or wait for the all-clear".into();
        }
        if reason.contains("count=unknown") || reason.contains("not measuring") {
            return "process limit: fno cannot read the machine's load (count=unknown); wait a beat or run it in a terminal".into();
        }
        return "process limit reached; wait for load to drop or run it in a terminal".into();
    }
    let line = reason
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.len() > 24 {
        let head = line
            .char_indices()
            .nth(10)
            .map(|(b, _)| b)
            .unwrap_or(line.len());
        if let Some(at) = line[head..].find(". ").or_else(|| line[head..].find("; ")) {
            return line[..head + at + 1].to_string();
        }
    }
    line.to_string()
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
    /// One rect per painted chip, in paint order. `Message` and the flags
    /// editor paint no chip.
    pub chips: Vec<(Focus, RtRect)>,
    /// The cwd facts line above the chips (reserved rows, painted only while
    /// Project holds focus or the mouse). Its height fits the longer of the
    /// two facts texts wrapped, so moving focus never resizes the sheet.
    pub cwd_line: RtRect,
    /// The editor window (the message, or the flags editor focused in its
    /// place) in body coords.
    pub message: RtRect,
    pub start_chunk: usize,
    pub editor_rows: usize,
    /// The pills row: one rect per pill (flag + value + the x gutter), for
    /// the mouse's remove hit test. Empty when no pill paints.
    pub pills: Vec<RtRect>,
    /// The pills row's y (painted only when a pill or value capture shows).
    pub pills_y: u16,
    /// The keybar's first body row (right under the bottom chip row).
    pub keybar_y: u16,
    /// The keybar wrapped to the sheet, one entry per row.
    pub keybar: Vec<String>,
    /// Each pill's text rows: `(x, y, text)` in body coords.
    pub pill_text: Vec<(u16, u16, String)>,
    /// The [cancel] footer rect while an attempt is pending.
    pub cancel: Option<RtRect>,
    /// The keybar's trailing `esc close` / `esc cancel` word: a clickable
    /// close affordance, the same gesture as the Esc key.
    pub esc_rect: Option<RtRect>,
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
        // Fixed rows: the cwd facts line, the top chips, a blank row, the
        // editor (1..=6 rows), a blank row, the bottom chip rows (1, or 2
        // when the right group wraps), the keybar, the lifecycle line.
        // Top row: `Where Project` with the `Branch` + worktree pair beside
        // them; when the pair cannot share the row it wraps to its own -
        // a chip value is never truncated to make the row.
        // Bottom row split: `+ mode` left; harness, model and effort
        // right-aligned. When the two groups cannot share one row the right
        // group wraps to its own row - a chip value is never truncated to
        // make the row.
        let top: Vec<Focus> = vec![Focus::Where, Focus::Project];
        let branch: Vec<Focus> = if branch_offered(self, &view.launcher_catalog) {
            vec![Focus::Branch, Focus::Worktree]
        } else {
            Vec::new()
        };
        let left: Vec<Focus> = vec![Focus::Permission];
        let right: Vec<Focus> = match Focus::tab_order(self, &view.launcher_catalog).last() {
            Some(Focus::Effort) => vec![Focus::Harness, Focus::Model, Focus::Effort],
            _ => vec![Focus::Harness, Focus::Model],
        };
        let catalog = &view.launcher_catalog;
        let chip_w = |f: Focus| -> usize {
            if f == Focus::Worktree {
                // The checkbox paints no caret: label + one trailing pad.
                return label_width(&self.chip_label(f, catalog)) as usize + 1;
            }
            label_width(&self.chip_label(f, catalog)) as usize + 3 // text + caret + padding
        };
        let top_w: usize = top.iter().map(|f| chip_w(*f)).sum::<usize>() + (top.len() - 1) * 2;
        let branch_w: usize =
            branch.iter().map(|f| chip_w(*f)).sum::<usize>() + branch.len().saturating_sub(1) * 2;
        let left_w: usize = left.iter().map(|f| chip_w(*f)).sum::<usize>() + (left.len() - 1) * 2;
        let right_w: usize =
            right.iter().map(|f| chip_w(*f)).sum::<usize>() + (right.len() - 1) * 2;
        let facts_w = inner_w.saturating_sub(FACTS_VALUE_X).max(1);
        let facts_rows = [self.branch_facts(view), self.draft.cwd()]
            .iter()
            .map(|t| wrap_message(t, facts_w).len())
            .max()
            .unwrap_or(1)
            .max(1);
        let (keybar, esc_at) = keybar_lines(&self.keybar(), inner_w);
        let top_rows = if branch_w == 0 || top_w + 2 + branch_w <= inner_w {
            1
        } else {
            2
        };
        let bottom_rows = if left_w + 2 + right_w <= inner_w {
            1
        } else {
            2
        };
        // The editor gets the leftover height, capped at 6 wrapped rows.
        let pills_row = !self.draft.pills.is_empty() || self.draft.pill_value_capture;
        let (mut pill_text, mut pill_rects, pill_rows) = if pills_row {
            self.pill_rows(inner_w)
        } else {
            (Vec::new(), Vec::new(), 0)
        };
        // top chips, 2 blanks, pills rows, bottom chips, keybar, footer
        let rest = top_rows + 1 + 1 + pill_rows + bottom_rows + keybar.len() + 1;
        // The facts rows give way first on a short terminal, so the keybar and
        // its esc word never fall off the bottom; one row and one editor row
        // always stay.
        let facts_rows = facts_rows.min(rows.saturating_sub(2 + rest + 1).max(1));
        let other = facts_rows + rest;
        let editor_rows = (rows.saturating_sub(2 + other)).clamp(1, 6);
        let framed_h = 2 + other + editor_rows;
        let origin = (
            ((rows.saturating_sub(framed_h)) / 2) as u16,
            ((cols.saturating_sub(framed_w)) / 2) as u16,
        );
        // Top chips at body row 1 (row 0 is the cwd line). A pathological
        // narrow sheet clamps each chip to the row: the value truncates
        // (paint_chip ellipsizes) rather than overflowing the buffer.
        let mut chips: Vec<(Focus, RtRect)> = Vec::new();
        let mut push_row = |row: usize, start: usize, group: &[Focus]| {
            let mut x = start;
            for f in group {
                let w = chip_w(*f).min(inner_w.saturating_sub(x)).max(1);
                chips.push((*f, RtRect::new(x as u16, row as u16, w as u16, 1)));
                x = (x + w + 2).min(inner_w);
            }
        };
        push_row(facts_rows, 0, &top);
        if !branch.is_empty() {
            let row = facts_rows + top_rows - 1;
            push_row(row, inner_w.saturating_sub(branch_w), &branch);
        }
        // Bottom chips (the pills rows sit between editor and blank).
        let pills_y = facts_rows + 2 + top_rows + editor_rows;
        let bottom_y = pills_y + pill_rows;
        for (_, y, _) in &mut pill_text {
            *y += pills_y as u16;
        }
        for r in &mut pill_rects {
            r.y += pills_y as u16;
        }
        if bottom_rows == 1 {
            push_row(bottom_y, 0, &left);
            push_row(bottom_y, inner_w.saturating_sub(right_w), &right);
        } else {
            push_row(bottom_y, 0, &left);
            push_row(bottom_y + 1, inner_w.saturating_sub(right_w), &right);
        }
        // The editor window follows the cursor (1..=6 rows).
        let wrap_w = inner_w.saturating_sub(PROMPT_GUTTER);
        let chunks = wrap_message(&self.draft.message, wrap_w);
        let (cur_row, _) = wrapped_cursor(&self.draft.message, self.draft.cursor_chars, wrap_w);
        let mut start_chunk = chunks.len().saturating_sub(editor_rows);
        if cur_row < start_chunk {
            start_chunk = cur_row;
        } else if cur_row >= start_chunk + editor_rows {
            start_chunk = cur_row + 1 - editor_rows;
        }
        // The [cancel] item rides the keybar row while an attempt is pending.
        let pending = matches!(self.phase, Phase::Unknown { .. } | Phase::Submitting { .. });
        let keybar_y = bottom_y + bottom_rows;
        let cancel = pending.then(|| RtRect::new(0, keybar_y as u16, 8, 1));
        // The keybar's trailing esc word is the chip: "esc close" at rest,
        // "esc cancel" while an attempt is pending. Either way the click is
        // the Esc key's gesture.
        let esc_rect =
            esc_at.map(|(row, x, w)| RtRect::new(x as u16, (keybar_y + row) as u16, w as u16, 1));
        Some(SheetLayout {
            origin,
            framed_w,
            framed_h,
            chips,
            cwd_line: RtRect::new(0, 0, inner_w as u16, facts_rows as u16),
            message: RtRect::new(
                0,
                (facts_rows + 1 + top_rows) as u16,
                inner_w as u16,
                editor_rows as u16,
            ),
            start_chunk,
            editor_rows,
            pills: pill_rects,
            pills_y: pills_y as u16,
            keybar_y: keybar_y as u16,
            keybar,
            pill_text,
            cancel,
            esc_rect,
        })
    }

    /// The editor's cursor cell in SCREEN coordinates (row, col): the one
    /// place the terminal's real cursor sits while the sheet is open. The
    /// terminal renders it in its own configured cursor color, so the
    /// cursor follows the terminal theme on dark and light grounds alike.
    /// `None` = the sheet did not lay out.
    pub(crate) fn editor_cursor_cell(&self, sl: &SheetLayout) -> Option<(u16, u16)> {
        let wrap_w = sl.framed_w.saturating_sub(2).saturating_sub(PROMPT_GUTTER);
        let chunks = wrap_message(&self.draft.message, wrap_w);
        let (cur_row, cur_col) =
            wrapped_cursor(&self.draft.message, self.draft.cursor_chars, wrap_w);
        let (_, text) = chunks.get(cur_row)?;
        let disp_col: usize = text
            .chars()
            .take(cur_col)
            .map(|c| usize::from(UnicodeWidthChar::width(c).unwrap_or(0)))
            .sum();
        let x = sl.message.x + PROMPT_GUTTER as u16 + disp_col as u16;
        let in_sheet = cur_row >= sl.start_chunk && x < sl.message.x + sl.message.width;
        in_sheet.then(|| {
            let y = sl.message.y + (cur_row - sl.start_chunk) as u16;
            (y, x)
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
        // The cwd facts line (row 0, reserved): the label bold, the value
        // regular. It shows while Project holds focus or the mouse;
        // Branch/Worktree focus shows the project's launch facts instead.
        let facts = if self.focus == Focus::Branch || self.focus == Focus::Worktree {
            Some(("Branch", self.branch_facts(view)))
        } else if self.focus == Focus::Project || self.project_hover {
            Some(("Working directory", self.draft.cwd()))
        } else {
            None
        };
        if let Some((label, text)) = facts {
            buf.set_string(
                sl.cwd_line.x,
                sl.cwd_line.y,
                label,
                role_style(Role::PanelHead, &view.theme),
            );
            let facts_w = inner_w.saturating_sub(FACTS_VALUE_X).max(1);
            for (i, (_, line)) in wrap_message(&text, facts_w)
                .into_iter()
                .take(usize::from(sl.cwd_line.height))
                .enumerate()
            {
                buf.set_string(
                    sl.cwd_line.x + FACTS_VALUE_X as u16,
                    sl.cwd_line.y + i as u16,
                    line,
                    role_style(Role::PanelBody, &view.theme),
                );
            }
        }
        // The chip row(s): the focused chip is the one filled chip; the
        // caret rides the dim caret role. The worktree box greys out under a
        // `never` policy and paints no caret: there is nothing to drop.
        for (f, r) in &sl.chips {
            let never = *f == Focus::Worktree && self.draft.policy_never(&view.launcher_catalog);
            // Tab must SHOW where it landed: the focused chip carries the
            // filled inverse block (the accent text alone read at the same
            // weight as every other chip), the `never` box
            // included - focus names where the keyboard sits, not what the
            // box will do (Enter still refuses there).
            // An UNfocused `never` box is the disabled grammar: dim.
            let style = if *f == self.focus {
                role_style(Role::ChipFocus, &view.theme)
            } else if never {
                role_style(Role::BodyDim, &view.theme)
            } else {
                role_style(Role::PanelBody, &view.theme)
            };
            paint_chip(
                &mut buf,
                *r,
                &self.chip_label(*f, &view.launcher_catalog),
                style,
                false,
            );
            if *f != Focus::Plus && *f != Focus::Worktree {
                let caret_x = r.x + r.width - 1;
                buf[(caret_x, r.y)].set_char('\u{25be}');
                buf[(caret_x, r.y)].set_style(role_style(Role::PanelMeta, &view.theme));
            }
        }
        // The editor: prompt gutter, wrapped rows, and on an empty draft the
        // dim placeholder naming the shape. No painted cursor glyph: the
        // terminal's REAL cursor marks the insert point (it is routed to
        // [`Self::editor_cursor_cell`]), so its color follows the terminal
        // theme on every ground. The flags editor takes the same rows while
        // it holds the keyboard.
        let wrap_w = inner_w.saturating_sub(PROMPT_GUTTER);
        let chunks = wrap_message(&self.draft.message, wrap_w);
        for k in 0..sl.editor_rows {
            let Some((_, text)) = chunks.get(sl.start_chunk + k) else {
                break;
            };
            let y = sl.message.y + k as u16;
            buf.set_string(sl.message.x + PROMPT_GUTTER as u16, y, text, RtStyle::new());
        }
        if sl.editor_rows > 0 {
            // Shell mode paints its own glyph in the accent role so the
            // mode is visible before the first keystroke lands.
            let (glyph, glyph_role) = if self.shell {
                ("! ", Role::BodyAccent)
            } else {
                ("\u{276f} ", Role::BodyDim)
            };
            buf.set_string(
                sl.message.x,
                sl.message.y,
                glyph,
                role_style(glyph_role, &view.theme),
            );
            if self.draft.message.is_empty() {
                let placeholder = if self.shell {
                    format!("shell command in {}", self.draft.cwd())
                } else {
                    "prompt \u{b7} -- flags \u{b7} @ node \u{b7} ! shell \u{b7} ? help".to_string()
                };
                buf.set_string(
                    sl.message.x + PROMPT_GUTTER as u16,
                    sl.message.y,
                    placeholder,
                    role_style(Role::PanelMeta, &view.theme),
                );
            }
        }
        // The pills row, between the input and the blank row above the
        // bottom chips: one row, horizontally laid out; each pill keeps its
        // own rect for the x hit test. While value capture holds, the
        // capturing pill paints the typed value draft.
        for (x, y, text) in &sl.pill_text {
            buf.set_string(*x, *y, text, role_style(Role::Chip, &view.theme));
        }
        for r in &sl.pills {
            buf.set_string(
                r.x + r.width - 1,
                r.y,
                "\u{00d7}",
                role_style(Role::PanelMeta, &view.theme),
            );
        }
        // Keybar row: [cancel] while pending, then the key rule; the
        // lifecycle line under it, dim.
        let keybar_y = sl.keybar_y;
        if let Some(cr) = sl.cancel {
            paint_chip(
                &mut buf,
                cr,
                "[cancel]",
                role_style(Role::Chip, &view.theme),
                false,
            );
        }
        // The esc word paints with the chip style so it reads as the same
        // affordance the modal borders carry; the click target sits under it.
        let last_row = (body_h as u16).saturating_sub(1);
        for (i, line) in sl.keybar.iter().enumerate() {
            let y = (keybar_y + i as u16).min(last_row);
            let esc = sl.esc_rect.filter(|r| r.y == keybar_y + i as u16);
            let split = esc.map_or(line.len(), |r| {
                line.char_indices()
                    .nth(r.x as usize)
                    .map_or(line.len(), |(b, _)| b)
            });
            buf.set_string(0, y, &line[..split], RtStyle::new());
            if let Some(r) = esc {
                buf.set_string(r.x, y, &line[split..], role_style(Role::Chip, &view.theme));
            }
        }
        // The lifecycle line wraps inside the sheet instead of clipping at
        // the width: a refusal reason cut mid-sentence hid its own remedy.
        // One row is the floor: a too-short terminal still shows the line's
        // head, as the single clipped row always did.
        let footer_y = usize::from(keybar_y) + sl.keybar.len();
        let footer_rows = body_h.saturating_sub(footer_y).max(1);
        for (i, (_, line)) in wrap_message(&self.footer(), inner_w)
            .into_iter()
            .take(footer_rows)
            .enumerate()
        {
            buf.set_string(
                0,
                u16::try_from(footer_y + i).unwrap_or(u16::MAX),
                line,
                RtStyle::new().add_modifier(Modifier::DIM),
            );
        }
        crate::ratatui_blit::blit_area(&buf, oy, ox, cells, cols);
    }
}

/// The footer key rule, per state: the whole grammar, one line, never
/// replaced by the lifecycle line under it.
impl Launcher {
    fn keybar(&self) -> String {
        if matches!(self.phase, Phase::Unknown { .. } | Phase::Submitting { .. }) {
            return "^o detail \u{b7} esc cancel".to_string();
        }
        if matches!(self.phase, Phase::Refused { .. }) {
            return "^o detail \u{b7} esc close".to_string();
        }
        // Shell mode runs instead of launching: the bar names its own keys.
        if self.shell {
            return "\u{21b5} run \u{b7} \u{232b} back to \u{276f} \u{b7} ^j newline \u{b7} esc close"
                .to_string();
        }
        // One grammar for the whole chip row: Tab moves, Enter opens the
        // focused chip's picker or launches from the input - the hint names
        // which, so the footer never advertises a generic "open/launch".
        // ^j is a newline in the input and the launch-from-anywhere key from
        // a chip.
        let enter = if self.focus == Focus::Message {
            "\u{21b5} launch".to_string()
        } else if self.focus == Focus::Worktree {
            "\u{21b5} toggle worktree".to_string()
        } else {
            format!("\u{21b5} open {}", self.focus.name())
        };
        let ctrl_j = if self.focus == Focus::Message {
            "^j newline"
        } else {
            "^j launch"
        };
        format!("{enter} \u{b7} tab next \u{b7} {ctrl_j} \u{b7} esc close")
    }
}

/// A launch is offered while the sheet edits: a pending Submitting or an
/// Unknown attempt must be cancelled explicitly (Esc) before a new one arms.
fn launcher_can_launch(view: &View) -> bool {
    view.launcher
        .as_ref()
        .is_some_and(|l| !matches!(l.phase, Phase::Submitting { .. } | Phase::Unknown { .. }))
}

/// The one draw arm the client's overlay chain calls: the sheet, then the
/// open popover (a chip picker, or the `@` node picker), in that order.
/// Returns the editor cursor's screen cell when the sheet drew AND no picker
/// holds the keyboard: the one place the terminal's real cursor may sit
/// (`None` otherwise, including the closed launcher).
pub(crate) fn draw_overlay(
    view: &View,
    cells: &mut [crate::proto::Cell],
    rows: usize,
    cols: usize,
) -> Option<(u16, u16)> {
    let Some(l) = view.launcher.as_ref() else {
        return None;
    };
    let mut cursor = None;
    if let Some(sl) = l.sheet_layout(view) {
        l.paint_sheet(view, cells, rows, cols, &sl);
        cursor = l.editor_cursor_cell(&sl);
    }
    if let Some(pk) = l.picker.as_ref() {
        // A chip picker's rows are a snapshot: when the axis's row source
        // moved under it (a catalog read landing mid-picker), draw a popup
        // rebuilt from the FRESH rows instead. No mutation, so the compose
        // path stays immutable; the stored picker updates on the next key
        // or click through rebuild_picker.
        let draw_pk: Picker = if pk.field != Focus::Message {
            let (fresh_rows, fresh_actions) =
                picker_rows(l, pk.field, &view.launcher_catalog, &view.backlog);
            if fresh_rows != pk.all_rows {
                let mut updated = pk.clone();
                updated.all_rows = fresh_rows;
                updated.all_actions = fresh_actions;
                let (popup, actions) = filtered_popup(
                    updated.field,
                    &updated.all_rows,
                    &updated.all_actions,
                    &updated.filter,
                    updated.anchor,
                );
                updated.popup = popup;
                updated.actions = actions;
                updated
            } else {
                pk.clone()
            }
        } else {
            pk.clone()
        };
        crate::popup::draw(
            cells,
            rows,
            cols,
            &draw_pk.popup.render(view.term),
            &view.theme,
        );
        // A picker holds the keyboard: the terminal cursor hides while it
        // is open (nothing in the sheet is taking characters).
        cursor = None;
    }
    cursor
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
    // dismisses the picker; any other report outside falls through. A stale
    // snapshot refreshes first, so a click commits through the rows painted.
    refresh_stale_picker(view);
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
        // A field-disjoint snapshot for the commit path.
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
                    let field = picker.field;
                    picker.popup.select(target);
                    let action = picker
                        .popup
                        .targets()
                        .get(target)
                        .and_then(|(row, _)| picker.actions.get(*row))
                        .cloned()
                        .flatten();
                    let anchor = picker.anchor;
                    l.picker = Some(picker);
                    if let Some(action) = action {
                        commit_picker_action(l, &catalog, action, portal, field, anchor);
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
    let hit_chip = sl.chips.iter().find(|(_, r)| hit(*r)).map(|(f, _)| *f);
    let hit_message = hit(sl.message);
    let hit_cancel = sl.cancel.is_some_and(|r| hit(r));
    let hit_esc = sl.esc_rect.is_some_and(|r| hit(r));
    if let MouseKind::Move = rep.kind {
        // Motion over the Project chip shows the cwd facts line; motion
        // anywhere else over the sheet is consumed silently.
        if let Some(l) = view.launcher.as_mut() {
            l.project_hover = hit_chip == Some(Focus::Project);
        }
        return Ok(true);
    }
    if !matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
        return Ok(true);
    }
    if hit_esc {
        // The esc word is the key, by mouse: cancel a pending attempt, else
        // close (hide + retain) - the same gesture as the Esc key.
        let pending = view
            .launcher
            .as_ref()
            .is_some_and(|l| matches!(l.phase, Phase::Unknown { .. } | Phase::Submitting { .. }));
        if pending {
            if let Some(l) = view.launcher.as_mut() {
                l.phase = Phase::Editing;
                l.armed = None;
            }
        } else {
            close(view);
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
    // A pill's x gutter removes the pill: flag + value + one trailing cell,
    // the same geometry paint and the layout rects keep.
    let hit_pill = sl.pills.iter().position(|r| hit(*r));
    if let Some(idx) = hit_pill {
        // Only the trailing cell (the x glyph) removes; a click on the pill
        // body is consumed silently.
        let r = sl.pills[idx];
        let x_hit = col == ox + (r.x + r.width).saturating_sub(1) as usize;
        if x_hit {
            if let Some(l) = view.launcher.as_mut() {
                l.draft.pills.remove(idx);
                l.draft.bump();
            }
            return Ok(true);
        }
    }
    if hit_message {
        // The obvious gesture: a press on the input focuses it.
        if let Some(l) = view.launcher.as_mut() {
            l.focus = Focus::Message;
        }
        return Ok(true);
    }
    let Some(field) = hit_chip else {
        return Ok(true);
    };
    // A press on a chip focuses it and drops its picker at it. The worktree
    // box toggles instead of opening a picker; a `never` project's Branch
    // chip is read-only: focus only, no picker.
    if field == Focus::Worktree {
        if let Some(l) = view.launcher.as_mut() {
            l.focus = field;
            toggle_worktree(l, &view.launcher_catalog);
        }
        return Ok(true);
    }
    if field == Focus::Branch
        && view
            .launcher
            .as_ref()
            .is_some_and(|l| l.draft.policy_never(&view.launcher_catalog))
    {
        if let Some(l) = view.launcher.as_mut() {
            l.focus = field;
        }
        return Ok(true);
    }
    let anchor = view
        .launcher
        .as_ref()
        .and_then(|l| chip_anchor(l, view, field));
    if let Some(l) = view.launcher.as_mut() {
        l.focus = field;
        open_picker_at(l, &view.launcher_catalog, &view.backlog, anchor, field);
    }
    Ok(true)
}
