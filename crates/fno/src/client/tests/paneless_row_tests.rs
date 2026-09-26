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
