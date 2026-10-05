//! Where a node tap goes: the plan in Obsidian when it lives in the
//! configured vault, else the node in the backlog details pane.

use std::path::{Path, PathBuf};

use super::*;
use crate::link::PlanLink;

pub(super) async fn open(view: &mut View, id: String) {
    let plan = view
        .backlog
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| c.plan_path.clone());
    if let Some(plan) = plan {
        // Off-loop: the config read and a cold Obsidian launch must not
        // stall the render loop.
        let outcome = tokio::task::spawn_blocking(move || {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let cfg = crate::digest_overlay::ObsidianCfg::read(&cwd);
            match crate::link::plan_link(Some(Path::new(&plan)), &cfg) {
                PlanLink::Obsidian { uri } => Some(crate::link::open_fno_uri(&uri)),
                _ => None,
            }
        })
        .await
        .unwrap_or_else(|_| Some(Err("opener task failed".to_string())));
        match outcome {
            Some(Ok(())) => return view.set_notice(format!("opened the plan for {id}")),
            Some(Err(e)) => return view.set_notice(format!("open failed: {e}")),
            None => {}
        }
    }
    // The cascade's middle leg (x-4310 item 4): no vault plan opens the
    // GitHub-or-Linear link the node stores, through the PR tap's opener.
    if let Some(link) = view
        .backlog
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| c.link.clone())
    {
        return update_menu::open_pr(view, link).await;
    }
    open_detail(view, id);
}

/// The node id painted at `(row, col)`, from the spans the last compose
/// recorded - the one board-tap check, whichever backlog pane drew them.
pub(super) fn span_at(view: &View, row: u16, col: u16) -> Option<String> {
    let (row, col) = (row as usize, col as usize);
    view.node_spans
        .borrow()
        .iter()
        .find(|s| s.row == row && col >= s.col && col < s.col + s.len)
        .map(|s| s.id.clone())
}

/// The node in the backlog details pane. The pane lives on the
/// experimental board: off, the notice says where the jump would land. A
/// board already open keeps its gathered state - the tap opens the detail
/// ON it, never a fresh board that reads back to empty.
pub(super) fn open_detail(view: &mut View, id: String) {
    if !view.experimental_backlog {
        view.set_notice(format!(
            "node {id}: the backlog view is off (sideline menu)"
        ));
        return;
    }
    if view.backlog_board.is_none() {
        View::open(view);
    }
    if let Some(b) = view.backlog_board.as_mut() {
        b.detail = Some(node_detail::NodeDetailOverlay {
            node_id: id,
            trail: Vec::new(),
            sel: 0,
            scroll: 0,
        });
    }
}
