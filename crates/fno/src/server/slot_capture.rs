//! Capture-side pane -> slot naming: the persisted layout spec a live
//! tab becomes. Moved out of the parent under the file-budget gate; the
//! code the transient-view change touched moves with it.

use super::*;

/// Capture-side pane -> slot naming. Slot names are decided at
/// CAPTURE, never at restore, so two snapshots of one session agree on which
/// pane is which: a pane with an fno id names its slot that id and binds
/// `Fno(id)` (restore re-attaches it); a pane without one names itself
/// `p<ordinal>` and binds `Shell`. A duplicate attach id (the mirroring-ready
/// case `PaneLocation` documents) gets a `#2` suffix rather than colliding.
pub(super) struct SlotCapture<'a> {
    pane_owner: &'a HashMap<u64, &'a str>,
    /// Each pane's live cwd, read once before the tab loop.
    pane_cwd: &'a HashMap<u64, String>,
    /// Every live portal seat -> (index, row_key), read once before
    /// the tab loop. A seated leaf names its slot after the portal instead of
    /// an ordinal, so the capture keeps what restore needs to hold it again.
    portal_seats: &'a HashMap<u64, (u8, String)>,
    /// The live portals map and the registry snapshot, read once: a seated
    /// leaf's `PortalSlot` carries the row's harness and FULL session id (the
    /// fill guard) resolved through the same join the reach uses.
    portals: &'a BTreeMap<u8, Portal>,
    agents: &'a [crate::agents_view::RegistryAgent],
    /// The transient view panes: a leaf wearing the marker captures as an
    /// ordinal Shell slot (never an owner binding that restore would
    /// re-attach), and an all-transient tab is not stored at all.
    transient_views: &'a HashMap<u64, ()>,
    pub(super) slots: Vec<LayoutSlot>,
    by_pane: HashMap<u64, String>,
    ordinal: usize,
}

impl<'a> SlotCapture<'a> {
    pub(super) fn new(
        pane_owner: &'a HashMap<u64, &str>,
        pane_cwd: &'a HashMap<u64, String>,
        portal_seats: &'a HashMap<u64, (u8, String)>,
        portals: &'a BTreeMap<u8, Portal>,
        agents: &'a [crate::agents_view::RegistryAgent],
        transient_views: &'a HashMap<u64, ()>,
    ) -> Self {
        SlotCapture {
            pane_owner,
            pane_cwd,
            portal_seats,
            portals,
            agents,
            transient_views,
            slots: Vec::new(),
            by_pane: HashMap::new(),
            ordinal: 0,
        }
    }

    /// The live tree -> the persisted spec. Weights are renormalized on the
    /// way out rather than trusted: `tree::check_invariants` requires branch
    /// ratios summing to 1.0, but a stored document is untrusted input and
    /// geometry divides by the sum.
    pub(super) fn node_to_spec(&mut self, node: &Node) -> LayoutTreeSpec {
        match node {
            Node::Leaf(p) => LayoutTreeSpec::Slot(self.name_leaf(*p)),
            Node::Branch { axis, children } => {
                let weights: Vec<f32> = children.iter().map(|(w, _)| w.max(0.0)).collect();
                let sum: f32 = weights.iter().sum();
                let even = 1.0 / children.len() as f32;
                let children = children
                    .iter()
                    .zip(weights)
                    .map(|((_, n), w)| LayoutTreeChild {
                        weight: if sum > 0.0 { w / sum } else { even },
                        tree: self.node_to_spec(n),
                    })
                    .collect();
                LayoutTreeSpec::Split {
                    axis: *axis,
                    children,
                }
            }
        }
    }

    fn name_leaf(&mut self, pane: u64) -> String {
        // Order is transient, portal, owner, ordinal. A transient view
        // leaf captures as an ordinal Shell slot: restore may mint a
        // plain shell in its cell, but no owner binding survives that
        // would re-attach the row behind the operator's back.
        let base = if self.transient_views.contains_key(&pane) {
            self.ordinal += 1;
            format!("p{}", self.ordinal)
        } else {
            match self.portal_seats.get(&pane) {
                Some((index, _)) => format!("portal{index}"),
                None => match self.pane_owner.get(&pane) {
                    Some(id) => id.to_string(),
                    None => {
                        self.ordinal += 1;
                        format!("p{}", self.ordinal)
                    }
                },
            }
        };
        let mut name = base.clone();
        let mut n = 2;
        while self.slots.iter().any(|s| s.name == name) {
            name = format!("{base}#{n}");
            n += 1;
        }
        let binding =
            if self.transient_views.contains_key(&pane) || self.portal_seats.contains_key(&pane) {
                LayoutBinding::Shell
            } else {
                match self.pane_owner.get(&pane) {
                    Some(id) => LayoutBinding::Fno(id.to_string()),
                    None => LayoutBinding::Shell,
                }
            };
        let portal = self.portal_seats.get(&pane).map(|(index, row)| {
            // The row facts the fill guard reads back after a restart:
            // harness + FULL session id of the row the seat showed at
            // capture, resolved through the same agents snapshot the reach
            // itself used. A row that no longer resolves captures as None
            // and fills unguarded.
            let row_facts = crate::thread_viewer::row_for_pane(self.portals, pane, self.agents)
                .map(|agent| (agent.harness.clone(), agent.harness_session_id.clone()))
                .unwrap_or((None, None));
            PortalSlot {
                index: *index,
                row: row.clone(),
                harness: row_facts.0,
                session_id: row_facts.1,
            }
        });
        self.slots.push(LayoutSlot {
            name: name.clone(),
            binding,
            cwd: self.pane_cwd.get(&pane).cloned(),
            portal,
            // The restart join: the leaf remembers the pane id that lived
            // here, so restore can bind its re-adopted keeper twin.
            pane_id: Some(pane),
        });
        self.by_pane.insert(pane, name.clone());
        name
    }

    /// The slot name capture gave `pane` (for the persisted focus marker).
    pub(super) fn slot_of(&self, pane: u64) -> Option<String> {
        self.by_pane.get(&pane).cloned()
    }
}
