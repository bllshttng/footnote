//! The sideline agents view's group-by axes (workspace / team / cwd /
//! status) and the manual-reorder editor behind the Manual sort column.
//! Split from client.rs under the file-budget gate; everything here reaches
//! the client's private items through `super::*`.

use super::*;

/// The team a row renders under: the row itself when teamed, else the first
/// teamed ancestor on its lineage (the same walk [`View::lead_label`]
/// runs). `None` = no teamed ancestor: the `~ unteamed` band.
pub(super) fn team_lead_name(view: &View, a: &AgentRow) -> Option<String> {
    if a.role_level.is_some() {
        return Some(a.name.clone());
    }
    let mut parent = lineage_parent(a);
    let mut steps = 0;
    while let Some(pid) = parent {
        if steps >= view.layout.agents.len() {
            return None;
        }
        let row = view
            .layout
            .agents
            .iter()
            .find(|r| r.harness_session_id.as_deref() == Some(pid))?;
        if row.role_level.is_some() {
            return Some(row.name.clone());
        }
        parent = lineage_parent(row);
        steps += 1;
    }
    None
}

/// The bucket label a row lands in on the active axis, shared by the row
/// builder and the reorder guard so the two cannot disagree about membership.
fn group_key_of(view: &View, name: &str) -> String {
    let Some(a) = view.layout.agents.iter().find(|a| a.name == name) else {
        return String::new();
    };
    match view.agent_group {
        AgentGroup::Workspace => view
            .layout
            .squads
            .iter()
            .find(|s| a.squad == Some(s.id))
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "~ elsewhere".into()),
        AgentGroup::Team => team_lead_name(view, a).unwrap_or_else(|| "~ unteamed".into()),
        AgentGroup::Cwd => a.cwd_base.clone().unwrap_or_else(|| "(no cwd)".into()),
        AgentGroup::Status => match agent_lattice_state(a) {
            LatticeState::Blocked | LatticeState::DoneUnseen => "Needs input".into(),
            LatticeState::Working | LatticeState::Unmeasured | LatticeState::Empty => {
                "Working".into()
            }
            LatticeState::Idle | LatticeState::Exited => "Completed".into(),
        },
    }
}

type Bucket<'a> = (String, Vec<&'a AgentRow>);

fn push<'a>(buckets: &mut Vec<Bucket<'a>>, key: String, a: &'a AgentRow) {
    match buckets.iter_mut().find(|(k, _)| *k == key) {
        Some((_, members)) => members.push(a),
        None => buckets.push((key, vec![a])),
    }
}

/// The row list on a non-workspace axis: one band per bucket, agents in
/// layout order inside it. The post-pass run sort (`sort_agent_runs`) orders
/// each band's contiguous agent run by the active column, exactly as it does
/// the workspace sections.
pub(super) fn grouped_rows_with_depths(view: &View) -> (Vec<DisplayRow<'_>>, Vec<usize>) {
    let buckets: Vec<Bucket<'_>> = match view.agent_group {
        AgentGroup::Workspace => Vec::new(), // unreachable: the caller guards
        AgentGroup::Team => {
            let mut buckets: Vec<Bucket<'_>> = Vec::new();
            for a in &view.layout.agents {
                let key = team_lead_name(view, a).unwrap_or_else(|| "~ unteamed".into());
                push(&mut buckets, key, a);
            }
            buckets
        }
        AgentGroup::Cwd => {
            let mut buckets: Vec<Bucket<'_>> = Vec::new();
            for a in &view.layout.agents {
                let key = a.cwd_base.clone().unwrap_or_else(|| "(no cwd)".into());
                push(&mut buckets, key, a);
            }
            buckets
        }
        AgentGroup::Status => {
            let mut needs: Vec<&AgentRow> = Vec::new();
            let mut working: Vec<&AgentRow> = Vec::new();
            let mut done: Vec<&AgentRow> = Vec::new();
            for a in &view.layout.agents {
                match agent_lattice_state(a) {
                    LatticeState::Blocked | LatticeState::DoneUnseen => needs.push(a),
                    LatticeState::Working | LatticeState::Unmeasured | LatticeState::Empty => {
                        working.push(a)
                    }
                    LatticeState::Idle | LatticeState::Exited => done.push(a),
                }
            }
            vec![
                ("Needs input".into(), needs),
                ("Working".into(), working),
                ("Completed".into(), done),
            ]
        }
    };
    emit(view, buckets)
}

fn emit<'a>(view: &'a View, buckets: Vec<Bucket<'a>>) -> (Vec<DisplayRow<'a>>, Vec<usize>) {
    let mut out: Vec<DisplayRow<'a>> = Vec::new();
    let mut depth_at: HashMap<usize, usize> = HashMap::new();
    for (label, members) in buckets.iter() {
        // An empty band renders nothing: a `Completed (0)` header is noise,
        // and the Status axis always builds all three buckets.
        if members.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(DisplayRow::Blank);
        }
        let key = SectionKey::Group(label.clone());
        let sview = view.section_view(&key);
        let rollup = section_rollup(members.iter().map(|a| agent_lattice_state(a)));
        out.push(DisplayRow::Header {
            label: label.clone(),
            rollup,
            key,
            view: sview,
        });
        if sview == SectionView::Collapsed {
            continue;
        }
        for a in members {
            if sview == SectionView::Expanded || !a.exited {
                depth_at.insert(out.len(), 0);
                out.push(DisplayRow::Agent(a));
            }
        }
    }
    let depths = (0..out.len())
        .map(|i| depth_at.get(&i).copied().unwrap_or(0))
        .collect();
    (out, depths)
}

/// Shift+up/down in the row selector: swap the focused agent with its
/// neighbour agent row and record the display sequence as the manual order,
/// switching the sort to Manual so the swap is visible at once. A band-edge
/// move in a group mode refuses: the band order is label-appearance order,
/// so a cross-band swap would be silently undone on the next paint.
pub(super) fn reorder_agent_rows(view: &mut View, cur: usize, delta: isize) {
    let rows = view.painted_rows();
    let name = match rows.get(cur) {
        Some(DisplayRow::Agent(a)) => a.name.clone(),
        _ => {
            view.set_notice("shift+arrow reorders an agent row".into());
            return;
        }
    };
    let dir = delta.signum();
    let mut j = cur as isize + dir;
    let neighbor = loop {
        if j < 0 || j as usize >= rows.len() {
            view.set_notice("no agent row that way".into());
            return;
        }
        if let DisplayRow::Agent(n) = &rows[j as usize] {
            break n.name.clone();
        }
        j += dir;
    };
    if view.agent_group != AgentGroup::Workspace
        && group_key_of(view, &name) != group_key_of(view, &neighbor)
    {
        view.set_notice("reorder stays inside one band".into());
        return;
    }
    let mut order: Vec<String> = rows
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(x) => Some(x.name.clone()),
            _ => None,
        })
        .collect();
    let (Some(i), Some(k)) = (
        order.iter().position(|n| *n == name),
        order.iter().position(|n| *n == neighbor),
    ) else {
        view.set_notice("row is not in the manual order yet".into());
        return;
    };
    order.swap(i, k);
    view.manual_order = order;
    view_store::save_manual_order(&view.manual_order);
    if view.agent_sort.column != AgentSortColumn::Manual {
        view.agent_sort = AgentSort {
            column: AgentSortColumn::Manual,
            direction: SortDirection::Ascending,
        };
        view_store::save_prefs(view.density, view.agent_sort);
        view.set_notice("sorted manually; shift+up/down reorders".into());
    }
}
