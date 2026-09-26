//! Pane id reveal: the transient `pane <id>` label over each live pane.
//!
//! Lifted whole from client_tests.rs when the file went over budget; a
//! child module sees the file-local helpers and the file-top imports
//! through `use super::*`.

use super::*;

#[test]
fn pane_id_reveal_labels_each_id_inside_its_own_rectangle() {
    let mut view = two_pane_view();
    let t0 = Instant::now();
    view.reveal_pane_ids_at(t0);
    let frame = view.compose_at(t0 + Duration::from_millis(100));
    let cols = view.term.1 as usize;
    for (pid, rect) in &view.layout.panes {
        let label = format!("pane {pid}");
        // A framed pane anchors the label inside its CONTENT rect, one cell
        // in from the rect's ring on every side.
        let start = view.panel_w() as usize + rect.x as usize + 1 + (rect.cols as usize - 2)
            - label.chars().count();
        let row = TAB_BAR_ROWS as usize + rect.y as usize + 1;
        let painted: String = label
            .chars()
            .enumerate()
            .map(|(offset, _)| frame.cells[row * cols + start + offset].c)
            .collect();
        assert_eq!(painted, label, "pane {pid} label is not in its rectangle");
        assert!(start >= view.panel_w() as usize + rect.x as usize + 1);
        assert!(
            start + label.chars().count()
                <= view.panel_w() as usize + rect.x as usize + rect.cols as usize - 1
        );
    }
}

#[test]
fn pane_id_reveal_tracks_tab_layout_and_expires_without_layout_space() {
    let mut view = two_pane_view();
    let t0 = Instant::now();
    view.reveal_pane_ids_at(t0);
    let first = frame_text(&view.compose_at(t0 + Duration::from_millis(100)));
    assert!(first.contains("pane 10"));
    assert!(first.contains("pane 11"));

    view.layout.panes = vec![
        (
            91,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 35,
            },
        ),
        (
            94,
            Rect {
                x: 36,
                y: 0,
                rows: 29,
                cols: 36,
            },
        ),
    ];
    view.frames.insert(91, text_frame(29, 35, 'c'));
    view.frames.insert(94, text_frame(29, 36, 'd'));
    let second = frame_text(&view.compose_at(t0 + Duration::from_millis(200)));
    assert!(second.contains("pane 91"));
    assert!(second.contains("pane 94"));
    assert!(!second.contains("pane 10"));
    assert!(!second.contains("pane 11"));

    let expired =
        frame_text(&view.compose_at(t0 + PANE_ID_REVEAL_WINDOW + Duration::from_millis(1)));
    assert!(!expired.contains("pane 91"));
    assert!(!expired.contains("pane 94"));
}

#[test]
fn pane_id_reveal_skips_only_a_rectangle_too_narrow_for_its_label() {
    let mut view = two_pane_view();
    view.layout.panes[1].1.cols = 5;
    let t0 = Instant::now();
    view.reveal_pane_ids_at(t0);
    let frame = frame_text(&view.compose_at(t0 + Duration::from_millis(1)));
    assert!(frame.contains("pane 10"));
    assert!(!frame.contains("pane 11"));
}
