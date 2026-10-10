//! The shared open-session chooser: one builder every "open this session
//! here" affordance renders through (today the mail-header @sender tap; the
//! Claude Code mod pane lands separately). Every entry maps to a
//! `MenuAction` the row menu already executes, so Enter, digits, mouse and
//! Esc all come from the existing menu input paths with no new input code.

use super::*;

/// One open-chooser pick. Persisted in `mux-view.json` as its lowercase
/// name (the `feed_order` shape), so a choice survives a restart. The
/// disabled Mod pane row carries no pick on purpose: nothing in the enum
/// parses as it, so it can never become the saved pick or a pre-selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    Right,
    Left,
    Up,
    Down,
    Tab,
}

impl OpenTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Left => "left",
            Self::Up => "up",
            Self::Down => "down",
            Self::Tab => "tab",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "right" => Self::Right,
            "left" => Self::Left,
            "up" => Self::Up,
            "down" => Self::Down,
            "tab" => Self::Tab,
            _ => return None,
        })
    }

    /// The flat target index of the row this pick pre-selects. Targets skip
    /// the disabled Mod pane entry, so New tab is index 4 of 5; Split right
    /// is the default.
    fn sel(self) -> usize {
        match self {
            Self::Left => 1,
            Self::Up => 2,
            Self::Down => 3,
            Self::Tab => 4,
            Self::Right => 0,
        }
    }
}

/// The persisted name of the pick an executed chooser action records.
/// `None` for any action the chooser never builds, so a row-menu action
/// that happens to share an arm never rewrites the saved pick.
pub(super) fn pick_of_action(action: MenuAction) -> Option<&'static str> {
    let pick = match action {
        MenuAction::Split(d) | MenuAction::MoveDir(d) | MenuAction::PortalAt(Some(d)) => match d {
            Dir::Right => OpenTarget::Right,
            Dir::Left => OpenTarget::Left,
            Dir::Up => OpenTarget::Up,
            Dir::Down => OpenTarget::Down,
        },
        MenuAction::NewTab | MenuAction::BreakOut | MenuAction::PortalAt(None) => OpenTarget::Tab,
        _ => return None,
    };
    Some(pick.as_str())
}

/// Build the open-session chooser for `agent`. The six entries map per the
/// row's LIVE state: a pane-hosted row moves its live pane (`MoveDir` /
/// `BreakOut`), a paneless attachable row attaches it (`Split` / `NewTab`),
/// and a paneless live thread with no attach id opens a fresh portal view
/// at the spot (`PortalAt`). `last` pre-selects the repeated pick. An
/// exited row refuses: there is nothing left to open.
pub(super) fn build_open_chooser(
    agent: &AgentRow,
    last: Option<OpenTarget>,
) -> Result<RowMenu, String> {
    if agent.exited {
        return Err(format!("{} has exited", agent.name));
    }
    let (splits, tab) = if agent.pane_id.is_some() {
        (
            [
                MenuAction::MoveDir(Dir::Right),
                MenuAction::MoveDir(Dir::Left),
                MenuAction::MoveDir(Dir::Up),
                MenuAction::MoveDir(Dir::Down),
            ],
            MenuAction::BreakOut,
        )
    } else if agent.attach_id.is_some() {
        (
            [
                MenuAction::Split(Dir::Right),
                MenuAction::Split(Dir::Left),
                MenuAction::Split(Dir::Up),
                MenuAction::Split(Dir::Down),
            ],
            MenuAction::NewTab,
        )
    } else {
        (
            [
                MenuAction::PortalAt(Some(Dir::Right)),
                MenuAction::PortalAt(Some(Dir::Left)),
                MenuAction::PortalAt(Some(Dir::Up)),
                MenuAction::PortalAt(Some(Dir::Down)),
            ],
            MenuAction::PortalAt(None),
        )
    };
    let mut rows: Vec<PopupRow> = vec![
        PopupRow::Header(format!("Open {}", agent.name)),
        PopupRow::Rule,
    ];
    let mut actions: Vec<MenuAction> = Vec::new();
    for (glyph, label, act) in [
        ("→", "Split right", splits[0]),
        ("←", "Split left", splits[1]),
        ("↑", "Split up", splits[2]),
        ("↓", "Split down", splits[3]),
        ("▤", "New tab", tab),
    ] {
        rows.push(PopupRow::Entry {
            glyph: glyph.into(),
            label: label.into(),
            hint: String::new(),
            enabled: true,
        });
        actions.push(act);
    }
    // Disabled until the Claude Code mod lands: a greyed entry carries no
    // target, so arrows, Enter and clicks can never reach the dead slot,
    // and it is never the pre-selected row.
    rows.push(PopupRow::Entry {
        glyph: "⊞".into(),
        label: "Mod pane (needs the Claude Code mod)".into(),
        hint: String::new(),
        enabled: false,
    });
    let mut menu = RowMenu {
        popup: Popup::new(rows, Anchor::Center),
        target: MenuTarget::OpenSession(AgentIdent::of(agent)),
        actions,
    };
    menu.popup.sel = last.map(OpenTarget::sel).unwrap_or(0);
    Ok(menu)
}

/// The row carrying `name`, by exact name. The tapped handle IS the row
/// name, so the tap opens the handle's own row; the registry keeps agent
/// names unique, so first match is exact match.
fn row_for_name<'a>(rows: &'a [AgentRow], name: &str) -> Option<&'a AgentRow> {
    rows.iter().find(|a| a.name == name)
}

/// Open the chooser on the row named `handle` - the tapped `@sender` header
/// token. No row carries the handle: a notice, and nothing opens.
pub(super) fn open_for_session(view: &mut View, handle: &str) {
    let Some(row) = row_for_name(&view.layout.agents, handle).cloned() else {
        view.set_notice(format!("sender {handle}: no session found"));
        return;
    };
    let last = crate::view_store::load_open_target().and_then(|s| OpenTarget::parse(&s));
    match build_open_chooser(&row, last) {
        Ok(menu) => {
            view.clear_peek();
            view.row_menu = Some(menu);
            view.row_menu_esc.clear();
        }
        Err(msg) => view.set_notice(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::focus_agent;
    use super::*;

    fn attachable() -> AgentRow {
        let mut a = focus_agent(3);
        a.pane_id = None;
        a.attach_id = Some("deadbee1".into());
        a
    }

    fn portal_thread() -> AgentRow {
        let mut a = focus_agent(3);
        a.pane_id = None;
        a
    }

    #[test]
    fn the_chooser_maps_row_state_remembers_the_pick_and_finds_by_session() {
        // AC3-HP. The last pick pre-selects: down sits on index 3, whose
        // action is Split down for an attachable row; nothing saved sits on
        // Split right.
        let menu = build_open_chooser(&attachable(), Some(OpenTarget::Down)).unwrap();
        assert_eq!(menu.popup.sel, 3);
        assert_eq!(menu.actions[3], MenuAction::Split(Dir::Down));
        let menu = build_open_chooser(&attachable(), None).unwrap();
        assert_eq!(menu.popup.sel, 0);
        assert_eq!(menu.actions[0], MenuAction::Split(Dir::Right));

        // AC4-ERR. A pane-hosted row MOVES its live pane; a paneless
        // attachable row ATTACHES it; a paneless live thread with no attach
        // id opens a fresh portal at the spot.
        let pane = build_open_chooser(&focus_agent(7), None).unwrap();
        assert_eq!(pane.actions[0], MenuAction::MoveDir(Dir::Right));
        assert_eq!(pane.actions[3], MenuAction::MoveDir(Dir::Down));
        assert_eq!(pane.actions[4], MenuAction::BreakOut);
        let thread = build_open_chooser(&portal_thread(), None).unwrap();
        assert_eq!(thread.actions[0], MenuAction::PortalAt(Some(Dir::Right)));
        assert_eq!(thread.actions[3], MenuAction::PortalAt(Some(Dir::Down)));
        assert_eq!(thread.actions[4], MenuAction::PortalAt(None));

        // AC4-ERR. The Mod pane entry draws greyed and carries no target:
        // arrows, Enter and clicks cannot reach it, and no disk spelling
        // parses as it, so it is never a saved pick nor a pre-selection.
        let menu = build_open_chooser(&attachable(), None).unwrap();
        let dead = menu
            .popup
            .rows
            .iter()
            .find(|r| matches!(r, PopupRow::Entry { label, .. } if label.starts_with("Mod pane")))
            .expect("the Mod pane entry renders");
        if let PopupRow::Entry { enabled, .. } = dead {
            assert!(!enabled, "the entry is greyed");
        }
        assert_eq!(menu.actions.len(), 5, "the dead entry carries no action");
        assert!(
            OpenTarget::parse("mod_pane").is_none(),
            "a disabled row never reads back as a saved pick"
        );

        // AC4-ERR. Nothing is left to open on an exited row: the builder
        // refuses, and the caller notices, naming the row.
        let mut dead_row = focus_agent(3);
        dead_row.exited = true;
        let err = match build_open_chooser(&dead_row, None) {
            Err(e) => e,
            Ok(_) => panic!("an exited row refuses the chooser"),
        };
        assert!(err.contains("has exited"), "the refusal names the state");
        assert!(err.contains(&dead_row.name), "the refusal names the row");

        // AC3-HP. Each chooser arm persists the pick it just executed; an
        // action the chooser never builds answers None.
        assert_eq!(pick_of_action(MenuAction::Split(Dir::Left)), Some("left"));
        assert_eq!(pick_of_action(MenuAction::MoveDir(Dir::Down)), Some("down"));
        assert_eq!(
            pick_of_action(MenuAction::PortalAt(Some(Dir::Up))),
            Some("up")
        );
        assert_eq!(pick_of_action(MenuAction::PortalAt(None)), Some("tab"));
        assert_eq!(pick_of_action(MenuAction::BreakOut), Some("tab"));
        assert_eq!(
            pick_of_action(MenuAction::Focus),
            None,
            "a non-chooser action never rewrites the pick"
        );
        // The disk spelling is the lowercase name; parse is its exact
        // inverse for every selectable pick.
        for pick in [
            OpenTarget::Right,
            OpenTarget::Left,
            OpenTarget::Up,
            OpenTarget::Down,
            OpenTarget::Tab,
        ] {
            assert_eq!(OpenTarget::parse(pick.as_str()), Some(pick));
        }

        // AC5-HP. The tapped handle opens the row carrying that exact name.
        let mut named = focus_agent(1);
        named.name = "t-glm-9663".into();
        let rows = [named, focus_agent(2)];
        let found = row_for_name(&rows, "t-glm-9663").expect("the handle resolves");
        assert_eq!(found.pane_id, Some(1));
        assert!(
            row_for_name(&rows, "t-none").is_none(),
            "no row carries the handle, no session"
        );
    }
}
