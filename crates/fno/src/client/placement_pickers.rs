//! The sideline's placement pickers: which pane does this row land in?
//!
//! Two overlays share one shape because they share one interaction - a
//! numbered single-axis list with a cursor, a digit accelerator that jumps
//! the cursor, and a commit that resolves against the live layout:
//!
//! - [`AttachPlace`] (selector `p`): place an attachable row in a WORKSPACE,
//!   optionally with split geometry.
//! - [`PortalPick`] (selector `P`, x-9fd0): show a paneless live row through
//!   a PORTAL - an existing open one by index, or a new one the server
//!   numbers.
//!
//! Both used to live in client.rs, which is shrink-only under the per-file
//! budget; the portal picker is the addition that paid for the move.
//! client.rs keeps the stdin routing arms, the render branches, and the row
//! resolution that decides the picker can open.

use super::*;

/// The attach-placement picker state (selector `p`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachPlace {
    pub(crate) id: String,
    /// Index into `squads` of the highlighted destination. A CURSOR, not a
    /// squad id: the picker now moves like every other overlay, and an index
    /// cannot name a workspace that is not in the list the operator is reading.
    /// Staleness is still checked at commit, because the layout can change
    /// under an open picker.
    pub(crate) cursor: usize,
    /// Every non-mission workspace, uncapped. It used to be `.take(9)`, which
    /// silently dropped the 10th onward - at 14 squads five simply did not
    /// exist as far as the operator could tell.
    pub(crate) squads: Vec<u64>,
    pub(crate) esc: Vec<u8>,
}

impl AttachPlace {
    /// The selected destination squad id, or `None` when the list is empty.
    /// The one place `cursor` is turned back into an id, so no caller can
    /// index `squads` out of step with the drawn highlight.
    pub(crate) fn target(&self) -> Option<u64> {
        self.squads.get(self.cursor).copied()
    }
}

/// (x-9fd0) The portal-placement picker state: the attach id of the row being
/// placed and a cursor. Like [`AttachPlace`] the cursor is an index into the
/// DRAWN list, never a portal index - portal membership is derived per frame,
/// so a stored copy of the list would be the staleness defect; the commit
/// resolves the cursor against the rows the layout holds at that moment.
pub(crate) struct PortalPick {
    pub(crate) id: String,
    pub(crate) cursor: usize,
    pub(crate) esc: Vec<u8>,
}

impl View {
    /// Open the attach-placement picker on `squads` (non-empty; the caller
    /// reports its own no-workspace notice), starting on `owner`'s workspace if
    /// that is still a candidate, else the active one, else the first.
    ///
    /// The default is resolved HERE, not at the two call sites, for the same
    /// reason they share `attach_dst_squads`: the click door (`apply_hit`) and
    /// the keyboard door (`p`/Enter) each carried an identical copy of this
    /// resolution, which is the N-reachable-paths trap the list extraction was
    /// already fixing. One door's worth of logic, two doors.
    pub(crate) fn open_attach_place(&mut self, id: String, owner: Option<u64>, squads: Vec<u64>) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.create = None;
        self.rename = None;
        self.confirm = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.move_pick = None;
        self.portal_pick = None;
        self.clear_peek();
        let active = self.layout.active_squad;
        let cursor = owner
            .and_then(|sid| squads.iter().position(|s| *s == sid))
            .or_else(|| squads.iter().position(|s| *s == active))
            .unwrap_or(0);
        self.attach_place = Some(AttachPlace {
            id,
            cursor,
            squads,
            esc: Vec::new(),
        });
    }

    /// Open the portal-placement picker (x-9fd0) on `id`: one row per OPEN
    /// portal (index, tab, occupant), then a new-portal row, with the
    /// new-portal row PRE-SELECTED so `P` then Enter sends exactly what bare
    /// `P` sent when it allocated immediately (`portal_new`, the server picks
    /// the index) - and `P` `2` Enter places into portal 2. Same overlay-
    /// clearing discipline as [`View::open_attach_place`].
    pub(crate) fn open_portal_pick(&mut self, id: String) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.create = None;
        self.rename = None;
        self.confirm = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.move_pick = None;
        self.attach_place = None;
        self.clear_peek();
        // The new-portal row is the LAST row, so the cursor starts at the open
        // count - zero open portals leaves cursor 0, the only row there is.
        self.portal_pick = Some(PortalPick {
            id,
            cursor: self.open_portal_rows().len(),
            esc: Vec::new(),
        });
    }

    /// Build the attach-placement picker lines: a header, one row per candidate
    /// workspace, then a footer where each axis names its OWN keys.
    ///
    /// Rows past the ninth carry no number, because no digit reaches them. The
    /// drawn numbering therefore never lies about what a digit will do, which is
    /// the property that lets the list be uncapped and the digit accelerator
    /// stay nine-wide without the two contradicting each other.
    pub(crate) fn attach_place_lines(&self, picker: &AttachPlace) -> Vec<String> {
        const W: usize = 54;
        let mut lines = vec![pad_to(" attach placement", W)];
        for (i, &sid) in picker.squads.iter().enumerate() {
            let name = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == sid)
                .map(|s| s.name.as_str())
                .unwrap_or("(gone)");
            let marker = if i == picker.cursor { '›' } else { ' ' };
            // Only the first nine are digit-addressable; the rest get a blank
            // gutter rather than a number no key produces.
            let ord = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            lines.push(pad_to(&format!(" {marker} {ord} {name}"), W));
        }
        // Two lines, not three: enter/t and space/. are each one action under
        // two keys, so they collapse to one entry apiece. The split row spells
        // `shift+HJKL` rather than a bare `HJKL`, naming the modifier in words
        // so it reads as "hjkl, held with shift" rather than an unrelated set
        // of four capital-letter bindings.
        lines.push(pad_to(" hjkl/arrows move · 1-9 jump · shift+HJKL split", W));
        lines.push(pad_to(
            " enter/t new tab in › · space/. here · esc/q cancel",
            W,
        ));
        lines
    }

    /// (x-9fd0) The open portals as `(occupant row, portal index)`, in index
    /// order, derived per frame from the live layout - the same derivation the
    /// `◫N` sideline markers render from. Portal membership is a pointer
    /// relation the server recomputes per frame, so this is never stored on
    /// the picker: a stored copy is the staleness defect, and an open picker
    /// must reflect a portal that closed the moment the next frame lands.
    pub(crate) fn open_portal_rows(&self) -> Vec<(&AgentRow, u8)> {
        let mut rows: Vec<(&AgentRow, u8)> = self
            .layout
            .agents
            .iter()
            .filter_map(|a| a.portal.map(|p| (a, p)))
            .collect();
        rows.sort_by_key(|(_, p)| *p);
        rows
    }

    /// Build the portal-placement picker lines (x-9fd0): a header, one row per
    /// OPEN portal (list position, portal index, tab, occupant), then the
    /// new-portal row, then a footer where each axis names its OWN keys.
    ///
    /// The digit column addresses LIST POSITIONS (the accelerator jumps the
    /// cursor), so it carries the same never-lies discipline as the attach
    /// picker: rows past the ninth get a blank gutter, and the full portal
    /// index space stays reachable through the cursor keys the footer names -
    /// the index column shows the true `0..=255` address, the gutter only ever
    /// hides a jump no key produces.
    pub(crate) fn portal_pick_lines(&self, pick: &PortalPick) -> Vec<String> {
        // 62, not the attach picker's 54: the footer names four axes on ONE
        // line and pad_to would ellipsize it at 54.
        const W: usize = 62;
        let rows = self.open_portal_rows();
        let mut lines = vec![pad_to(" portal placement", W)];
        for (i, (a, idx)) in rows.iter().enumerate() {
            let marker = if i == pick.cursor { '›' } else { ' ' };
            let ord = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            let tab = match self.agent_tab_context(a.squad, a.tab) {
                Some(TabContext::Ordinal(n)) => format!("tab {n}  "),
                Some(TabContext::Named(name)) => format!("·{name}  "),
                None => String::new(),
            };
            lines.push(pad_to(
                &format!(" {marker} {ord} ◫{idx}  {tab}{}", a.name),
                W,
            ));
        }
        let marker = if pick.cursor == rows.len() {
            '›'
        } else {
            ' '
        };
        let ord = if rows.len() < 9 {
            (rows.len() + 1).to_string()
        } else {
            " ".into()
        };
        lines.push(pad_to(&format!(" {marker} {ord} +   new portal"), W));
        lines.push(pad_to(
            " hjkl/arrows move · 1-9 jump · enter place · esc/q cancel",
            W,
        ));
        lines
    }
}

/// The attach-placement picker's keys (selector `p`).
pub(crate) async fn attach_place_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let keys = {
        let Some(picker) = view.attach_place.as_mut() else {
            return Ok(StdinFlow::Continue);
        };
        let mut esc = std::mem::take(&mut picker.esc);
        let keys = fold_selector_keys(&mut esc, bytes);
        picker.esc = esc;
        keys
    };

    for key in keys {
        if (b'1'..=b'9').contains(&key) {
            // The digit accelerator now JUMPS THE CURSOR rather than setting a
            // separate selection, so the two axes cannot disagree about what is
            // selected. Out of range (fewer than N workspaces) is a BEL, not a
            // silent no-op: the operator asked for a row that is not there.
            let idx = (key - b'1') as usize;
            match view.attach_place.as_mut() {
                Some(picker) if idx < picker.squads.len() => picker.cursor = idx,
                _ => {
                    // Out of range DROPS THE REST OF THE READ, it does not just
                    // beep and carry on. Terminal input arrives in batches, so a
                    // fast `9L` on a three-workspace layout would otherwise BEL
                    // and then commit an immediate right-split into whatever the
                    // cursor was already on - a placement the operator never
                    // chose, from keys they typed believing row 9 existed. The
                    // remaining bytes were composed against a model that just
                    // proved wrong, so none of them may commit.
                    let _ = raw_out(b"\x07");
                    view.set_notice("no workspace at that number".into());
                    return Ok(StdinFlow::Continue);
                }
            }
            continue;
        }

        // Lowercase h/j/k/l MOVE the cursor. They used to commit the attach with
        // a split direction, which made a wrong guess a finalized placement
        // rather than a mis-set toggle - and because `fold_selector_keys`
        // rewrites the arrows to these same bytes irreversibly, pressing Down to
        // scan the list attached the agent. Cursor motion is what the arrows
        // already mean in every sibling overlay, so that is what they mean here.
        //
        // Split direction moves to UPPERCASE, which is not a new convention but
        // the mux's existing one: keys.rs binds lowercase hjkl to focus movement
        // and uppercase HJKL to resize. Lowercase navigates, uppercase acts on
        // geometry. This picker was the one overlay that broke that rule.
        let step = match key {
            b'k' | b'h' => Some(-1isize),
            b'j' | b'l' => Some(1isize),
            _ => None,
        };
        if let Some(delta) = step {
            if let Some(picker) = view.attach_place.as_mut() {
                let len = picker.squads.len();
                if len > 0 {
                    // Clamped, no wrap - same discipline as the navigator cursor.
                    let cur = picker.cursor.min(len - 1) as isize;
                    picker.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
                }
            }
            continue;
        }

        // Every commit key here acts on the CURSOR, except Space/`.`, which
        // never do. No key's meaning depends on whether the cursor has moved.
        //
        // Enter opens a new tab in the cursor-marked workspace; `t` is a named
        // alias for the same commit (operator ruling: a mouse-only door onto
        // workspace actions is unreachable for an operator whose right-click
        // never reaches the mux, so every commit here keeps a keyboard-only
        // alias). Space attaches HERE instead: repoint the
        // focused pane, ignoring the cursor by design, and let the server pick
        // swap-viewer vs take-over-idle-shell. `.` is kept as Space's alias
        // rather than freed, so the muscle memory from when `.` was the only
        // "here" binding still works.
        let (split, here) = match key {
            b'H' => (Some(Some(Dir::Left)), false),
            b'J' => (Some(Some(Dir::Down)), false),
            b'K' => (Some(Some(Dir::Up)), false),
            b'L' => (Some(Some(Dir::Right)), false),
            b'\r' | b'\n' | b't' => (Some(None), false),
            b' ' | b'.' => (Some(None), true),
            0x1b | b'q' => {
                view.attach_place = None;
                return Ok(StdinFlow::Continue);
            }
            _ => (None, false),
        };
        let Some(split) = split else { continue };
        let picker = view.attach_place.take().unwrap();
        let attachable = view.layout.agents.iter().any(|a| {
            a.pane_id.is_none() && !a.exited && a.attach_id.as_deref() == Some(picker.id.as_str())
        });
        if !attachable {
            view.set_notice("agent is no longer attachable".into());
            return Ok(StdinFlow::Continue);
        }
        // Here is route-anchored - it never touches the cursor-selected
        // workspace, so a vanished target must not block it (only split/new-tab).
        // The staleness check still runs at commit, because the layout can shift
        // under an open picker; the cursor only guarantees the index is in range
        // of the list as drawn, not that the squad still exists.
        let dst = picker.target();
        if !here && !dst.is_some_and(|sid| view.layout.squads.iter().any(|s| s.id == sid)) {
            view.set_notice("workspace is no longer available".into());
            return Ok(StdinFlow::Continue);
        }
        write_msg(
            sock_w,
            &ClientMsg::Command(Command::AttachAgent {
                id: picker.id,
                placement: PanePlacement {
                    portal_new: false,
                    target: match dst.filter(|_| !here) {
                        Some(sid) => PaneTarget::SquadId(sid),
                        None => PaneTarget::CurrentRoute,
                    },
                    split,
                    here,
                    tab: None,
                    at: None,
                    fallback: PlacementFallback::NewTab,
                    max_panes: None,
                    thread_pane: false,
                    portal: None,
                },
            }),
        )
        .await
        .map_err(|e| format!("attach placement send failed: {e}"))?;
        return Ok(StdinFlow::Continue);
    }
    Ok(StdinFlow::Continue)
}

/// (x-9fd0) The portal picker's keys, mirrored on [`attach_place_keys`] so one
/// interaction never grows a second vocabulary: lowercase hjkl and the arrows
/// MOVE the cursor, `1`-`9` jump the cursor to that list row, Enter commits,
/// esc/q cancels. The list is re-derived per key from the live layout, so a
/// portal that closed under the open picker is gone from the rows the very
/// next keypress - and the commit - sees.
pub(crate) async fn portal_pick_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let keys = {
        let Some(pick) = view.portal_pick.as_mut() else {
            return Ok(StdinFlow::Continue);
        };
        let mut esc = std::mem::take(&mut pick.esc);
        let keys = fold_selector_keys(&mut esc, bytes);
        pick.esc = esc;
        keys
    };

    for key in keys {
        // Open portals + the new-portal row, as drawn NOW - never the list as
        // it stood when the picker opened.
        let len = view.open_portal_rows().len() + 1;
        if (b'1'..=b'9').contains(&key) {
            let idx = (key - b'1') as usize;
            match view.portal_pick.as_mut() {
                Some(pick) if idx < len => pick.cursor = idx,
                _ => {
                    // Out of range DROPS THE REST OF THE READ, it does not just
                    // beep and carry on. Terminal input arrives in batches, so a
                    // fast `9j` on a four-row list would otherwise BEL and then
                    // move onto a row the operator never chose - from keys they
                    // typed believing row 9 existed. The remaining bytes were
                    // composed against a model that just proved wrong, so none
                    // of them may commit.
                    let _ = raw_out(b"\x07");
                    view.set_notice("no portal at that number".into());
                    return Ok(StdinFlow::Continue);
                }
            }
            continue;
        }

        // Lowercase h/j/k/l MOVE the cursor, the same rule the attach picker
        // settled on: `fold_selector_keys` rewrites the arrows to these same
        // bytes irreversibly, so scanning the list must never commit.
        let step = match key {
            b'k' | b'h' => Some(-1isize),
            b'j' | b'l' => Some(1isize),
            _ => None,
        };
        if let Some(delta) = step {
            if let Some(pick) = view.portal_pick.as_mut() {
                if len > 0 {
                    // Clamped, no wrap - same discipline as the navigator cursor.
                    let cur = pick.cursor.min(len - 1) as isize;
                    pick.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
                }
            }
            continue;
        }

        match key {
            0x1b | b'q' => {
                view.portal_pick = None;
                return Ok(StdinFlow::Continue);
            }
            b'\r' | b'\n' => {
                let pick = view.portal_pick.take().unwrap();
                // The agent is re-checked at commit, the way the attach picker
                // re-checks its target: the row can exit or gain a pane while
                // the picker sits open, and the server's stale-id refusal must
                // not be the first thing that tells the operator.
                let attachable = view.layout.agents.iter().any(|a| {
                    a.pane_id.is_none()
                        && !a.exited
                        && a.attach_id
                            .as_deref()
                            .map(|s| s == pick.id.as_str())
                            .unwrap_or(a.name == pick.id)
                });
                if !attachable {
                    view.set_notice("agent is no longer attachable".into());
                    return Ok(StdinFlow::Continue);
                }
                // Resolve the cursor against the CURRENT portal set: the list
                // is derived per frame, so the cursor only guarantees a row in
                // the list as drawn now. A commit past the new-portal row means
                // portals closed under the picker; refuse rather than act on
                // the stale list (AC5-EDGE), exactly the attach picker's
                // "workspace is no longer available" shape.
                let rows = view.open_portal_rows();
                let placement = if pick.cursor < rows.len() {
                    PanePlacement {
                        portal: Some(rows[pick.cursor].1),
                        ..PanePlacement::default()
                    }
                } else if pick.cursor == rows.len() {
                    PanePlacement {
                        portal_new: true,
                        ..PanePlacement::default()
                    }
                } else {
                    view.set_notice("portal is no longer available".into());
                    return Ok(StdinFlow::Continue);
                };
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::AttachAgent {
                        id: pick.id,
                        placement,
                    }),
                )
                .await
                .map_err(|e| format!("portal placement send failed: {e}"))?;
                return Ok(StdinFlow::Continue);
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}
