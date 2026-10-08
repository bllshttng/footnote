//! Where a live paneless thread row must appear: listed in the finder, and
//! painted or counted by a stated fold in the sideline, never silently gone.
//! Its own module because client_tests.rs is shrink-only under the file budget.
use super::*;

fn is_attach_at_portal_zero(hit: &ChromeHit, id: &str) -> bool {
    matches!(hit, ChromeHit::Cmds(c) if *c == vec![Command::AttachAgent {
        id: id.into(),
        placement: PanePlacement { portal: Some(0), ..Default::default() },
    }])
}

#[test]
fn finder_lists_a_live_paneless_row_as_an_attach() {
    let v = view_with_agents(vec![paneless_bg_row("thread-1")]);
    let rows = v.nav_rows();
    let row = rows
        .iter()
        .find(|r| r.label.contains("thread-1"))
        .expect("the finder lists the paneless row");
    assert!(is_attach_at_portal_zero(&row.hit, "job1"));
}

#[tokio::test]
async fn shift_arrow_runs_the_matching_split_cell() {
    // The split cells advertise shift+arrows under one group label (the
    // portal picker's gesture); this pins that the menu ANSWERS them:
    // shift+left folds to ShiftArrow and runs Split(Left) through the same
    // execute path Enter uses, closing the menu.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    assert!(v.open_row_menu(idx, Anchor::Center));
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, b"\x1b[1;2D", &mut buf).await.unwrap();
    assert!(
        v.row_menu.is_none(),
        "the shift+arrow split closes the menu"
    );
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert_eq!(placement.split, Some(Dir::Left));
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[test]
fn nine_live_paneless_rows_are_painted_or_counted() {
    let dir = isolate_view_store("paneless-nine");
    let agents = (0..9)
        .map(|i| paneless_bg_row(&format!("thread-{i}")))
        .collect();
    let v = view_with_agents(agents);
    let fold = idle_fold(&v);
    assert!(fold.is_some(), "the hidden rows are stated by a fold row");
    assert_eq!(
        rendered(&v, "thread-") + fold.map_or(0, |(hidden, _)| hidden),
        9,
        "every paneless row is painted or counted"
    );
    let rows = v.nav_rows();
    for i in 0..9 {
        let name = format!("thread-{i}");
        assert!(
            rows.iter().any(|r| r.label.contains(&name)),
            "a folded row stays findable: {name}"
        );
    }
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_live_paneless_orphan_is_counted_by_the_elsewhere_header() {
    let dir = isolate_view_store("paneless-orphan");
    let mut orphan = paneless_bg_row("stray-thread");
    orphan.squad = None;
    let v = view_with_agents(vec![orphan]);
    assert!(
        v.display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Header { key, .. } if *key == SectionKey::Elsewhere)),
        "the elsewhere header states the orphan"
    );
    let rows = v.nav_rows();
    let row = rows
        .iter()
        .find(|r| r.label.contains("stray-thread"))
        .expect("the finder lists the orphan while its section is collapsed");
    assert!(is_attach_at_portal_zero(&row.hit, "job1"));
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The top-K fold's target set after this branch split the old
/// blind `Idle` three ways. Every non-attention state must still fold: on
/// `origin/main` a badgeless row was `Idle` and folded, so folding only
/// `Idle` would strand both a pristine shell AND every badgeless bg
/// worker (`server.rs` hard-codes `pane_activity: None` on watch-only
/// paneless rows), driving `idle_budget` to zero on any real fleet. The
/// `?` glyph, not the fold, is what keeps a no-reading row honest.
#[test]
fn idle_fold_takes_every_non_attention_state() {
    let with = |activity: Option<ShellActivity>| {
        let mut r = agent_row("w", 1, None, true);
        r.pane_activity = activity;
        r
    };
    assert!(
        is_idle_row(&with(Some(ShellActivity::Empty))),
        "a pristine shell folds, else the fold cap dies on a shell-heavy squad"
    );
    assert!(is_idle_row(&with(Some(ShellActivity::Idle))), "idle folds");
    assert!(
        is_idle_row(&with(Some(ShellActivity::Unmeasured))),
        "an unmeasured row folds: the `?` glyph carries the honesty, not the cap"
    );
    assert!(
        is_idle_row(&with(None)),
        "a badgeless bg worker (pane_activity None) folds as it did before the split"
    );
    assert!(
        !is_idle_row(&with(Some(ShellActivity::Running))),
        "a running pane is attention, not fold"
    );
    let mut dead = with(Some(ShellActivity::Empty));
    dead.exited = true;
    assert!(
        !is_idle_row(&dead),
        "dead rows are the section view's business"
    );
}

// d-954c2cbf: every spawn appears in the sideline. The elsewhere section
// defaults Expanded, so a live orphan row paints without the operator
// expanding anything (x-3909).
#[test]
fn a_live_paneless_orphan_row_renders_without_expanding_elsewhere() {
    let dir = isolate_view_store("paneless-orphan-visible");
    let mut orphan = paneless_bg_row("stray-thread");
    orphan.squad = None;
    let v = view_with_agents(vec![orphan]);
    assert!(
        v.display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Agent(a) if a.name == "stray-thread")),
        "a live orphan renders by default, not behind the fold"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// The LiveOnly half of the same default: a majority-exited elsewhere section
// keeps its live rows up and folds the dead ones behind the rollup.
#[test]
fn a_majority_exited_elsewhere_section_folds_dead_rows_only() {
    let dir = isolate_view_store("paneless-orphan-liveonly");
    let mut live = paneless_bg_row("live-thread");
    live.squad = None;
    let mut stale = paneless_bg_row("stale-thread");
    stale.squad = None;
    stale.exited = true;
    let mut stale2 = paneless_bg_row("stale-thread-2");
    stale2.squad = None;
    stale2.exited = true;
    let v = view_with_agents(vec![stale, stale2, live]);
    let rows = v.display_rows();
    assert!(
        rows.iter()
            .any(|r| matches!(r, DisplayRow::Agent(a) if a.name == "live-thread")),
        "the live row stays up"
    );
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, DisplayRow::Agent(a) if a.name.starts_with("stale-thread"))),
        "dead rows fold behind the header rollup"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}
