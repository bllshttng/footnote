//! How sideline agent rows order: the active column's comparator, applied to
//! each contiguous agent run and to each lineage level inside one, shared by
//! the card view and the extended table so both read in the sorted order and
//! the painted age is the value sorted on.

use super::*;

#[allow(clippy::type_complexity)]
pub(super) fn append_sorted_agent_group<'a>(
    out: &mut Vec<(DisplayRow<'a>, usize)>,
    group: &mut Vec<(Vec<(DisplayRow<'a>, usize)>, &'a AgentRow)>,
    sort: AgentSort,
    needs: &HashMap<String, NeedKind>,
    now_secs: u64,
) {
    let mut subtrees: Vec<(
        Vec<(Vec<(DisplayRow<'a>, usize)>, &'a AgentRow)>,
        &'a AgentRow,
    )> = Vec::new();
    for item in group.drain(..) {
        let depth = item.0.first().map(|(_, depth)| *depth).unwrap_or_default();
        if depth == 0 || subtrees.is_empty() {
            let root = item.1;
            subtrees.push((vec![item], root));
        } else {
            subtrees.last_mut().unwrap().0.push(item);
        }
    }
    subtrees.sort_by(|(_, a), (_, b)| {
        compare_agent_rows(
            a,
            b,
            sort,
            needs.get(a.name.as_str()).copied(),
            needs.get(b.name.as_str()).copied(),
            now_secs,
        )
    });
    for (items, _) in subtrees {
        let items = sort_lineage_level(items, sort, needs, now_secs);
        for (rows, _) in items {
            out.extend(rows);
        }
    }
}

/// Sorts one lineage level in place. `items[0]` is the level's root and keeps
/// its place; every later item starts (one level deeper) or continues (deeper
/// still) a child run, runs order by their leading agent, and each run's own
/// children sort the same way. Sublines ride inside their agent's item, so an
/// agent never separates from its detail rows.
fn sort_lineage_level<'a>(
    mut items: Vec<(Vec<(DisplayRow<'a>, usize)>, &'a AgentRow)>,
    sort: AgentSort,
    needs: &HashMap<String, NeedKind>,
    now_secs: u64,
) -> Vec<(Vec<(DisplayRow<'a>, usize)>, &'a AgentRow)> {
    let Some(root_depth) = items
        .first()
        .and_then(|(rows, _)| rows.first())
        .map(|(_, d)| *d)
    else {
        return items;
    };
    let mut runs: Vec<Vec<_>> = Vec::new();
    for item in items.drain(1..) {
        let depth = item.0.first().map(|(_, d)| *d).unwrap_or(root_depth);
        if depth <= root_depth + 1 || runs.is_empty() {
            runs.push(vec![item]);
        } else {
            runs.last_mut().expect("non-empty run above").push(item);
        }
    }
    runs.sort_by(|a, b| {
        compare_agent_rows(
            a[0].1,
            b[0].1,
            sort,
            needs.get(a[0].1.name.as_str()).copied(),
            needs.get(b[0].1.name.as_str()).copied(),
            now_secs,
        )
    });
    // The root survived `drain(1..)` at index 0; the sorted child runs
    // append after it, so the level keeps its head and its order.
    items.extend(
        runs.into_iter()
            .flat_map(|run| sort_lineage_level(run, sort, needs, now_secs)),
    );
    items
}

fn compare_agent_rows(
    a: &AgentRow,
    b: &AgentRow,
    sort: AgentSort,
    need_a: Option<NeedKind>,
    need_b: Option<NeedKind>,
    now_secs: u64,
) -> Ordering {
    let order = match sort.column {
        AgentSortColumn::Status => {
            let a_key = attention_key(a, need_a);
            let b_key = attention_key(b, need_b);
            let a_state = if a.exited {
                u8::MAX
            } else {
                pane_state(a.badge, a.seen, a.pane_activity) as u8
            };
            let b_state = if b.exited {
                u8::MAX
            } else {
                pane_state(b.badge, b.seen, b.pane_activity) as u8
            };
            apply_direction(
                a_state
                    .cmp(&b_state)
                    .then_with(|| a_key.0.cmp(&b_key.0))
                    .then_with(|| a_key.1.cmp(&b_key.1))
                    .then_with(|| a_key.2.cmp(&b_key.2)),
                sort.direction,
            )
        }
        AgentSortColumn::Agent => apply_direction(a.name.cmp(&b.name), sort.direction),
        AgentSortColumn::LastMessage => cmp_optional(
            a.tail.as_deref().filter(|value| !value.is_empty()),
            b.tail.as_deref().filter(|value| !value.is_empty()),
            sort.direction,
        ),
        AgentSortColumn::Pr => cmp_optional(a.pr, b.pr, sort.direction),
        AgentSortColumn::Age => {
            cmp_optional(row_age(a, now_secs), row_age(b, now_secs), sort.direction)
        }
    };
    order
}

fn cmp_optional<T: Ord>(a: Option<T>, b: Option<T>, direction: SortDirection) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => apply_direction(a.cmp(&b), direction),
    }
}

fn apply_direction(order: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => order,
        SortDirection::Descending => order.reverse(),
    }
}

fn row_age(a: &AgentRow, now_secs: u64) -> Option<u64> {
    a.last_activity_age_s
        .or_else(|| a.updated_at.map(|updated| now_secs.saturating_sub(updated)))
}
