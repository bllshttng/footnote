//! The v1 card menu: float to top, defer, plan, open plan. The reorder
//! verbs route through `fno backlog` server-side; the mux never writes the
//! graph. Floated READY cards carry a "may dispatch" hint: the dispatcher can
//! pick one up in about a minute, and the guards it applies (containers,
//! batching, stale candidates, project scope) are not modeled here, so the
//! hint promises nothing.
//!
//! Moved out of `client.rs` with the Plan entry: the file is over budget and
//! may only shrink, and the menu is the card's question, not the client's.

use super::*;

/// The one card label, id first (backlog_view owns the shape); every client
/// paint site folds through it.
use crate::backlog_view::card_label;

pub(super) fn build_card_menu(
    card: &BacklogCard,
    obsidian: &crate::digest_overlay::ObsidianCfg,
    anchor: Anchor,
) -> RowMenu {
    let label = card_label(card);
    let float_hint = match card.state {
        CardState::Ready => "may dispatch",
        _ => "",
    };
    let mut rows = vec![
        PopupRow::Header(label.clone()),
        PopupRow::Rule,
        PopupRow::Entry {
            glyph: "▲".into(),
            label: "Float to top".into(),
            hint: float_hint.into(),
            enabled: true,
        },
        PopupRow::Entry {
            glyph: "⏸".into(),
            label: "Defer".into(),
            hint: String::new(),
            enabled: true,
        },
        PopupRow::Entry {
            glyph: "✎".into(),
            label: "Plan".into(),
            hint: "spawn a blueprint".into(),
            enabled: true,
        },
    ];
    let mut actions = vec![
        MenuAction::Backlog(BacklogVerb::RankTop),
        MenuAction::Backlog(BacklogVerb::Defer),
        MenuAction::PlanSpawn,
    ];
    // LD7: a node with no plan is greyed (state can change; the item will
    // apply later). Obsidian off is absent instead - no state change in this
    // menu can unlock it, so a permanently-greyed item would advertise a
    // capability nothing here can turn on.
    match crate::link::plan_link(card.plan_path.as_deref().map(Path::new), obsidian) {
        crate::link::PlanLink::Unavailable(crate::link::PlanUnavailable::NoPlan) => {
            rows.push(PopupRow::Entry {
                glyph: "▤".into(),
                label: "Open plan".into(),
                hint: "no plan".into(),
                enabled: false,
            });
            // Disabled: 0 cells, so no action slot - actions stays index-aligned
            // with Popup::targets(), never with rows.
        }
        crate::link::PlanLink::Unavailable(crate::link::PlanUnavailable::ObsidianOff) => {}
        crate::link::PlanLink::Obsidian { .. } => {
            rows.push(PopupRow::Entry {
                glyph: "▤".into(),
                label: "Open plan".into(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(MenuAction::OpenPlan);
        }
        crate::link::PlanLink::PlainFile(_) => {
            rows.push(PopupRow::Entry {
                glyph: "▤".into(),
                label: "Open plan (file)".into(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(MenuAction::OpenPlan);
        }
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Card(card.id.clone()),
        actions,
    }
}
