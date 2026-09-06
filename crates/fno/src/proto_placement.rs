//! The placement selectors: which squad, tab, and split a spawn asks for,
//! and the receipt the server hands back when the placement lands. Moved out
//! of proto.rs (file budget shrink) and re-exported there, so
//! `fno::proto::PanePlacement` and siblings keep their paths. The version
//! consts stay in proto.rs itself: the version-bump parity gate greps the
//! physical file for them.
#![allow(rustdoc::broken_intra_doc_links)]

use serde::{Deserialize, Serialize};

use super::{Dir, TabId};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PaneTarget {
    #[default]
    CurrentRoute,
    SquadName(String),
    SquadId(u64),
}

/// Which tab a [`PanePlacement`] / [`LayoutScope`] addresses within a squad
/// (v41, layout-api). `Index` is the 1-based ORDINAL the UI shows (`·N`,
/// x-1499): 1 is the first tab in display order and 0 is always refused. It
/// is an interactive convenience ONLY - ordinals renumber as tabs open and
/// close, so a script that captured one earlier may hit a different tab;
/// receipts return `Id`/`Name`, which are stable. `New` forces a fresh tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum TabSel {
    /// The squad's currently active tab.
    #[default]
    Active,
    /// The tab at 1-based display ordinal `n` (the UI's `·N`; 0 is refused).
    Index(usize),
    /// A stable tab id (preferred in scripts).
    Id(TabId),
    /// An operator-chosen tab name (preferred in scripts).
    Name(String),
    /// Force a brand-new tab.
    New,
}

/// What a [`PanePlacement`] does when the resolved anchor cannot take the
/// split (minimum size, stale anchor, selector conflict). The shipped default
/// `NewTab` preserves the legacy focused-relative fallback; `--at current`
/// (v44, x-6928) sets `Refuse` so exact origin placement never silently lands
/// in a fresh tab. `#[serde(default)]` keeps v43 placements wire-tolerant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlacementFallback {
    #[default]
    NewTab,
    Refuse,
}

/// Server-authored exact-placement receipt (v44, x-6928). Carries the
/// committed anchor/direction/fallback plus the squad/tab the split landed in,
/// so a `--at current` caller captures real identities instead of predicting
/// pane ids (AC1-UI). `None` on legacy focused-relative spawns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedPlacement {
    pub anchor: u64,
    pub direction: Dir,
    pub fallback: PlacementFallback,
    pub squad: u64,
    pub tab: TabId,
    /// (v51, x-1499) The landed tab's name and 1-based ordinal, so the
    /// human receipt can print `tab=<name-or-·N> tab_id=<id>` - the stable id
    /// alone names something the operator cannot find on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_ordinal: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PanePlacement {
    #[serde(default)]
    pub target: PaneTarget,
    #[serde(default)]
    pub split: Option<Dir>,
    /// (v31, x-9f75) Open-here: repoint the sender's focused pane at the target session rather than minting a
    /// tab/split. Valid only with the default `target` (CurrentRoute) and no `split`; the server refuses a
    /// conflicting combination. `#[serde(default)]` keeps v30 placements wire-tolerant.
    #[serde(default)]
    pub here: bool,
    /// (v41, layout-api) Which tab in the resolved squad to place into. `None`
    /// keeps the pre-v41 behavior (the squad's active tab, or a new one).
    #[serde(default)]
    pub tab: Option<TabSel>,
    /// (v41, layout-api) An anchor pane to place ADJACENT to (with `split`). The
    /// anchor must live in the resolved `tab`, else the server refuses with
    /// [`err_code::BAD_REQUEST`]. `None` keeps the pre-v41 whole-tab placement.
    #[serde(default)]
    pub at: Option<u64>,
    /// (v44, x-6928) Strict-placement policy. Default `NewTab` keeps the legacy
    /// fallback; `Refuse` (set by `--at current`) fails closed instead of
    /// substituting focus or minting a tab.
    #[serde(default)]
    pub fallback: PlacementFallback,
    /// Maximum leaves accepted in the resolved target tab before an exact
    /// split refuses. Absent on legacy and non-agent placement requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_panes: Option<usize>,
    /// (x-07c2) DEPRECATED by `portal` below, kept as the compatibility
    /// alias for one generation: `true` means portal 0. Read ONLY when
    /// `portal` is absent, and only through [`PanePlacement::portal_target`]
    /// so no code past the decode edge ever sees two fields that overlap.
    /// Drop it when `MIN_COMPAT_PROTO` passes 64, the version `portal`
    /// shipped in.
    #[serde(default)]
    pub thread_pane: bool,
    /// (v64, x-8f9d) Which portal this placement targets. `Some(n)` reaches
    /// portal n; `None` is no portal. A portal is the dedicated pane a
    /// thread is shown through, indexed from 0 and addressable at launch.
    /// No portal at n opens one (never persisted as a squad member), a
    /// portal at n on another row repoints it in place, a portal at n on
    /// this row focuses it. Mutually exclusive with `here`, `at`, `split`
    /// and a non-default `target` (a portal owns its geometry); the server
    /// refuses a conflicting combination. Additive and `#[serde(default)]`,
    /// so every existing placement stays wire-identical and the
    /// compatibility floor does not move (see `MIN_COMPAT_PROTO` above).
    #[serde(default)]
    pub portal: Option<u8>,
    /// (v64, x-8f9d) Open in the NEXT FREE portal, letting the SERVER pick the
    /// index. Set by the sideline's new-portal gesture, which knows it wants
    /// "another one" and not a particular number.
    ///
    /// The index cannot be chosen by the caller. Two clients computing it from
    /// the rows they last rendered both pick the same number, and the second
    /// reach silently repoints the first one's brand-new portal. The server
    /// processes reaches one at a time, so allocating there is atomic by
    /// construction.
    ///
    /// Ignored when `portal` names an index: an explicit address wins over
    /// "any". Additive and `#[serde(default)]`, so the floor does not move.
    #[serde(default)]
    pub portal_new: bool,
}

impl PanePlacement {
    /// (x-8f9d) The portal this placement targets, folding the deprecated
    /// `thread_pane` alias. `portal` wins; `thread_pane: true` resolves to
    /// portal 0, which is where every pre-v64 client always landed. This is
    /// the one normalisation, so callers read a single value.
    pub fn portal_target(&self) -> Option<u8> {
        self.portal
            .or(if self.thread_pane { Some(0) } else { None })
    }

    /// (x-8f9d) Does this placement ask for a portal at all, by any of the
    /// three spellings? `portal_target` answers WHICH index and cannot answer
    /// this one, because `portal_new` names no index - the server picks it.
    /// The geometry refusal and the routing decision both read this, so a
    /// new-portal reach is refused and routed on the same terms as an
    /// addressed one.
    pub fn wants_portal(&self) -> bool {
        self.portal_target().is_some() || self.portal_new
    }
}
