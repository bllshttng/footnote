//! Grouping partition for the grid rail (ab-1fab1fdf, Phase 1).
//!
//! `group_by(rows, key) -> Vec<Group>` is the only public surface.
//! Everything here is pure over the registry rows that `filter_pty_agents`
//! already reads - zero new I/O so the run loop passes the same slice it
//! already holds.
//!
//! ## Invariants
//!
//! - Groups are sorted by header (lexicographic, ascending).
//! - Members within each group are stable by name (ascending).
//! - A row with a missing / null field is bucketed into `"unknown"` rather
//!   than panicking (Domain Pitfall: defensive reads, never unwrap).
//! - An empty group (0 members) may appear when a key produces a header with
//!   no matching rows; callers must short-circuit before calling
//!   `layout::compute` with pane_count 0 (AC1-EDGE).
//! - Sum invariant: `groups.iter().map(|g| g.members.len()).sum() == in_scope_count`.

use crate::state::{InsideLegReport, InsideLegState};
use serde_json::Value;

/// Which field to partition on. `g` cycles through in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKey {
    Cwd,
    Session,
    Provider,
    Status,
    /// Manual squads (x-5b3e). Unlike the others this is NOT a row-field
    /// partition: squad groups are resolved from `~/.fno/squads.json` by
    /// [`super::squads::squad_groups`], not [`group_by`] (which has no squad
    /// store and returns empty for this key). A `g`-cycle into this view swaps
    /// the rail's derived sidelines for the user's squads; the same agent still
    /// appears under its repo sideline in the Cwd view (reference membership).
    Squad,
    /// Both derived sidelines AND manual squads at once (x-fef5). Like
    /// [`Squad`](GroupKey::Squad) this is NOT a row-field partition: the rail
    /// assembles it in [`super::run`]'s `base_groups` as the `Cwd` sidelines
    /// concatenated with the squad groups, each half in a disjoint key namespace
    /// (`cwd:` / `squad:`) so an agent appearing in both is two independently
    /// selectable occurrences (the occurrence cursor, x-8a6a). [`group_by`]
    /// returns empty for this key, mirroring `Squad`.
    Union,
}

impl GroupKey {
    /// The next key in the cycle (cwd -> session -> provider -> status -> squad -> cwd).
    pub fn next(self) -> GroupKey {
        match self {
            GroupKey::Cwd => GroupKey::Session,
            GroupKey::Session => GroupKey::Provider,
            GroupKey::Provider => GroupKey::Status,
            GroupKey::Status => GroupKey::Squad,
            GroupKey::Squad => GroupKey::Union,
            GroupKey::Union => GroupKey::Cwd,
        }
    }

    /// Human-readable name for the footer `group-by: <key>` display.
    pub fn label(self) -> &'static str {
        match self {
            GroupKey::Cwd => "cwd",
            GroupKey::Session => "session",
            GroupKey::Provider => "provider",
            GroupKey::Status => "status",
            GroupKey::Squad => "squad",
            GroupKey::Union => "union",
        }
    }

    /// Extract the grouping value from a registry row. Returns `None` when
    /// the field is absent or not a string (caller buckets to `"unknown"`).
    fn extract<'a>(&self, row: &'a Value) -> Option<&'a str> {
        match self {
            // Repo-rollup (x-cb89): bucket by the canonical repo root stamped
            // onto the frozen rail snapshot by `repo::stamp_row` (a worktree and
            // its repo's main checkout carry the same `_repo_root`, so they roll
            // up under one sideline). Fall back to the literal `cwd` when the row
            // was never stamped (e.g. the railless path or a pure unit test).
            GroupKey::Cwd => row
                .get(crate::grid::repo::REPO_ROOT_FIELD)
                .and_then(Value::as_str)
                .or_else(|| row.get("cwd").and_then(Value::as_str)),
            GroupKey::Provider => row.get("provider").and_then(Value::as_str),
            GroupKey::Status => row.get("status").and_then(Value::as_str),
            // Session is provider-specific on disk. Python (which authors most
            // rows) writes `{codex,gemini}_session_id` / `cc_session_id` /
            // `claude_short_id` and exposes a unified `session_id` only as a
            // computed @property it NEVER serializes; only Rust-authored rows
            // carry `session_id` directly. Reading the bare `"session_id"` key
            // therefore collapses every Python row into one `"unknown"` bucket.
            // Try the real fields in priority order, falling back to the stable
            // `short_id` so a sessionless agent groups under its own identity
            // rather than co-bucketing with unrelated sessionless agents.
            GroupKey::Session => SESSION_ID_FIELDS
                .iter()
                .find_map(|field| row.get(field).and_then(Value::as_str)),
            // Squad grouping is resolved from the squad store, not a row field.
            // `group_by` short-circuits this key before `extract` runs; this arm
            // exists only for exhaustiveness.
            GroupKey::Squad => None,
            // Union is assembled in `base_groups` (sidelines ++ squads), never a
            // row-field partition; `group_by` short-circuits it before `extract`.
            GroupKey::Union => None,
        }
    }
}

/// Registry fields that may carry an agent's session identity, in priority
/// order. See [`GroupKey::extract`] for the cross-language rationale (Python's
/// `session_id` is a non-serialized computed property, so real on-disk rows
/// carry only the provider-specific ids).
const SESSION_ID_FIELDS: &[&str] = &[
    "session_id",
    "codex_session_id",
    "gemini_session_id",
    "cc_session_id",
    "claude_short_id",
    "short_id",
];

/// One group of agent indices sharing the same key value.
///
/// `members` are indices into the original `rows` slice (the same slice passed
/// to `group_by`). The renderer maps them to agent names / pane slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Display header (the raw key value, or `"unknown"`).
    pub header: String,
    /// Cursor-identity key. Usually the raw key value (same as `header`, or
    /// `"unknown"` for missing fields), but composite views namespace it with a
    /// prefix (`squad:` for squads, `cwd:` for the union's sidelines) so an
    /// agent in two visible groups stays two distinct occurrences.
    pub key_value: String,
    /// Indices into the agent list, stable by name (ascending). May be empty.
    pub members: Vec<usize>,
}

/// Partition `rows` (the agent row list, already filtered to in-scope PTY
/// agents) by `key`. Returns groups sorted by header, members sorted by name.
///
/// The indices in `members` are into `rows` - the caller holds both and can
/// look up names with `rows[idx]["name"]`.
///
/// # Defensive contract
///
/// - A row with a missing / null / non-string `key` field is bucketed under
///   `"unknown"`. No unwrap, no panic.
/// - A row with a missing `name` field uses `""` as its sort key (sorted to
///   front of its group), but IS included so the sum invariant holds.
pub fn group_by(rows: &[Value], key: GroupKey) -> Vec<Group> {
    // Squad and Union are not row-field partitions: their groups come from the
    // squad store (`squads::squad_groups`) and the `base_groups` union assembly
    // respectively. Returning empty here keeps a stray `group_by` call from
    // collapsing every row into one "unknown" bucket (the rail routes these keys
    // through `base_groups` instead - see `rail_view_groups`).
    if matches!(key, GroupKey::Squad | GroupKey::Union) {
        return Vec::new();
    }
    // Collect (key_value, name, original_index) triples.
    let mut entries: Vec<(String, String, usize)> = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let kv = key.extract(row).unwrap_or("unknown").to_string();
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (kv, name, idx)
        })
        .collect();

    // Sort by (key_value, name) so groups are contiguous and members are name-stable.
    entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    // Group into contiguous key-value runs.
    let mut groups: Vec<Group> = Vec::new();
    for (kv, _name, idx) in entries {
        if let Some(last) = groups.last_mut() {
            if last.key_value == kv {
                last.members.push(idx);
                continue;
            }
        }
        // Repo-rollup display (x-cb89): the Cwd key buckets on the full repo-root
        // PATH (so two repos sharing a basename stay distinct - AC1-EDGE) but the
        // header shows just the basename (`footnote`, not the full path - US1).
        // Every other key displays its raw value verbatim.
        let header = match key {
            GroupKey::Cwd => crate::grid::repo::basename(&kv).to_string(),
            _ => kv.clone(),
        };
        groups.push(Group {
            header,
            key_value: kv,
            members: vec![idx],
        });
    }

    groups
}

// ── Attention badges ─────────────────────────────────────────────────────────

/// Attention badge for a group header: which agents within the group
/// currently need attention. Used by the rail renderer to show distinct
/// badges from the member count (AC5-UI).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupBadge {
    /// Count of agents currently waiting for input, from the live readiness
    /// scan (`Pane::is_waiting`) - NOT a registry status string.
    pub needs_input: usize,
    /// Count of agents whose process has exited, from the live per-pane
    /// `ConnState::Exited` - NOT a registry status string.
    pub exited: usize,
}

impl GroupBadge {
    /// True when no attention is needed (no badge to render).
    pub fn is_empty(&self) -> bool {
        self.needs_input == 0 && self.exited == 0
    }
}

/// One member's attention contribution under the 3-tier authority lattice
/// (inside-out E3.3): `Exited (PTY) > inside-leg (within ttl) > scraper >
/// unknown`. Pure and total over its inputs so it is unit-testable without a
/// pane or a registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberAttention {
    /// PTY process exited - tier 1, beats everything (umbrella D4).
    Exited,
    /// Needs the operator: a live inside-leg `blocked`, or (no live inside-leg
    /// authority) the scraper's `waiting` readiness signal.
    NeedsInput,
    /// No attention: working (live inside-leg, busy) or genuinely idle.
    None,
}

/// Resolve one member's attention under the 3-tier authority lattice. `exited`
/// is PTY liveness (`ConnState::Exited`), `report` the registry's latest
/// inside-leg state, `now_secs` the wall clock for the TTL gate, `scraper_waiting`
/// the screen-scan readiness (`Pane::is_waiting`).
///
/// - **Tier 1 (PTY exit)** wins outright: a dead pane is never resurrected by a
///   stale inside-leg badge (umbrella Locked Decision D4).
/// - **Tier 2 (inside-leg, within TTL)** is authoritative over the scraper while
///   live: `working` suppresses the scraper's false "needs input" when a prompt
///   glyph is mid-render (AC-X2-3); `blocked` raises attention; `done` means the
///   turn finished, so it yields to the scraper (tier 3).
/// - **Tier 3 (scraper)** decides when there is no live inside-leg authority
///   (absent, expired per AC-X2-2, or `done`).
/// - **Tier 4 (unknown)** is "no attention" - the `None` default.
fn member_attention(
    exited: bool,
    report: Option<&InsideLegReport>,
    now_secs: u64,
    scraper_waiting: bool,
) -> MemberAttention {
    if exited {
        return MemberAttention::Exited;
    }
    if let Some(rep) = report {
        if rep.is_live_at(now_secs) {
            match rep.state {
                InsideLegState::Working => return MemberAttention::None,
                InsideLegState::Blocked => return MemberAttention::NeedsInput,
                // `done`: the turn completed; fall through to the scraper, which
                // reads the agent's now-idle/waiting screen state.
                InsideLegState::Done => {}
            }
        }
    }
    if scraper_waiting {
        MemberAttention::NeedsInput
    } else {
        MemberAttention::None
    }
}

/// Compute attention badges for a set of groups from the **live** per-agent
/// signals the run loop already tracks: `waiting[i]` (the focused-pane
/// readiness scan, `Pane::is_waiting`), `exited[i]` (`ConnState::Exited`), and
/// `inside_leg[i]` (the registry's latest inside-leg report for that agent, the
/// 3-tier authority's middle tier - inside-out E3.3). All slices are indexed by
/// agent index - the same index `Group::members` holds - so a member out of
/// range is treated as "no signal" (defensive `.get`, never panics); pass an
/// empty `inside_leg` slice when no reports apply. `now_secs` is the wall clock
/// the TTL gate ages reports against (AC-X2-2).
///
/// This deliberately does NOT read the registry `status` string: an `--all`
/// fleet is pre-filtered to alive-ish statuses (ready/idle/busy/live/spawning),
/// and `rail_rows` is a frozen startup snapshot, so a status-based badge could
/// never fire for a running agent that later exits or starts waiting. The live
/// signal is the only correct source (sigma-review, ab-ecf48467). The inside-leg
/// report is snapshot-bound to that same `rail_rows` today, but the TTL gate
/// makes it self-correcting: a stale `working` ages out and the live scraper
/// takes over, so the badge never pins a forever-`working` even without a
/// per-frame registry refresh (which the run loop's own comment defers).
///
/// Kept pure and separate from `group_by` so it can be recomputed every frame
/// without re-partitioning.
pub fn compute_badges_from_live(
    groups: &[Group],
    waiting: &[bool],
    exited: &[bool],
    inside_leg: &[Option<InsideLegReport>],
    now_secs: u64,
) -> Vec<GroupBadge> {
    groups
        .iter()
        .map(|g| {
            let mut badge = GroupBadge::default();
            for &idx in &g.members {
                let att = member_attention(
                    exited.get(idx).copied().unwrap_or(false),
                    inside_leg.get(idx).and_then(|o| o.as_ref()),
                    now_secs,
                    waiting.get(idx).copied().unwrap_or(false),
                );
                match att {
                    MemberAttention::Exited => badge.exited += 1,
                    MemberAttention::NeedsInput => badge.needs_input += 1,
                    MemberAttention::None => {}
                }
            }
            badge
        })
        .collect()
}

/// The per-member "needs the operator" signal AFTER the 3-tier authority lattice
/// (inside-out E3.3): a live inside-leg `working` suppresses a false scraper
/// `waiting`, a live `blocked` raises attention even when the scraper is quiet,
/// and an exited pane never needs input. This is the resolved signal the `a`
/// attention filter must use (via [`attention_view`]) so the filtered rail view
/// honors the SAME authority the badges do - otherwise the filter would hide a
/// `blocked` pane the scraper can't see, or keep a `working` pane the inside leg
/// has already cleared (codex P2). Returns a `Vec<bool>` indexed like
/// `waiting`/`exited`/`inside_leg`, sized to `waiting.len()`.
pub fn needs_input_after_authority(
    waiting: &[bool],
    exited: &[bool],
    inside_leg: &[Option<InsideLegReport>],
    now_secs: u64,
) -> Vec<bool> {
    (0..waiting.len())
        .map(|idx| {
            matches!(
                member_attention(
                    exited.get(idx).copied().unwrap_or(false),
                    inside_leg.get(idx).and_then(|o| o.as_ref()),
                    now_secs,
                    waiting[idx],
                ),
                MemberAttention::NeedsInput
            )
        })
        .collect()
}

/// Return the **live** members of `group`: its member indices with any agent
/// whose pane has exited filtered out. `exited[i]` is the live per-pane signal
/// (`ConnState::Exited`), indexed the same way `Group::members` is, so an index
/// out of range is treated as "not exited" (defensive `.get`, never panics).
///
/// GroupTile tiles the live members so a member that exits mid-session drops its
/// tile and the survivors reflow to fill the freed space (AC3-FR). The full
/// `group.members` is retained for the rail list and the header's `(count)` /
/// `xN` badge, so an exited agent stays visible in the rail (with its exited
/// label) even though it no longer holds a tile in the main area. Member order
/// is preserved (still name-stable).
pub fn live_members(group: &Group, exited: &[bool]) -> Vec<usize> {
    group
        .members
        .iter()
        .copied()
        .filter(|&idx| !exited.get(idx).copied().unwrap_or(false))
        .collect()
}

/// Filter `groups` to the attention view: each group keeps only the members
/// currently waiting for input (`waiting[idx]`), and a group left with no
/// waiting member is dropped entirely. This backs the `a` attention filter - the
/// rail shows only the agents that need the operator, hiding idle and exited
/// ones.
///
/// `waiting[i]` is the live readiness signal (`Pane::is_waiting`), indexed the
/// same way `Group::members` is, so an out-of-range index is treated as "not
/// waiting" (defensive `.get`, never panics). Member order and headers are
/// preserved. Because a waiting agent is never also exited (`is_waiting` excludes
/// non-scannable states), the attention set is a subset of the live set, so this
/// composes cleanly with [`live_members`] in GroupTile.
pub fn attention_view(groups: &[Group], waiting: &[bool]) -> Vec<Group> {
    groups
        .iter()
        .filter_map(|g| {
            let members: Vec<usize> = g
                .members
                .iter()
                .copied()
                .filter(|&idx| waiting.get(idx).copied().unwrap_or(false))
                .collect();
            if members.is_empty() {
                None
            } else {
                Some(Group {
                    header: g.header.clone(),
                    key_value: g.key_value.clone(),
                    members,
                })
            }
        })
        .collect()
}

// ── Rail selection state ─────────────────────────────────────────────────────

/// Which axis the user is currently interacting with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusAxis {
    /// Arrows navigate the rail; the compositor is in watch mode.
    RailNav,
    /// Keystrokes forward to the focused PTY; rail keys are suspended.
    PaneDrive,
}

/// What the main area renders (US3). Toggled live with `Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainMode {
    /// One focused agent fills the main area at full width (Tab zooms here).
    Single,
    /// The selected agent's whole group tiles side-by-side in the main area.
    /// The E5c default (AC-2): a space shows its members auto-tiled.
    GroupTile,
}

/// Rail navigation state: which row is selected and which groups are expanded.
///
/// Selection identifies a **group occurrence**, not just an agent: the pair
/// (`selected_group_key`, `selected_agent_idx`). The agent index survives
/// `group_by` re-partitions (focus follows the agent, not the slot - AC4-FR),
/// and the group key disambiguates which copy is selected when the same agent
/// appears in two visible groups - an agent recruited into 2+ squads in the
/// Squad view, or the simultaneous sideline+squad union (x-8a6a). When the agent
/// is gone from the new partition, clamp to the nearest valid row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailState {
    /// Index into `rows` of the currently selected agent; `None` when the
    /// fleet is empty.
    pub selected_agent_idx: Option<usize>,
    /// `key_value` of the group occurrence the selection sits in. Disambiguates
    /// the selected copy when `selected_agent_idx` appears in 2+ groups (the
    /// duplicate-index case this state exists to fix). `None` before the first
    /// nav/anchor, or for a selection set directly without an occurrence; the
    /// resolvers then fall back to the first group containing the agent
    /// (back-compat, the pre-x-8a6a first-match behavior).
    ///
    /// `pub(crate)`, not `pub`: nothing outside this crate reads or writes it,
    /// and keeping the pair's second half off the public API stops a future call
    /// site from desyncing it with a bare `selected_agent_idx` write. Mutate it
    /// only through the nav methods (or alongside `selected_agent_idx`).
    pub(crate) selected_group_key: Option<String>,
    /// The active focus axis.
    pub axis: FocusAxis,
    /// The active group-by key.
    pub group_key: GroupKey,
    /// What the main area renders: a single focused pane, or the selected
    /// agent's whole group tiled side-by-side (US3).
    pub main_mode: MainMode,
    /// When `true`, the rail lists only agents waiting for input - the
    /// attention filter (`a`). Orthogonal to `group_key` and `main_mode`, so it
    /// persists across regroups and mode toggles. Defaults `false` (show all).
    pub attention_filter: bool,
}

impl RailState {
    pub fn new(group_key: GroupKey) -> Self {
        RailState {
            selected_agent_idx: None,
            selected_group_key: None,
            axis: FocusAxis::RailNav,
            group_key,
            // E5c AC-2: a space auto-tiles its members. GroupTile is the
            // default so >1 agent in one project shows as tiled panes without a
            // Tab toggle; a single-member space tiles as one full-width pane.
            main_mode: MainMode::GroupTile,
            attention_filter: false,
        }
    }

    /// Toggle the attention filter (`a`). The caller MUST re-partition through
    /// [`attention_view`] and re-anchor the selection afterward (the visible
    /// member set changes), mirroring the `g`/Tab full-repaint discipline.
    pub fn toggle_attention_filter(&mut self) {
        self.attention_filter = !self.attention_filter;
    }

    /// Advance to the next group-by key (`g`). Does NOT clear the selection:
    /// the caller MUST call [`re_anchor`](Self::re_anchor) after
    /// re-partitioning so the same agent stays selected across the regroup
    /// (focus-follows-agent, AC4-FR).
    pub fn cycle_group_key(&mut self) {
        self.group_key = self.group_key.next();
    }

    /// Total members across all groups - the length of the flat rail list. A
    /// duplicated agent counts once per occurrence (so a member in two squads
    /// adds two rail rows), which is exactly the cursor space nav steps over.
    fn flat_len(groups: &[Group]) -> usize {
        groups.iter().map(|g| g.members.len()).sum()
    }

    /// The flat rail position (index into the group-order-then-member-order
    /// flattening) of the currently selected **occurrence**. Resolves by the
    /// (group key, agent) pair so a duplicated agent lands on the exact copy the
    /// cursor sits in. Falls back to the first occurrence of the agent when the
    /// group key does not match any current group (a re-partition moved it, or
    /// the selection was set directly without a key). `None` when nothing is
    /// selected or the agent is absent from `groups`.
    fn current_pos(&self, groups: &[Group]) -> Option<usize> {
        let agent = self.selected_agent_idx?;
        let mut pos = 0usize;
        let mut first_match: Option<usize> = None;
        for g in groups {
            for &m in &g.members {
                if m == agent {
                    if first_match.is_none() {
                        first_match = Some(pos);
                    }
                    if self.selected_group_key.as_deref() == Some(g.key_value.as_str()) {
                        return Some(pos);
                    }
                }
                pos += 1;
            }
        }
        first_match
    }

    /// Set the selection to the occurrence at flat position `pos`, updating both
    /// `selected_agent_idx` (the agent there) and `selected_group_key` (the
    /// group it occupies) so the pair always identifies one occurrence. Returns
    /// the agent index, or `None` when `pos` is out of range.
    fn set_pos(&mut self, groups: &[Group], pos: usize) -> Option<usize> {
        let mut i = 0usize;
        for g in groups {
            for &m in &g.members {
                if i == pos {
                    self.selected_agent_idx = Some(m);
                    self.selected_group_key = Some(g.key_value.clone());
                    return Some(m);
                }
                i += 1;
            }
        }
        None
    }

    /// After a re-partition (re-group), re-anchor the selection to the same
    /// occurrence. The agent index leads (focus-follows-agent, AC4-FR); the
    /// group key follows it into whatever group now holds it. If the agent no
    /// longer appears in `groups` (it was removed), clamp to the first available
    /// member across all groups.
    ///
    /// Returns the new `selected_agent_idx`.
    pub fn re_anchor(&mut self, groups: &[Group]) -> Option<usize> {
        if Self::flat_len(groups) == 0 {
            self.selected_agent_idx = None;
            self.selected_group_key = None;
            return None;
        }
        // Keep the exact occurrence when the agent is still present (current_pos
        // re-resolves the group key into the new partition); else clamp to first.
        let pos = self.current_pos(groups).unwrap_or(0);
        self.set_pos(groups, pos)
    }

    /// Move selection up in the rail (toward the first row). Clamps at the
    /// beginning (no wrap-panic). With nothing selected, lands on the first
    /// member (consistent with `move_down`). Returns the new selected index.
    pub fn move_up(&mut self, groups: &[Group]) -> Option<usize> {
        if Self::flat_len(groups) == 0 {
            self.selected_agent_idx = None;
            self.selected_group_key = None;
            return None;
        }
        let new_pos = match self.current_pos(groups) {
            Some(pos) => pos.saturating_sub(1),
            None => 0,
        };
        self.set_pos(groups, new_pos)
    }

    /// Move selection down in the rail (toward the last row). Clamps at the
    /// end (no wrap-panic). With nothing selected, lands on the first member
    /// (NOT the second - the `None` case must not skip member 0, gemini HIGH).
    /// Steps the flat occurrence position, so a duplicated agent's second copy
    /// is reachable past its first (AC4-EDGE, the duplicate-index fix).
    pub fn move_down(&mut self, groups: &[Group]) -> Option<usize> {
        let total = Self::flat_len(groups);
        if total == 0 {
            self.selected_agent_idx = None;
            self.selected_group_key = None;
            return None;
        }
        let new_pos = match self.current_pos(groups) {
            Some(pos) => (pos + 1).min(total - 1),
            None => 0,
        };
        self.set_pos(groups, new_pos)
    }

    /// Enter drive mode on the currently selected agent.
    /// Returns `false` (and stays RailNav) when no agent is selected.
    pub fn enter_drive(&mut self) -> bool {
        if self.selected_agent_idx.is_some() {
            self.axis = FocusAxis::PaneDrive;
            true
        } else {
            false
        }
    }

    /// Exit drive mode, returning to RailNav. Always succeeds.
    pub fn exit_drive(&mut self) {
        self.axis = FocusAxis::RailNav;
    }

    /// Revert the axis to RailNav when the driven agent exits or its socket
    /// drops (AC2-FR). Returns `true` iff the axis actually changed, so the
    /// caller can force a full-paint and surface an "agent exited" cue rather
    /// than stranding the operator in a phantom PaneDrive whose keystrokes
    /// vanish into a closed PTY. Idempotent: a no-op (returns `false`) in
    /// RailNav.
    pub fn revert_to_nav(&mut self) -> bool {
        if self.axis == FocusAxis::PaneDrive {
            self.axis = FocusAxis::RailNav;
            true
        } else {
            false
        }
    }

    // ── Main-area mode (US3) ─────────────────────────────────────────────────

    /// Toggle the main area between [`MainMode::Single`] and
    /// [`MainMode::GroupTile`] (`Tab`). The displayed group page is derived from
    /// the selected member at render time (see [`selected_group_page`](
    /// Self::selected_group_page)), so the toggle only flips the mode; the
    /// caller forces a full repaint (region map changed) and re-sizes the
    /// now-visible panes.
    pub fn toggle_main_mode(&mut self) {
        self.main_mode = match self.main_mode {
            MainMode::Single => MainMode::GroupTile,
            MainMode::GroupTile => MainMode::Single,
        };
    }

    /// The group that currently holds the selected agent, if any. GroupTile
    /// tiles exactly this group's members. Returns `None` when nothing is
    /// selected or the selection is absent from `groups` (empty fleet), so the
    /// caller can avoid calling `layout::compute` with a 0-member group
    /// (AC1-EDGE / AC3-EDGE).
    pub fn selected_group<'a>(&self, groups: &'a [Group]) -> Option<&'a Group> {
        let sel = self.selected_agent_idx?;
        // Prefer the exact occupied group (matching key); the first-match scan is
        // the back-compat fallback when no occurrence is pinned (key None) or the
        // pinned group is gone. This is the fix for "GroupTile tiles the wrong
        // group for a shared agent" - the second copy now resolves to its own
        // group, not the first one that happens to contain the agent.
        if let Some(key) = &self.selected_group_key {
            if let Some(g) = groups
                .iter()
                .find(|g| &g.key_value == key && g.members.contains(&sel))
            {
                return Some(g);
            }
        }
        groups.iter().find(|g| g.members.contains(&sel))
    }

    /// True iff `member_idx` in `group` is the selected occurrence - the agent
    /// matches AND `group` is the occupied group. The renderer uses this so a
    /// shared agent is accented only in the group the cursor sits in, not in
    /// every group it appears in (the "highlights every occurrence" fix). When
    /// no occurrence is pinned (key None, pre-nav), falls back to highlighting
    /// the agent wherever it appears (the pre-x-8a6a behavior).
    pub fn is_selected_occurrence(&self, group: &Group, member_idx: usize) -> bool {
        if self.selected_agent_idx != Some(member_idx) {
            return false;
        }
        match &self.selected_group_key {
            Some(key) => &group.key_value == key,
            None => true,
        }
    }

    /// The 0-based position of the selected agent within `group.members`, or
    /// `None` when the selection is not a member of `group`.
    pub fn selected_position(&self, group: &Group) -> Option<usize> {
        let sel = self.selected_agent_idx?;
        group.members.iter().position(|&m| m == sel)
    }

    /// The 0-based page of the tiled group that holds the selected member, for a
    /// main area of the given per-page `capacity`. GroupTile renders this page
    /// so the selected (accented) tile is ALWAYS on screen - selection drives
    /// the page rather than tracking an independent index, so `Enter`/`d` never
    /// drives an off-page agent (mirrors `PageLayout`'s focus-anchored
    /// `current_page`). Defaults to page 0 when the selection is absent.
    pub fn selected_group_page(&self, group: &Group, capacity: usize) -> usize {
        let cap = capacity.max(1);
        self.selected_position(group)
            .map(|pos| pos / cap)
            .unwrap_or(0)
    }

    /// Jump the selection forward / back by one page (`capacity` members) within
    /// `group`, clamped to the group's bounds (no wrap, no out-of-range -
    /// AC3-ERR / AC4-EDGE). Because the rendered page follows the selection,
    /// this is how `]`/`[` page through an oversized tiled group. A no-op for an
    /// empty group.
    pub fn page_jump(&mut self, group: &Group, forward: bool, capacity: usize) {
        let cap = capacity.max(1);
        if group.members.is_empty() {
            return;
        }
        let pos = self.selected_position(group).unwrap_or(0);
        let new_pos = if forward {
            (pos + cap).min(group.members.len() - 1)
        } else {
            pos.saturating_sub(cap)
        };
        self.selected_agent_idx = Some(group.members[new_pos]);
        // Paging stays within `group`, so keep the occurrence key aligned to it
        // (the caller passes the resolved selected group / its live view).
        self.selected_group_key = Some(group.key_value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Helper ───────────────────────────────────────────────────────────────

    fn make_rows(specs: &[(&str, &str, &str, &str)]) -> Vec<Value> {
        // (name, cwd, provider, status)
        specs
            .iter()
            .map(|(name, cwd, provider, status)| {
                json!({ "name": name, "cwd": cwd, "provider": provider, "status": status })
            })
            .collect()
    }

    // ── AC1-HP: group_by cwd produces correct groups ──────────────────────

    #[test]
    fn ac1_hp_group_by_cwd_three_distinct_cwds() {
        let rows = make_rows(&[
            ("wkA", "/repo/alpha", "codex", "live"),
            ("wkB", "/repo/beta", "gemini", "live"),
            ("wkC", "/repo/alpha", "codex", "live"),
            ("wkD", "/repo/gamma", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);

        // Three distinct cwds; sorted by key_value (the full path). The header
        // is the repo basename (US1), while key_value keeps the full path.
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].header, "alpha");
        assert_eq!(groups[0].key_value, "/repo/alpha");
        assert_eq!(groups[1].header, "beta");
        assert_eq!(groups[2].header, "gamma");

        // Alpha group has two members (wkA at idx 0, wkC at idx 2).
        assert_eq!(groups[0].members.len(), 2);
    }

    #[test]
    fn ac1_hp_sum_invariant_holds() {
        let rows = make_rows(&[
            ("wkA", "/repo/alpha", "codex", "live"),
            ("wkB", "/repo/beta", "gemini", "live"),
            ("wkC", "/repo/alpha", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let total_members: usize = groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(
            total_members,
            rows.len(),
            "sum invariant: every row appears once"
        );
    }

    #[test]
    fn ac1_hp_members_sorted_by_name_within_group() {
        let rows = make_rows(&[
            ("wkZ", "/repo/alpha", "codex", "live"),
            ("wkA", "/repo/alpha", "codex", "live"),
            ("wkM", "/repo/alpha", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        assert_eq!(groups.len(), 1);
        // Members must be sorted by name: wkA, wkM, wkZ.
        let names: Vec<&str> = groups[0]
            .members
            .iter()
            .map(|&i| rows[i]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["wkA", "wkM", "wkZ"]);
    }

    // ── Repo-rollup (x-cb89): grouping reads the stamped `_repo_root` ─────────

    /// A row stamped with `_repo_root` + `cwd` by `repo::stamp_row`.
    fn stamped(name: &str, cwd: &str, repo_root: &str) -> Value {
        json!({
            "name": name,
            "cwd": cwd,
            "provider": "claude",
            "status": "live",
            crate::grid::repo::REPO_ROOT_FIELD: repo_root,
        })
    }

    #[test]
    fn ac1_hp_repo_rollup_merges_worktrees_under_one_entry() {
        // AC1-HP: the main checkout and a worktree (different cwds) carry the
        // SAME `_repo_root`, so they roll up into ONE `footnote` sideline.
        let rows = vec![
            stamped(
                "wkMain",
                "/code/footnote/footnote",
                "/code/footnote/footnote",
            ),
            stamped(
                "wkLeaf",
                "/conductor/workspaces/footnote/e5c-layout",
                "/code/footnote/footnote",
            ),
        ];
        let groups = group_by(&rows, GroupKey::Cwd);
        assert_eq!(groups.len(), 1, "one repo entry, not one-per-checkout");
        assert_eq!(groups[0].header, "footnote", "header is the repo basename");
        assert_eq!(groups[0].key_value, "/code/footnote/footnote");
        assert_eq!(groups[0].members.len(), 2);
    }

    #[test]
    fn ac1_edge_shared_basename_stays_two_entries() {
        // AC1-EDGE: two repos rooted at different paths but sharing the basename
        // `footnote` must NOT collapse - grouping keys on the full root path.
        let rows = vec![
            stamped("wkA", "/a/footnote", "/a/footnote"),
            stamped("wkB", "/b/footnote", "/b/footnote"),
        ];
        let groups = group_by(&rows, GroupKey::Cwd);
        assert_eq!(groups.len(), 2, "distinct roots are distinct sidelines");
        assert!(groups.iter().all(|g| g.header == "footnote"));
        let keys: Vec<&str> = groups.iter().map(|g| g.key_value.as_str()).collect();
        assert!(keys.contains(&"/a/footnote") && keys.contains(&"/b/footnote"));
    }

    #[test]
    fn ac1_err_ungrouped_sentinel_buckets_together() {
        // A non-git agent is stamped `_repo_root = "ungrouped"`; all such agents
        // share the single ungrouped sideline and are never dropped.
        let rows = vec![
            stamped("wkA", "/tmp/scratch", crate::grid::repo::UNGROUPED),
            stamped("wkB", "/tmp/other", crate::grid::repo::UNGROUPED),
            stamped("wkC", "/code/footnote/footnote", "/code/footnote/footnote"),
        ];
        let groups = group_by(&rows, GroupKey::Cwd);
        assert_eq!(groups.len(), 2);
        let ungrouped = groups
            .iter()
            .find(|g| g.header == "ungrouped")
            .expect("ungrouped sideline");
        assert_eq!(
            ungrouped.members.len(),
            2,
            "both non-git agents bucket here"
        );
    }

    // ── AC1-ERR: missing / null cwd -> "unknown" bucket ──────────────────

    #[test]
    fn ac1_err_missing_cwd_buckets_to_unknown() {
        let rows = vec![
            json!({ "name": "wkA", "provider": "codex", "status": "live" }), // no cwd
            json!({ "name": "wkB", "cwd": null, "provider": "gemini", "status": "live" }),
            json!({ "name": "wkC", "cwd": "/repo/alpha", "provider": "codex", "status": "live" }),
        ];
        let groups = group_by(&rows, GroupKey::Cwd);

        // "unknown" (missing/null cwd, unstamped) and the "alpha" repo group.
        assert_eq!(groups.len(), 2);
        let unknown = groups
            .iter()
            .find(|g| g.header == "unknown")
            .expect("unknown group");
        assert_eq!(unknown.members.len(), 2, "both missing-cwd rows in unknown");

        let alpha = groups
            .iter()
            .find(|g| g.key_value == "/repo/alpha")
            .expect("alpha group");
        assert_eq!(alpha.header, "alpha", "header is the repo basename");
        assert_eq!(alpha.members.len(), 1);
    }

    #[test]
    fn ac1_err_no_panic_on_malformed_row() {
        // Entirely empty row - no fields at all.
        let rows = vec![
            json!({}),
            json!({ "name": "wkA", "cwd": "/repo/alpha", "provider": "codex", "status": "live" }),
        ];
        // Must not panic.
        let groups = group_by(&rows, GroupKey::Cwd);
        let total: usize = groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(total, 2, "empty row still counted in sum invariant");
    }

    // ── AC1-EDGE: empty group (0 members) - note: group_by never produces 0-member
    // groups because every row maps to exactly one group. The AC1-EDGE case of a
    // "0-member group" refers to a GROUP KEY with no matching rows (all rows land
    // in other groups). That means the caller must handle this by not calling
    // layout::compute with pane_count 0 when a group is selected that has no
    // members. We verify the invariant here: every Group has >= 1 member IFF it
    // came from group_by on non-empty rows.

    #[test]
    fn ac1_edge_no_zero_member_groups_from_nonempty_rows() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/beta", "gemini", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        for g in &groups {
            assert!(
                !g.members.is_empty(),
                "group_by never produces 0-member groups from non-empty input"
            );
        }
    }

    #[test]
    fn ac1_edge_empty_rows_produces_no_groups() {
        let groups = group_by(&[], GroupKey::Cwd);
        assert!(groups.is_empty());
    }

    // ── AC4-HP: cycle group key ───────────────────────────────────────────

    #[test]
    fn ac4_hp_group_by_provider() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/beta", "gemini", "live"),
            ("wkC", "/alpha", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Provider);
        assert_eq!(groups.len(), 2);
        let codex = groups.iter().find(|g| g.header == "codex").unwrap();
        assert_eq!(codex.members.len(), 2);
        let gemini = groups.iter().find(|g| g.header == "gemini").unwrap();
        assert_eq!(gemini.members.len(), 1);
    }

    // ── AC4: group by session id (cross-language registry shape) ──────────
    //
    // On-disk rows are usually Python-authored: a unified `session_id` is a
    // computed @property Python NEVER serializes, so real rows carry only the
    // provider-specific `codex_session_id` / `gemini_session_id` /
    // `cc_session_id` / `claude_short_id`. Grouping by session must read those
    // (and the Rust-only `session_id`), or every agent collapses into one
    // `"unknown"` bucket and the Session view is dead.
    #[test]
    fn ac4_session_groups_by_provider_specific_id() {
        let rows = vec![
            json!({ "name": "wkA", "provider": "codex",  "status": "live", "codex_session_id":  "sess-A" }),
            json!({ "name": "wkB", "provider": "gemini", "status": "live", "gemini_session_id": "sess-B" }),
            json!({ "name": "wkC", "provider": "codex",  "status": "live", "codex_session_id":  "sess-A" }),
        ];
        let groups = group_by(&rows, GroupKey::Session);
        assert_eq!(
            groups.len(),
            2,
            "two distinct sessions, not one 'unknown' bucket: {groups:?}"
        );
        let a = groups
            .iter()
            .find(|g| g.header == "sess-A")
            .expect("sess-A group");
        assert_eq!(a.members.len(), 2, "wkA + wkC share sess-A");
        let b = groups
            .iter()
            .find(|g| g.header == "sess-B")
            .expect("sess-B group");
        assert_eq!(b.members.len(), 1, "wkB alone in sess-B");
    }

    #[test]
    fn ac4_session_rust_session_id_and_short_id_fallback() {
        let rows = vec![
            // Rust-authored row carries the unified `session_id` directly.
            json!({ "name": "wkA", "provider": "codex", "status": "live", "session_id": "sess-R" }),
            // Sessionless agent falls back to its stable short_id, NOT collapsed
            // into a shared "unknown" bucket with other sessionless agents.
            json!({ "name": "wkB", "provider": "codex", "status": "live", "short_id": "wkB-short" }),
        ];
        let groups = group_by(&rows, GroupKey::Session);
        assert_eq!(groups.len(), 2, "distinct session identities: {groups:?}");
        assert!(groups.iter().any(|g| g.header == "sess-R"));
        assert!(groups.iter().any(|g| g.header == "wkB-short"));
    }

    #[test]
    fn ac4_hp_cycle_returns_next_key() {
        assert_eq!(GroupKey::Cwd.next(), GroupKey::Session);
        assert_eq!(GroupKey::Session.next(), GroupKey::Provider);
        assert_eq!(GroupKey::Provider.next(), GroupKey::Status);
        // Status cycles into the manual-squads view (x-5b3e), then the union
        // (x-fef5), then back to cwd - the cycle closes.
        assert_eq!(GroupKey::Status.next(), GroupKey::Squad);
        assert_eq!(GroupKey::Squad.next(), GroupKey::Union);
        assert_eq!(GroupKey::Union.next(), GroupKey::Cwd);
    }

    // ── x-fef5: Union view (sidelines + squads at once) ───────────────────

    #[test]
    fn union_label_and_group_by_short_circuit() {
        // Union is a non-row-field view, so `group_by` returns empty (it never
        // collapses every row into one "unknown" bucket - the Squad short-circuit
        // invariant extended to Union). The rail routes it via `base_groups`.
        assert_eq!(GroupKey::Union.label(), "union");
        let rows = make_rows(&[
            ("wkA", "/a", "codex", "live"),
            ("wkB", "/b", "gemini", "live"),
        ]);
        assert!(
            group_by(&rows, GroupKey::Union).is_empty(),
            "Union short-circuits group_by to empty (assembled in base_groups)"
        );
    }

    #[test]
    fn union_occurrence_cursor_selects_each_half_independently() {
        // A hand-built union: a `cwd:` sideline and a `squad:` squad that SHARE
        // member idx 0 (agent `x`). The x-8a6a occurrence cursor must reach the
        // second occurrence and resolve `selected_group` to the occupied half
        // without disturbing the other (the whole reason x-8a6a came first).
        let sideline = Group {
            header: "root".to_string(),
            key_value: "cwd:/repo/root".to_string(),
            members: vec![0, 1], // x, y
        };
        let squad = Group {
            header: "stack".to_string(),
            key_value: "squad:stack".to_string(),
            members: vec![0, 2], // x, z
        };
        let groups = vec![sideline, squad];

        let mut rs = RailState::new(GroupKey::Union);
        // First occurrence: agent x under its sideline (flat pos 0).
        assert_eq!(rs.move_down(&groups), Some(0));
        assert_eq!(
            rs.selected_group(&groups).map(|g| g.key_value.as_str()),
            Some("cwd:/repo/root")
        );

        // Step to the squad's x (sideline has [0,1] then squad [0,2]; flat
        // positions 0=x@cwd, 1=y@cwd, 2=x@squad). Two move_downs land on x@squad.
        rs.move_down(&groups); // -> y@cwd (pos 1)
        assert_eq!(rs.move_down(&groups), Some(0), "back on agent x"); // -> x@squad (pos 2)
        assert_eq!(
            rs.selected_group(&groups).map(|g| g.key_value.as_str()),
            Some("squad:stack"),
            "second occurrence resolves to the squad half, not the sideline"
        );
        // Selecting the squad occurrence does not mark the sideline's x selected.
        assert!(!rs.is_selected_occurrence(&groups[0], 0));
        assert!(rs.is_selected_occurrence(&groups[1], 0));
    }

    // ── AC4-ERR: all agents share the same provider -> single group ───────

    #[test]
    fn ac4_err_all_same_provider_one_group() {
        let rows = make_rows(&[
            ("wkA", "/a", "codex", "live"),
            ("wkB", "/b", "codex", "idle"),
            ("wkC", "/c", "codex", "busy"),
        ]);
        let groups = group_by(&rows, GroupKey::Provider);
        assert_eq!(groups.len(), 1, "all same provider => one group, no crash");
        assert_eq!(groups[0].members.len(), 3);
    }

    // ── AC4-FR: focus follows the agent across re-partition ───────────────

    #[test]
    fn ac4_fr_focus_follows_agent_across_repartition() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"), // idx 0
            ("wkB", "/beta", "gemini", "live"), // idx 1
            ("wkC", "/alpha", "codex", "live"), // idx 2
        ]);

        // Start grouped by cwd; select wkB (idx 1, in /beta group).
        let groups_by_cwd = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.selected_agent_idx = Some(1); // wkB

        // Switch to provider grouping.
        state.cycle_group_key();
        let groups_by_provider = group_by(&rows, state.group_key);

        // Re-anchor: wkB (idx 1) is now in the "gemini" group.
        let anchored = state.re_anchor(&groups_by_provider);
        assert_eq!(
            anchored,
            Some(1),
            "focus stays on wkB (idx 1) after re-partition"
        );
        assert_eq!(state.selected_agent_idx, Some(1));
    }

    #[test]
    fn ac4_fr_focus_clamps_when_agent_removed() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"), // idx 0
            ("wkB", "/beta", "gemini", "live"), // idx 1
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.selected_agent_idx = Some(1); // wkB

        // Simulate wkB removed: new rows only have wkA.
        let new_rows = make_rows(&[("wkA", "/alpha", "codex", "live")]);
        let new_groups = group_by(&new_rows, GroupKey::Cwd);
        let anchored = state.re_anchor(&new_groups);

        // wkB (idx 1) is gone from new_groups; clamp to first member (idx 0 = wkA).
        assert_eq!(anchored, Some(0));
        assert_eq!(state.selected_agent_idx, Some(0));
    }

    // ── AC5-HP: header member count ───────────────────────────────────────

    #[test]
    fn ac5_hp_header_count_matches_members() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/alpha", "codex", "live"),
            ("wkC", "/alpha", "codex", "live"),
            ("wkD", "/alpha", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 4);
    }

    #[test]
    fn ac5_err_removal_reduces_count_and_selection_clamps() {
        // Start: 4 members; selection on idx 3.
        let rows4 = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/alpha", "codex", "live"),
            ("wkC", "/alpha", "codex", "live"),
            ("wkD", "/alpha", "codex", "live"),
        ]);
        let groups4 = group_by(&rows4, GroupKey::Cwd);
        assert_eq!(groups4[0].members.len(), 4);

        let mut state = RailState::new(GroupKey::Cwd);
        state.selected_agent_idx = Some(3); // wkD

        // Remove wkD: 3 remaining.
        let rows3 = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/alpha", "codex", "live"),
            ("wkC", "/alpha", "codex", "live"),
        ]);
        let groups3 = group_by(&rows3, GroupKey::Cwd);
        assert_eq!(groups3[0].members.len(), 3);

        // idx 3 no longer exists in new_groups; clamp to first.
        let anchored = state.re_anchor(&groups3);
        assert!(anchored.is_some());
        assert!(anchored.unwrap() < 3, "clamped to a valid index");

        // Suppress unused.
        drop(groups4);
    }

    // ── AC5-UI: attention badges ──────────────────────────────────────────

    #[test]
    fn ac5_ui_attention_badge_needs_input() {
        // Registry status is "live" for every row (the realistic case); the
        // attention signal comes from the LIVE waiting scan, not the frozen
        // status string. A status-based badge would never fire here.
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/alpha", "codex", "live"),
            ("wkC", "/beta", "gemini", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let waiting = vec![true, false, false]; // wkA (idx 0) is waiting for input
        let exited = vec![false, false, false];
        let badges = compute_badges_from_live(&groups, &waiting, &exited, &[], 0);

        // alpha group: wkA is needs-input.
        let alpha_idx = groups.iter().position(|g| g.header == "alpha").unwrap();
        assert_eq!(badges[alpha_idx].needs_input, 1);
        assert_eq!(badges[alpha_idx].exited, 0);

        // beta group: no attention needed.
        let beta_idx = groups.iter().position(|g| g.header == "beta").unwrap();
        assert!(badges[beta_idx].is_empty());
    }

    #[test]
    fn ac5_ui_attention_badge_exited() {
        // Both rows carry an alive-ish registry status; the exit is surfaced by
        // the live ConnState-derived `exited` flag, which is exactly what the
        // registry-status path used to miss.
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/alpha", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let waiting = vec![false, false];
        let exited = vec![true, false]; // wkA exited since startup
        let badges = compute_badges_from_live(&groups, &waiting, &exited, &[], 0);
        assert_eq!(badges[0].exited, 1);
        assert_eq!(badges[0].needs_input, 0);
        assert!(!badges[0].is_empty());
    }

    // ── 3-tier authority (inside-out E3.3): Exited > inside-leg(ttl) > scraper ──

    fn report_at(state: InsideLegState, received_at: &str, ttl_ms: Option<u64>) -> InsideLegReport {
        InsideLegReport {
            state,
            seq: 1,
            reason: None,
            received_at: received_at.into(),
            ttl_ms,
        }
    }

    #[test]
    fn ac_x2_3_inside_leg_working_beats_scraper_idle() {
        // The scraper would read a prompt glyph as idle/needs-input (waiting=true),
        // but a live inside-leg `working` report says the agent is busy. The hook
        // wins while live: NO needs_input badge (AC-X2-3).
        let rows = make_rows(&[("wkA", "/a", "claude", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let now = crate::state::rfc3339_like_to_secs("2026-06-27T00:00:00Z").unwrap();
        let inside_leg = vec![Some(report_at(
            InsideLegState::Working,
            "2026-06-27T00:00:00Z",
            Some(5000),
        ))];
        let badges = compute_badges_from_live(&groups, &[true], &[false], &inside_leg, now);
        assert!(
            badges[0].is_empty(),
            "live working suppresses the scraper's false needs_input"
        );
    }

    #[test]
    fn ac_x2_2_expired_working_ages_out_to_scraper() {
        // Same working report, but the clock is past received_at + ttl. The badge
        // must NOT stay working: it ages out and the live scraper (waiting=true)
        // takes over -> needs_input (AC-X2-2, never a permanent stale working).
        let rows = make_rows(&[("wkA", "/a", "claude", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let recv = crate::state::rfc3339_like_to_secs("2026-06-27T00:00:00Z").unwrap();
        let inside_leg = vec![Some(report_at(
            InsideLegState::Working,
            "2026-06-27T00:00:00Z",
            Some(5000),
        ))];
        let badges = compute_badges_from_live(&groups, &[true], &[false], &inside_leg, recv + 6);
        assert_eq!(
            badges[0].needs_input, 1,
            "expired working yields to the scraper"
        );
    }

    #[test]
    fn inside_leg_blocked_raises_attention_even_when_scraper_quiet() {
        let rows = make_rows(&[("wkA", "/a", "claude", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let now = crate::state::rfc3339_like_to_secs("2026-06-27T00:00:00Z").unwrap();
        let inside_leg = vec![Some(report_at(
            InsideLegState::Blocked,
            "2026-06-27T00:00:00Z",
            Some(5000),
        ))];
        // scraper not waiting, but a live `blocked` still needs the operator.
        let badges = compute_badges_from_live(&groups, &[false], &[false], &inside_leg, now);
        assert_eq!(badges[0].needs_input, 1);
    }

    #[test]
    fn exited_pty_beats_live_inside_leg_working() {
        // Tier 1: a dead pane is never resurrected by a stale `working` (D4).
        let rows = make_rows(&[("wkA", "/a", "claude", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let now = crate::state::rfc3339_like_to_secs("2026-06-27T00:00:00Z").unwrap();
        let inside_leg = vec![Some(report_at(
            InsideLegState::Working,
            "2026-06-27T00:00:00Z",
            Some(5000),
        ))];
        let badges = compute_badges_from_live(&groups, &[false], &[true], &inside_leg, now);
        assert_eq!(badges[0].exited, 1);
        assert_eq!(badges[0].needs_input, 0);
    }

    #[test]
    fn inside_leg_done_yields_to_scraper() {
        // `done` = turn finished; the scraper decides idle vs waiting (tier 3).
        let rows = make_rows(&[("wkA", "/a", "claude", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let now = crate::state::rfc3339_like_to_secs("2026-06-27T00:00:00Z").unwrap();
        let done = vec![Some(report_at(
            InsideLegState::Done,
            "2026-06-27T00:00:00Z",
            Some(5000),
        ))];
        // scraper waiting -> needs_input passes through.
        let badges = compute_badges_from_live(&groups, &[true], &[false], &done, now);
        assert_eq!(badges[0].needs_input, 1);
        // scraper quiet -> no attention.
        let badges = compute_badges_from_live(&groups, &[false], &[false], &done, now);
        assert!(badges[0].is_empty());
    }

    #[test]
    fn needs_input_after_authority_honors_lattice() {
        // The `a` attention filter must use the resolved signal, not raw waiting
        // (codex P2): working suppresses a false scraper-waiting, blocked raises
        // even when the scraper is quiet, exited never needs input.
        let now = crate::state::rfc3339_like_to_secs("2026-06-27T00:00:00Z").unwrap();
        let recv = "2026-06-27T00:00:00Z";
        let inside_leg = vec![
            Some(report_at(InsideLegState::Working, recv, Some(5000))), // scraper waiting, but working
            Some(report_at(InsideLegState::Blocked, recv, Some(5000))), // scraper quiet, but blocked
            None, // scraper waiting, no inside-leg
            None, // exited
        ];
        let waiting = [true, false, true, false];
        let exited = [false, false, false, true];
        let got = needs_input_after_authority(&waiting, &exited, &inside_leg, now);
        assert_eq!(got, vec![false, true, true, false]);
    }

    // ── AC5-FR: sum invariant under churn ─────────────────────────────────

    #[test]
    fn ac5_fr_sum_invariant_under_churn() {
        // After several polls the sum must always equal in-scope count.
        for n in 0..=5 {
            let specs: Vec<(&str, &str, &str, &str)> = (0..n)
                .map(|i| {
                    let cwd = if i % 2 == 0 { "/even" } else { "/odd" };
                    ("agent", cwd, "codex", "live")
                })
                .collect();
            // Rebuild a proper row set (can't use make_rows directly due to lifetimes on the tuple).
            let rows: Vec<Value> = specs
                .iter()
                .enumerate()
                .map(|(i, (_, cwd, provider, status))| {
                    json!({ "name": format!("wk{i}"), "cwd": cwd, "provider": provider, "status": status })
                })
                .collect();
            let groups = group_by(&rows, GroupKey::Cwd);
            let total: usize = groups.iter().map(|g| g.members.len()).sum();
            assert_eq!(total, n, "sum invariant at n={n}");
        }
    }

    // ── Rail navigation ───────────────────────────────────────────────────

    #[test]
    fn rail_move_up_clamps_at_first_row() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"), // idx 0
            ("wkB", "/alpha", "codex", "live"), // idx 1
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.selected_agent_idx = Some(0);

        // Move up from first: clamps, no panic.
        let result = state.move_up(&groups);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn rail_move_down_clamps_at_last_row() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"), // idx 0
            ("wkB", "/alpha", "codex", "live"), // idx 1
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.selected_agent_idx = Some(1);

        // Move down from last: clamps, no panic.
        let result = state.move_down(&groups);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn rail_move_down_advances_selection() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"), // idx 0
            ("wkB", "/alpha", "codex", "live"), // idx 1
            ("wkC", "/alpha", "codex", "live"), // idx 2
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.selected_agent_idx = Some(0); // wkA

        let result = state.move_down(&groups);
        // After sorting by name, members are [wkA=idx0, wkB=idx1, wkC=idx2].
        assert_eq!(result, Some(1));
    }

    // gemini HIGH: move_down with NO selection must land on the FIRST member,
    // not skip to the second. move_up with no selection also lands on first.
    #[test]
    fn rail_move_from_none_selects_first_member() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"), // idx 0
            ("wkB", "/alpha", "codex", "live"), // idx 1
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);

        let mut down = RailState::new(GroupKey::Cwd);
        assert_eq!(down.selected_agent_idx, None);
        assert_eq!(
            down.move_down(&groups),
            Some(0),
            "move_down from None -> first member"
        );

        let mut up = RailState::new(GroupKey::Cwd);
        assert_eq!(
            up.move_up(&groups),
            Some(0),
            "move_up from None -> first member"
        );
    }

    // ── x-8a6a: duplicate-index (an agent in 2+ groups) selection ─────────────
    //
    // The squad view (x-5b3e) and the deferred sideline+squad union put the SAME
    // agent index in two visible groups. The old index cursor (`position(==sel)`,
    // first-match) snapped any later occurrence back to the first, leaving the
    // second copy unreachable and tiling/highlighting the wrong group. These
    // assert the occurrence-aware (group key, agent) cursor.

    /// Agent 0 ("x") recruited into squads A and B; 1 ("y") in A only, 2 ("z") in
    /// B only. Mirrors the multi-squad GroupKey::Squad view (name-keyed groups).
    fn dup_groups() -> Vec<Group> {
        vec![
            Group {
                header: "A".into(),
                key_value: "A".into(),
                members: vec![0, 1],
            },
            Group {
                header: "B".into(),
                key_value: "B".into(),
                members: vec![0, 2],
            },
        ]
    }

    #[test]
    fn ac4_edge_move_down_reaches_second_occurrence() {
        // The bug this plan exists to fix: walking down must reach B's copy of x
        // (the duplicate), not stick at A's copy. Flat order: A.x, A.y, B.x, B.z.
        let groups = dup_groups();
        let mut rs = RailState::new(GroupKey::Squad);
        rs.re_anchor(&groups); // A.x (pos 0)
        assert_eq!(
            (rs.selected_agent_idx, rs.selected_group_key.as_deref()),
            (Some(0), Some("A"))
        );
        rs.move_down(&groups); // A.y
        assert_eq!(
            (rs.selected_agent_idx, rs.selected_group_key.as_deref()),
            (Some(1), Some("A"))
        );
        rs.move_down(&groups); // B.x - the SECOND occurrence of x
        assert_eq!(
            (rs.selected_agent_idx, rs.selected_group_key.as_deref()),
            (Some(0), Some("B")),
            "reached x's second copy in group B, not snapped back to A"
        );
        rs.move_down(&groups); // B.z
        assert_eq!(
            (rs.selected_agent_idx, rs.selected_group_key.as_deref()),
            (Some(2), Some("B"))
        );
        rs.move_down(&groups); // clamp at last
        assert_eq!(
            rs.selected_agent_idx,
            Some(2),
            "clamps at the last occurrence"
        );
    }

    #[test]
    fn ac4_edge_move_up_returns_to_first_occurrence() {
        let groups = dup_groups();
        let mut rs = RailState::new(GroupKey::Squad);
        rs.re_anchor(&groups);
        rs.move_down(&groups); // A.y
        rs.move_down(&groups); // B.x
        assert_eq!(rs.selected_group_key.as_deref(), Some("B"));
        rs.move_up(&groups); // back to A.y, NOT stuck on the duplicate
        assert_eq!(
            (rs.selected_agent_idx, rs.selected_group_key.as_deref()),
            (Some(1), Some("A"))
        );
    }

    #[test]
    fn ac3_hp_selected_group_resolves_the_occupied_copy() {
        // selected_group must return the group the cursor sits in, not the first
        // group that happens to contain the agent (the GroupTile-wrong-group bug).
        let groups = dup_groups();
        let mut rs = RailState::new(GroupKey::Squad);
        rs.re_anchor(&groups);
        rs.move_down(&groups); // A.y
        rs.move_down(&groups); // B.x
        let g = rs
            .selected_group(&groups)
            .expect("a group holds the selection");
        assert_eq!(
            g.key_value, "B",
            "the SECOND copy resolves to its own group"
        );
    }

    #[test]
    fn highlight_only_selected_occurrence() {
        // The renderer accents only the occupied copy; the agent's other copy is
        // not highlighted (the "highlights every occurrence" fix).
        let groups = dup_groups();
        let (a, b) = (&groups[0], &groups[1]);
        let mut rs = RailState::new(GroupKey::Squad);
        rs.re_anchor(&groups);
        rs.move_down(&groups); // A.y
        rs.move_down(&groups); // B.x
        assert!(
            rs.is_selected_occurrence(b, 0),
            "B's x is the selected copy"
        );
        assert!(
            !rs.is_selected_occurrence(a, 0),
            "A's x (the other copy) is NOT accented"
        );
        assert!(
            !rs.is_selected_occurrence(a, 1),
            "an unselected member is not accented"
        );
    }

    #[test]
    fn re_anchor_clamps_when_occupied_group_removed() {
        // Cursor on B.x, then B is un-recruited away. x still exists in A, so
        // focus-follows-agent lands on A.x and never indexes out of range (ERR).
        let groups = dup_groups();
        let mut rs = RailState::new(GroupKey::Squad);
        rs.re_anchor(&groups);
        rs.move_down(&groups);
        rs.move_down(&groups); // B.x
        assert_eq!(rs.selected_group_key.as_deref(), Some("B"));
        let only_a = vec![Group {
            header: "A".into(),
            key_value: "A".into(),
            members: vec![0, 1],
        }];
        let sel = rs.re_anchor(&only_a);
        assert_eq!(sel, Some(0), "x follows into the surviving group A");
        assert_eq!(rs.selected_group_key.as_deref(), Some("A"));
    }

    // ── Focus axis transitions ────────────────────────────────────────────

    #[test]
    fn ac2_focus_axis_enter_and_exit_drive() {
        let rows = make_rows(&[("wkA", "/alpha", "codex", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.re_anchor(&groups);

        // Start in RailNav.
        assert_eq!(state.axis, FocusAxis::RailNav);

        // Enter drive.
        let entered = state.enter_drive();
        assert!(entered);
        assert_eq!(state.axis, FocusAxis::PaneDrive);

        // Exit drive (Esc).
        state.exit_drive();
        assert_eq!(state.axis, FocusAxis::RailNav);
    }

    #[test]
    fn ac2_err_drive_refused_when_no_selection() {
        let mut state = RailState::new(GroupKey::Cwd);
        assert_eq!(state.selected_agent_idx, None);

        // Drive refused when nothing is selected.
        let entered = state.enter_drive();
        assert!(!entered);
        assert_eq!(
            state.axis,
            FocusAxis::RailNav,
            "axis stays RailNav after refusal"
        );
    }

    // ── AC2-FR: driven agent exits -> axis auto-reverts to RailNav ────────
    #[test]
    fn ac2_fr_revert_to_nav_on_driven_exit() {
        let rows = make_rows(&[("wkA", "/alpha", "codex", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut state = RailState::new(GroupKey::Cwd);
        state.re_anchor(&groups);
        assert!(state.enter_drive());
        assert_eq!(state.axis, FocusAxis::PaneDrive);

        // The driven agent exits -> revert reports a real change.
        assert!(
            state.revert_to_nav(),
            "first revert from PaneDrive returns true"
        );
        assert_eq!(state.axis, FocusAxis::RailNav);

        // Idempotent: already in RailNav -> no-op, returns false.
        assert!(!state.revert_to_nav(), "revert in RailNav is a no-op");
        assert_eq!(state.axis, FocusAxis::RailNav);
    }

    // ── Group key labels ──────────────────────────────────────────────────

    #[test]
    fn group_key_labels_are_distinct() {
        let labels: Vec<&str> = [
            GroupKey::Cwd,
            GroupKey::Session,
            GroupKey::Provider,
            GroupKey::Status,
        ]
        .iter()
        .map(|k| k.label())
        .collect();
        // All distinct.
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "labels must be distinct");
                }
            }
        }
    }

    // ── US3: main-area mode (Single <-> GroupTile) ────────────────────────

    #[test]
    fn ac3_ui_toggle_flips_main_mode() {
        let mut rs = RailState::new(GroupKey::Cwd);
        assert_eq!(
            rs.main_mode,
            MainMode::GroupTile,
            "GroupTile (auto-tile) is the E5c default"
        );
        rs.toggle_main_mode();
        assert_eq!(rs.main_mode, MainMode::Single, "Tab zooms to a single pane");
        rs.toggle_main_mode();
        assert_eq!(
            rs.main_mode,
            MainMode::GroupTile,
            "Tab toggles back to GroupTile"
        );
    }

    #[test]
    fn ac_e5c_2_space_auto_tiles_members_by_default() {
        // E5c AC-2: two agents in one project (cwd) render as two tiled panes
        // within that space BY DEFAULT - GroupTile, no Tab toggle required.
        let rs = RailState::new(GroupKey::Cwd);
        assert_eq!(
            rs.main_mode,
            MainMode::GroupTile,
            "a space auto-tiles its members by default (E5c AC-2)"
        );
    }

    #[test]
    fn ac3_hp_selected_group_resolves_group_holding_selection() {
        let rows = make_rows(&[
            ("wkA", "/repo/alpha", "codex", "live"),
            ("wkB", "/repo/beta", "gemini", "live"),
            ("wkC", "/repo/alpha", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut rs = RailState::new(GroupKey::Cwd);

        // Select wkC (row index 2) -> its group is the alpha repo with 2 members.
        rs.selected_agent_idx = Some(2);
        let grp = rs.selected_group(&groups).expect("a group holds wkC");
        assert_eq!(grp.header, "alpha");
        assert_eq!(grp.key_value, "/repo/alpha");
        assert_eq!(grp.members.len(), 2, "alpha holds wkA + wkC");
        assert!(grp.members.contains(&2));
    }

    #[test]
    fn ac3_edge_selected_group_none_when_unselected_or_absent() {
        let rows = make_rows(&[("wkA", "/repo/alpha", "codex", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let mut rs = RailState::new(GroupKey::Cwd);

        // Nothing selected -> no group (caller must avoid compute(0)).
        assert!(rs.selected_group(&groups).is_none());

        // Selection points at a row absent from the partition -> None, no panic.
        rs.selected_agent_idx = Some(99);
        assert!(rs.selected_group(&groups).is_none());
    }

    #[test]
    fn ac3_err_selected_group_page_derives_from_selection() {
        // A 5-member group, capacity 2 per page -> 3 pages.
        let group = Group {
            header: "/g".into(),
            key_value: "/g".into(),
            members: vec![10, 11, 12, 13, 14],
        };
        let mut rs = RailState::new(GroupKey::Cwd);

        rs.selected_agent_idx = Some(10); // position 0 -> page 0
        assert_eq!(rs.selected_group_page(&group, 2), 0);
        rs.selected_agent_idx = Some(12); // position 2 -> page 1
        assert_eq!(rs.selected_group_page(&group, 2), 1);
        rs.selected_agent_idx = Some(14); // position 4 -> page 2
        assert_eq!(rs.selected_group_page(&group, 2), 2);

        // Selection absent / capacity 0 -> page 0, no panic.
        rs.selected_agent_idx = Some(99);
        assert_eq!(rs.selected_group_page(&group, 2), 0);
        rs.selected_agent_idx = Some(12);
        // capacity 0 is treated as 1, so position 2 -> page 2 (each member its own page).
        assert_eq!(
            rs.selected_group_page(&group, 0),
            2,
            "capacity 0 treated as 1"
        );
    }

    #[test]
    fn ac3_err_page_jump_moves_selection_by_capacity_clamped() {
        let group = Group {
            header: "/g".into(),
            key_value: "/g".into(),
            members: vec![10, 11, 12, 13, 14],
        };
        let mut rs = RailState::new(GroupKey::Cwd);
        rs.main_mode = MainMode::GroupTile;
        rs.selected_agent_idx = Some(10); // position 0

        // Forward by a page of 2: position 0 -> 2 -> 4 -> clamps at 4 (last).
        rs.page_jump(&group, true, 2);
        assert_eq!(rs.selected_agent_idx, Some(12));
        rs.page_jump(&group, true, 2);
        assert_eq!(rs.selected_agent_idx, Some(14));
        rs.page_jump(&group, true, 2); // mash past the end
        assert_eq!(
            rs.selected_agent_idx,
            Some(14),
            "forward clamps at last member"
        );

        // Backward by a page: 4 -> 2 -> 0 -> clamps at 0.
        rs.page_jump(&group, false, 2);
        assert_eq!(rs.selected_agent_idx, Some(12));
        rs.page_jump(&group, false, 2);
        rs.page_jump(&group, false, 2); // mash past the start
        assert_eq!(
            rs.selected_agent_idx,
            Some(10),
            "backward clamps at first member"
        );
    }

    #[test]
    fn ac4_fr_main_mode_survives_group_key_cycle() {
        // The chosen main_mode must persist across a `g` re-partition; only the
        // key changes. Toggle off the GroupTile default to prove a non-default
        // choice survives too.
        let mut rs = RailState::new(GroupKey::Cwd);
        assert_eq!(rs.main_mode, MainMode::GroupTile, "E5c default");
        rs.toggle_main_mode(); // -> Single
        assert_eq!(rs.main_mode, MainMode::Single);
        rs.cycle_group_key();
        assert_eq!(rs.group_key, GroupKey::Session);
        assert_eq!(
            rs.main_mode,
            MainMode::Single,
            "regroup keeps the chosen main_mode"
        );
    }

    // ── AC3-FR: live_members filters exited panes for GroupTile reflow ────────

    fn group_with_members(members: &[usize]) -> Group {
        Group {
            header: "/repo/alpha".to_string(),
            key_value: "/repo/alpha".to_string(),
            members: members.to_vec(),
        }
    }

    #[test]
    fn live_members_all_alive_is_identity() {
        let g = group_with_members(&[0, 1, 2]);
        let exited = vec![false, false, false];
        assert_eq!(live_members(&g, &exited), vec![0, 1, 2]);
    }

    #[test]
    fn live_members_drops_exited_and_preserves_order() {
        // Middle member (idx 1) exits; the tile reflows to survivors 0 and 2,
        // keeping name-stable order.
        let g = group_with_members(&[0, 1, 2]);
        let exited = vec![false, true, false];
        assert_eq!(live_members(&g, &exited), vec![0, 2]);
    }

    #[test]
    fn live_members_all_exited_is_empty() {
        // A wholly-dead group yields no tiles; the caller short-circuits before
        // calling layout::compute with pane_count 0 (AC1-EDGE / AC3-EDGE).
        let g = group_with_members(&[3, 4]);
        let exited = vec![false, false, false, true, true];
        assert!(live_members(&g, &exited).is_empty());
    }

    #[test]
    fn live_members_out_of_range_signal_treated_as_alive() {
        // Defensive: a member index beyond the `exited` slice is "not exited",
        // never an out-of-bounds panic.
        let g = group_with_members(&[0, 9]);
        let exited = vec![false]; // index 9 absent
        assert_eq!(live_members(&g, &exited), vec![0, 9]);
    }

    // ── Attention filter (`a`): attention_view + RailState toggle ─────────────

    #[test]
    fn attention_filter_defaults_off_and_toggles() {
        let mut rs = RailState::new(GroupKey::Cwd);
        assert!(!rs.attention_filter, "filter defaults off (show all)");
        rs.toggle_attention_filter();
        assert!(rs.attention_filter, "first toggle turns it on");
        rs.toggle_attention_filter();
        assert!(!rs.attention_filter, "second toggle turns it off");
    }

    #[test]
    fn attention_filter_persists_across_group_key_cycle() {
        // The filter is orthogonal to grouping: cycling the key must not clear it.
        let mut rs = RailState::new(GroupKey::Cwd);
        rs.toggle_attention_filter();
        rs.cycle_group_key();
        assert_eq!(rs.group_key, GroupKey::Session);
        assert!(rs.attention_filter, "regroup keeps the attention filter");
    }

    #[test]
    fn attention_view_keeps_only_waiting_members() {
        // Two groups; only some members are waiting. Waiting members survive,
        // order preserved; a group with no waiting member is dropped entirely.
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/alpha", "codex", "live"),
            ("wkC", "/beta", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        // wkB (idx 1) waiting in the alpha repo; nothing waiting in beta.
        let waiting = vec![false, true, false];
        let view = attention_view(&groups, &waiting);
        assert_eq!(view.len(), 1, "beta has no waiting member -> dropped");
        assert_eq!(view[0].header, "alpha");
        assert_eq!(view[0].members, vec![1], "only the waiting member survives");
    }

    #[test]
    fn attention_view_empty_when_nothing_waiting() {
        let rows = make_rows(&[
            ("wkA", "/alpha", "codex", "live"),
            ("wkB", "/beta", "codex", "live"),
        ]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let view = attention_view(&groups, &[false, false]);
        assert!(
            view.is_empty(),
            "no waiting agents -> empty view (rail shows empty-state)"
        );
    }

    #[test]
    fn attention_view_out_of_range_signal_treated_as_not_waiting() {
        // Defensive: a member index beyond the `waiting` slice is "not waiting",
        // never an out-of-bounds panic.
        let rows = make_rows(&[("wkA", "/alpha", "codex", "live")]);
        let groups = group_by(&rows, GroupKey::Cwd);
        let view = attention_view(&groups, &[]); // empty waiting slice
        assert!(view.is_empty(), "absent signal -> not waiting -> dropped");
    }
}
