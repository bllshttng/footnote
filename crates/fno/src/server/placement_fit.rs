//! The `fit` placement (v80, x-ae47): the server picks the pane's tab -
//! moved out of server.rs (file budget shrink). Parent helpers resolve
//! through the glob.
use super::*;

/// The fit-vs-explicit-geometry refusal. `fit` hands the tab choice to the
/// server, so an explicit `tab`/`at`/`split`/`here`/portal contradicts it.
/// The control socket is reachable by any client, so the CLI gate covers one
/// caller and `run_pane` re-validates through here.
pub(crate) fn refuse_fit_with_geometry(placement: &PanePlacement) -> Option<(u32, String)> {
    if placement.fit
        && (placement.tab.is_some()
            || placement.at.is_some()
            || placement.split.is_some()
            || placement.here
            || placement.wants_portal())
    {
        return Some((
            err_code::BAD_REQUEST,
            "--fit selects its own tab and cannot be combined with --tab, --at, or --split"
                .to_string(),
        ));
    }
    None
}

impl Core {
    /// `fit` placement: the server picks the pane's tab. A squad-less route
    /// births the squad and its first tab (the same path a no-tab placement
    /// takes); a resolved squad takes the first tab in display order with
    /// room below `max_panes`, splitting at that tab's focus. No tab takes
    /// the pane -> a new tab, with `fell_back` true only when a tab WITH
    /// room refused the split for size, so the caller can tell "crowded
    /// tab" from the ordinary no-room mint.
    pub(super) fn place_with_fit(
        &mut self,
        dest: Option<u64>,
        squad_key: &str,
        pid: u64,
        placement: &PanePlacement,
    ) -> Result<(u64, TabId, bool), (u32, String)> {
        let Some(sid) = dest else {
            return self
                .place_spawned_pane(dest, squad_key, pid, None)
                .map_err(|e| (err_code::SPAWN_FAILED, e));
        };
        let Some(si) = self.session.squads.iter().position(|s| s.id == sid) else {
            self.reap_pane(pid);
            return Err((err_code::SPAWN_FAILED, "selected squad vanished".into()));
        };
        let mut refused_with_room = false;
        for ti in 0..self.session.squads[si].tabs.len() {
            if let Some(cap) = placement.max_panes {
                if tree::leaves(&self.session.squads[si].tabs[ti].root).len() >= cap {
                    continue;
                }
            }
            let tid = self.session.squads[si].tabs[ti].id;
            let vp = self.tab_rect(tid);
            let anchor = self.session.squads[si].tabs[ti].focus;
            let res = {
                let tab = &mut self.session.squads[si].tabs[ti];
                tree::split_at(tab, vp, anchor, Dir::Down, pid)
            };
            match res {
                Ok(()) => return Ok((sid, tid, false)),
                Err(tree::SplitError::TooSmall { .. }) => {
                    refused_with_room = true;
                }
                // A stale focus makes this tab unusable, not the spawn
                // fatal: skip it, and the mint below still lands the pane.
                Err(tree::SplitError::FocusNotFound(_)) => {}
            }
        }
        let tid = self.session.mint_tab_id();
        self.session.squads[si].tabs.push(Tab {
            name: None,
            id: tid,
            root: Node::Leaf(pid),
            focus: pid,
        });
        Ok((sid, tid, refused_with_room))
    }
}
