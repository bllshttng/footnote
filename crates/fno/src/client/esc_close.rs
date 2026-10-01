//! One tap check for every painted esc chip. The painters record each
//! esc-close span they put on screen (`chrome::close_chips_begin`), and a
//! left press on one presses Esc through the keyboard's own precedence list,
//! so a tap closes exactly what a pressed Esc closes, with no per-overlay
//! hit test to forget.

use super::*;

/// Whether `(row, col)` lands on an esc-close span the last frame painted.
pub(super) fn chip_at(view: &View, row: u16, col: u16) -> bool {
    let (row, col) = (row as usize, col as usize);
    view.close_chips
        .borrow()
        .iter()
        .any(|s| s.row == row && col >= s.col && col < s.col + s.len)
}

/// Press Esc for a tap on a chip: the overlay chain first, then the two
/// quiet-window releases the run loop would give a held lone Esc. A docked
/// column paints a chip without holding the keyboard, so when no overlay
/// takes the Esc the column takes it; an open but unfocused feed panel is
/// the same shape, and its chip closes the panel.
pub(super) async fn tap(
    view: &mut View,
    scanner: &mut Scanner,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut routed = overlay_keys::route(view, scanner, &[0x1b], sock_w).await;
    if routed.is_none() && (view.backlog_board.is_some() || view.org_board.is_some()) {
        view.region_owner = region_focus::RegionOwner::Board;
        routed = overlay_keys::route(view, scanner, &[0x1b], sock_w).await;
    }
    match routed {
        Some(Ok(StdinFlow::Detach)) => return Ok(StdinFlow::Detach),
        Some(Err(e)) => return Err(e),
        // An open but UNFOCUSED feed panel owns no keys, so the pressed Esc
        // had no reader anywhere above. The tap is a gesture on the panel's
        // own chip: it closes the panel.
        None if view.feed.is_some() => {
            feed_view::toggle(view, sock_w).await?;
            return Ok(StdinFlow::Continue);
        }
        _ => {}
    }
    if let StdinFlow::Detach = overlay_keys::flush_lone_esc(view, scanner, sock_w).await? {
        return Ok(StdinFlow::Detach);
    }
    if let Some(event) = scanner.flush_chord() {
        overlay_keys::flush_released_chord(view, event, sock_w).await?;
    }
    Ok(StdinFlow::Continue)
}

/// Compose, then tap the top layer's esc chip with a real SGR press and
/// release through `handle_stdin`. Returns how many ` esc ` chips the frame
/// painted. A chip sits on a top border, one space before its corner; a
/// footer's `esc close` or `esc cancel` words are close spans too, not chips.
#[cfg(test)]
pub(super) async fn tap_chip(v: &mut View) -> usize {
    let text = crate::vt::frame_text(&v.compose());
    let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
    let on_border = |s: &chrome::CloseSpan| {
        let c = lines.get(s.row).and_then(|l| l.get(s.col + s.len + 1));
        c.is_some_and(|c| ('\u{2500}'..='\u{257f}').contains(c))
    };
    let chips: Vec<chrome::CloseSpan> = v
        .close_chips
        .borrow()
        .iter()
        .copied()
        .filter(|s| s.len == 3 && on_border(s))
        .collect();
    let Some(s) = chips.last() else {
        return 0;
    };
    let (row, col) = (s.row + 1, s.col + 2);
    let (mut scanner, mut carry, mut buf) = (Scanner::default(), Vec::new(), Vec::new());
    for end in ['M', 'm'] {
        let report = format!("\x1b[<0;{col};{row}{end}");
        handle_stdin(v, &mut scanner, &mut carry, report.as_bytes(), &mut buf)
            .await
            .unwrap();
    }
    assert!(!v.modal_release_swallow, "the release ends the gesture");
    assert!(buf.is_empty(), "a tap sends nothing to a pane");
    chips.len()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{agent_row_at, blocked_row, view_with_agents};
    use super::*;

    type Open = Box<dyn Fn(&mut View)>;
    type Closed = Box<dyn Fn(&View) -> bool>;

    #[tokio::test]
    async fn every_painted_chip_closes_its_overlay() {
        let question = |v: &mut View| {
            v.questions_fold = Some(crate::needs_overlay::QuestionsFold {
                items: vec![super::super::tests::question_item(
                    "q-1",
                    &["a"],
                    Some(true),
                )],
                ..Default::default()
            });
            v.open_questions_list();
        };
        let walk: Vec<(&str, Open, Closed)> = vec![
            (
                "questions",
                Box::new(question),
                Box::new(|v| v.question_detail.is_none()),
            ),
            (
                "questions stacked",
                Box::new(move |v| {
                    v.term = (30, 80);
                    question(v)
                }),
                Box::new(|v| v.question_detail.is_none()),
            ),
            (
                "catch-up",
                Box::new(|v| v.digest = Some(vec!["catch-up".into()])),
                Box::new(|v| v.digest.is_none()),
            ),
            (
                "needs me",
                Box::new(|v| {
                    v.mine_fold = Some(Vec::new());
                    v.needs_fold = Some(Vec::new());
                    v.answers = Some(0);
                }),
                Box::new(|v| v.answers.is_none()),
            ),
            (
                "yard",
                Box::new(|v| {
                    v.yard = Some(YardSel {
                        sel: 0,
                        opened_at: Instant::now(),
                    })
                }),
                Box::new(|v| v.yard.is_none()),
            ),
            (
                "move to",
                Box::new(|v| v.open_move_to(1)),
                Box::new(|v| v.move_to.is_none()),
            ),
            (
                "attach",
                Box::new(|v| {
                    let squads = v.attach_dst_squads();
                    v.open_attach_place("c19cd2c3".into(), None, squads);
                }),
                Box::new(|v| v.attach_place.is_none()),
            ),
            (
                "portal",
                Box::new(|v| v.open_portal_pick("c19cd2c3".into())),
                Box::new(|v| v.portal_pick.is_none()),
            ),
            (
                "navigator",
                Box::new(|v| {
                    v.nav = Some(NavView {
                        query: String::new(),
                        state_filter: None,
                        cursor: 0,
                    })
                }),
                Box::new(|v| v.nav.is_none()),
            ),
            (
                "connections",
                Box::new(|v| v.connections = Some(crate::connections_view::ConnectionsView::new())),
                Box::new(|v| v.connections.is_none()),
            ),
            (
                "peek",
                Box::new(|v| {
                    let i = agent_row_at(v, |a| a.name == "w");
                    v.open_peek(i, "w".into());
                }),
                Box::new(|v| v.peek.is_none()),
            ),
            (
                "confirm",
                Box::new(|v| {
                    v.confirm = Some(ConfirmAction {
                        action: ConfirmKind::ReapAgents,
                        label: "reap".into(),
                    })
                }),
                Box::new(|v| v.confirm.is_none()),
            ),
            (
                "new workspace",
                Box::new(|v| v.open_create()),
                Box::new(|v| v.create.is_none()),
            ),
            (
                "rename",
                Box::new(|v| v.open_rename(RenameTarget::Tab(1))),
                Box::new(|v| v.rename.is_none()),
            ),
            (
                "recruit",
                Box::new(|v| {
                    v.marks.insert("a-1".into());
                    v.open_recruit();
                }),
                Box::new(|v| v.recruit.is_none() && v.marks.contains("a-1")),
            ),
            (
                "keys modal",
                Box::new(|v| v.open_keys_modal()),
                Box::new(|v| v.keys_modal.is_none()),
            ),
            (
                "row menu",
                Box::new(|v| {
                    let i = agent_row_at(v, |a| a.name == "w");
                    assert!(v.open_row_menu(i, Anchor::At { row: 1, col: 1 }));
                }),
                Box::new(|v| v.row_menu.is_none()),
            ),
            (
                "menu",
                Box::new(|v| v.open_sideline_menu(Anchor::Center)),
                Box::new(|v| v.aux.is_none()),
            ),
            (
                "settings",
                Box::new(|v| v.aux = Some(v.build_settings_modal())),
                Box::new(|v| v.aux.is_none()),
            ),
            (
                "update",
                Box::new(|v| v.aux = Some(update_menu::build_update_modal(None))),
                Box::new(|v| v.aux.is_none()),
            ),
            (
                "sweep",
                Box::new(|v| {
                    v.aux = Some(build_sweep_modal(&SweepCounts {
                        tabs: 1,
                        ..Default::default()
                    }))
                }),
                Box::new(|v| v.aux.is_none()),
            ),
            (
                "feed detail",
                Box::new(|v| {
                    let item: crate::feed_overlay::FeedItem = serde_json::from_str(
                        r#"{"ts":"2026-09-02T18:27:06Z","kind":"pr_created","title":"PR 1"}"#,
                    )
                    .unwrap();
                    v.feed_detail = Some(feed_detail::modal(v, item));
                }),
                Box::new(|v| v.feed_detail.is_none()),
            ),
            (
                "composer",
                Box::new(|v| {
                    agent_launcher::open_with(v, "/fno:target x-1".into(), None, "x-1".into())
                        .unwrap();
                }),
                Box::new(|v| v.launcher.is_none()),
            ),
        ];
        for (name, open, closed) in walk {
            let mut v = view_with_agents(vec![blocked_row("w", 10, None)]);
            v.term = (30, 100);
            open(&mut v);
            assert!(!closed(&v), "{name} opened");
            assert_eq!(tap_chip(&mut v).await, 1, "{name} paints one chip");
            assert!(closed(&v), "a tap on {name}'s chip closes it");
        }

        // Off the chip, a click inside the questions view reaches nothing
        // under it: the sideline selector stays shut.
        let mut v = view_with_agents(vec![blocked_row("w", 10, None)]);
        v.term = (30, 100);
        question(&mut v);
        let (mut scanner, mut carry, mut buf) = (Scanner::default(), Vec::new(), Vec::new());
        handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<0;3;3M", &mut buf)
            .await
            .unwrap();
        assert!(v.question_detail.is_some() && v.selector.is_none() && buf.is_empty());

        // A footer's `esc close` words are a close span too.
        let mut v = view_with_agents(vec![]);
        v.term = (30, 100);
        v.aux = Some(v.build_settings_modal());
        v.compose();
        let words = v.close_chips.borrow().iter().copied().find(|s| s.len > 3);
        let words = words.expect("the settings footer names esc close");
        let press = format!("\x1b[<0;{};{}M", words.col + 2, words.row + 1);
        handle_stdin(&mut v, &mut scanner, &mut carry, press.as_bytes(), &mut buf)
            .await
            .unwrap();
        assert!(v.aux.is_none(), "clicking `esc close` closes");

        // A family-B modal swallows an outside click without closing; a drag
        // after a chip press keeps the release latch until the left release.
        let mut v = view_with_agents(vec![]);
        v.confirm = Some(ConfirmAction {
            action: ConfirmKind::ReapAgents,
            label: "reap".into(),
        });
        handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<0;1;1M", &mut buf)
            .await
            .unwrap();
        assert!(v.confirm.is_some(), "an outside click never dismisses");
        v.compose();
        let chip = v.close_chips.borrow().iter().copied().find(|s| s.len == 3);
        let chip = chip.expect("confirm paints a chip");
        let (c, r) = (chip.col + 2, chip.row + 1);
        for (code, end, armed) in [(0, 'M', true), (32, 'M', true), (0, 'm', false)] {
            let report = format!("\x1b[<{code};{c};{r}{end}");
            handle_stdin(
                &mut v,
                &mut scanner,
                &mut carry,
                report.as_bytes(),
                &mut buf,
            )
            .await
            .unwrap();
            assert_eq!(v.modal_release_swallow, armed, "{report:?}");
        }
        assert!(v.confirm.is_none());
    }
}
