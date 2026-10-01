use crate::org_model::OrgTree;

#[derive(Debug, Clone)]
pub(crate) struct Placed {
    pub key: String,
    pub x: usize,
    pub y: usize,
    pub text: String,
}
#[derive(Debug, Clone)]
pub(crate) struct Edge {
    pub from: String,
    pub to: String,
}
#[derive(Debug, Clone, Default)]
pub(crate) struct Graph {
    pub boxes: Vec<Placed>,
    pub edges: Vec<Edge>,
}

pub(crate) fn unowned_key(agent: &crate::proto::AgentRow, index: usize) -> String {
    match crate::org_model::agent_key(agent) {
        Some(key) => format!("unowned:{key}:{:?}:{:?}", agent.pane_id, agent.attach_id),
        None => format!("unowned:unknown:{index}"),
    }
}

pub(crate) fn layout(tree: &OrgTree, width: usize) -> Graph {
    let mut graph = Graph::default();
    let column = (width / tree.leads.len().max(1)).max(44);
    let clipped = |text: String, max: usize| text.chars().take(max).collect::<String>();
    for (lead_index, lead) in tree.leads.iter().enumerate() {
        let x = lead_index * column;
        graph.boxes.push(Placed {
            key: format!("lead:{}", lead.holder.name),
            x,
            y: 0,
            text: clipped(
                format!("┌ {} L{} {} ┐", lead.holder.name, lead.level, lead.scope),
                column - 2,
            ),
        });
        let mut y = 2;
        for node in &lead.nodes {
            graph.boxes.push(Placed {
                key: format!("node:{}", node.view.card.id),
                x,
                y,
                text: clipped(
                    format!("├ {} {}", node.view.card.id, node.view.card.title),
                    20,
                ),
            });
            for blocked in &node.view.blocked_by {
                graph.edges.push(Edge {
                    from: format!("node:{}", node.view.card.id),
                    to: format!("node:{}", blocked.id),
                });
            }
            for (index, session) in node.current.iter().chain(&node.former).enumerate() {
                graph.boxes.push(Placed {
                    key: format!("session:{}:{index}", node.view.card.id),
                    x: x + 22,
                    y: y + index + 1,
                    text: clipped(
                        format!(
                            "└ {}",
                            session
                                .agent
                                .as_ref()
                                .map(|a| a.name.as_str())
                                .or(session.view.session_id.as_deref())
                                .unwrap_or("unobserved")
                        ),
                        column - 24,
                    ),
                });
            }
            y += node.current.len() + node.former.len() + 2;
        }
    }
    let x = tree.leads.len() * column;
    for (index, agent) in tree.unowned.iter().enumerate() {
        graph.boxes.push(Placed {
            key: unowned_key(agent, index),
            x,
            y: index + 1,
            text: clipped(format!("Unowned · {}", agent.name), column - 2),
        });
    }
    graph
}

pub(crate) fn lines(
    graph: &Graph,
    width: usize,
    height: usize,
    pan: (usize, usize),
    selected: Option<&str>,
) -> Vec<String> {
    let mut cells = vec![vec![' '; width]; height];
    let mut put = |x: usize, y: usize, text: &str| {
        if y < pan.1 || y - pan.1 >= height {
            return;
        }
        for (offset, ch) in text.chars().enumerate() {
            let xx = x + offset;
            if xx >= pan.0 && xx - pan.0 < width {
                cells[y - pan.1][xx - pan.0] = ch;
            }
        }
    };
    for placed in &graph.boxes {
        put(placed.x, placed.y, &placed.text);
        if selected == Some(placed.key.as_str()) {
            let offset = placed.text.chars().position(|c| c == ' ').unwrap_or(0);
            put(placed.x + offset, placed.y, "▶");
        }
    }
    for edge in &graph.edges {
        let from = graph.boxes.iter().find(|b| b.key == edge.from);
        let to = graph.boxes.iter().find(|b| b.key == edge.to);
        if let (Some(a), Some(b)) = (from, to) {
            let x = a.x.min(b.x).saturating_add(21);
            for y in a.y.min(b.y)..=a.y.max(b.y) {
                put(x, y, "│");
            }
            let left = a.x.min(b.x).saturating_add(21);
            let right = a.x.max(b.x).saturating_add(21);
            put(left, b.y, &"─".repeat(right - left + 1));
            put(b.x.saturating_add(21), b.y, "◀");
        }
    }
    cells
        .into_iter()
        .map(|row| row.into_iter().collect())
        .collect()
}
