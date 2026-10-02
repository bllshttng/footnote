//! The shared open-session chooser: one builder every "open this session
//! here" affordance renders through (today the mail-header @sender tap;
//! sibling x-79e0 wires the Claude Code mod row). Every entry maps to a
//! `MenuAction` the row menu already executes, so Enter, digits, mouse and
//! Esc all come from the existing menu input paths with no new input code.

use super::*;

/// One open-chooser pick. Persisted in `mux-view.json` as its lowercase
/// name (the `feed_order` shape), so a choice survives a restart. `ModPane`
/// is deliberately unparseable from disk: it is disabled until the Claude
/// Code mod lands, and a disabled row never becomes the saved pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    Right,
    Left,
    Up,
    Down,
    Tab,
    ModPane,
}

impl OpenTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Left => "left",
            Self::Up => "up",
            Self::Down => "down",
            Self::Tab => "tab",
            Self::ModPane => "mod_pane",
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
    /// the disabled Mod pane entry, so New tab is index 4 of 5; anything
    /// unselectable (nothing saved, `ModPane`) lands on Split right.
    fn sel(self) -> usize {
        match self {
            Self::Left => 1,
            Self::Up => 2,
            Self::Down => 3,
            Self::Tab => 4,
            _ => 0,
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

/// The row owning `from_key`, by session identity - not by name: two rows
/// may share a name, only one owns the session.
fn row_for_session<'a>(rows: &'a [AgentRow], from_key: &str) -> Option<&'a AgentRow> {
    rows.iter()
        .find(|a| a.harness_session_id.as_deref() == Some(from_key))
}

/// Open the chooser on the row whose harness session is `resolved` - the
/// identity the tapped fmail id resolved to. Resolve failed or no row
/// carries it: a notice, and nothing opens. Never a guess by name.
pub(super) fn open_for_session(view: &mut View, id: &str, resolved: Option<String>) {
    let row = resolved.and_then(|k| row_for_session(&view.layout.agents, k.as_str()).cloned());
    let Some(row) = row else {
        view.set_notice(format!("sender {id}: no session found"));
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

/// Resolve a mail-sender id to the sender's bus session key by shelling the
/// `fno-agents chats resolve` verb: argv array, output captured, 5 s bound.
/// `None` on any failure - the caller shows `no session found` and never
/// guesses by name.
pub(super) fn resolve_sender(id: &str) -> Option<String> {
    const BOUND: std::time::Duration = std::time::Duration::from_secs(5);
    let mut child = std::process::Command::new(crate::digest_overlay::fno_agents_bin())
        .args(["chats", "resolve", "--prefix", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("from_key")?.as_str().map(str::to_string)
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
        // arrows, Enter and clicks cannot reach it, so a pick landing there
        // degrades to Split right and no action exists to mis-fire.
        let menu = build_open_chooser(&attachable(), Some(OpenTarget::ModPane)).unwrap();
        assert_eq!(menu.popup.sel, 0);
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

        // AC5-HP. Two rows share a name; only one owns the tapped session.
        // The finder returns the session owner, never the first name match.
        let mut twin_a = focus_agent(1);
        twin_a.name = "twin".into();
        twin_a.harness_session_id = Some("fmail-aaaaaaaaaaaa".into());
        let mut twin_b = focus_agent(2);
        twin_b.name = "twin".into();
        twin_b.harness_session_id = Some("fmail-bbbbbbbbbbbb".into());
        let rows = [twin_a, twin_b];
        let found = row_for_session(&rows, "fmail-bbbbbbbbbbbb").expect("the owner resolves");
        assert_eq!(found.pane_id, Some(2));
        assert!(
            row_for_session(&rows, "fmail-cccccccccccc").is_none(),
            "no session, no row"
        );
    }
}
