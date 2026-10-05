//! The pure bounded window: pages land here, the cap trims here, and the
//! anchors (head, scan floor, end-of-history) move here. No rendering, no
//! IO - `apply_fold` translates a landed [`FeedPage`] into one [`FeedWindow::land`]
//! call and re-anchors the view around what this returns.

use crate::feed_overlay::{FeedItem, PageReq, FEED_PAGE};

/// Rows held: three pages. A landing that would exceed the cap drops rows
/// from the FAR end (the newest for an Older land, the oldest for a Newer
/// or Live land), so memory stays flat over a long scroll.
pub(crate) const FEED_MAX_ROWS: usize = 600;

/// What one land did, for the caller's re-anchoring and footer.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Land {
    /// Newest rows dropped to hold the cap (the head detached if this is 1+).
    pub dropped_newest: usize,
    /// Oldest rows dropped to hold the cap.
    pub dropped_oldest: usize,
}

/// The window's own state, kept next to the rows it bounds.
#[derive(Debug, Default)]
pub(crate) struct FeedWindow {
    /// Ascending: oldest first, the projection's order.
    pub items: Vec<FeedItem>,
    /// True while the operator sits at the newest row, so Live rows append
    /// in place instead of counting.
    pub head_attached: bool,
    /// The newest row ever seen (raw, matched or not).
    pub head_cursor: String,
    /// The oldest RAW row read. Under a client-matched query it can sit
    /// below the oldest kept row, which is what makes a page of matches
    /// smaller than a screen.
    pub scan_cursor: String,
    /// A short page from the Older direction: history's end.
    pub at_oldest: bool,
    /// Rows that landed above the detached head, unread.
    pub new_count: usize,
}

impl FeedWindow {
    /// Land one page. `rows` are already client-matched and ascending;
    /// `scan_floor` is the RAW page's oldest cursor, which the scan tracks
    /// even when the matcher kept none of the page. An empty page still
    /// moves the scan bounds: an empty answer is a real answer.
    pub fn land(&mut self, req: &PageReq, rows: Vec<FeedItem>, scan_floor: Option<String>) -> Land {
        let raw_newest = rows.last().map(|r| r.cursor.clone());
        let floor = scan_floor.or_else(|| rows.first().map(|r| r.cursor.clone()));
        let raw_len = rows.len();
        let mut land = Land::default();
        match req {
            PageReq::Head => {
                self.items = rows;
                self.head_attached = true;
                self.new_count = 0;
                self.at_oldest = raw_len < FEED_PAGE;
                if let Some(c) = raw_newest {
                    self.head_cursor = c;
                }
                if let Some(f) = floor {
                    self.scan_cursor = f;
                }
            }
            PageReq::Older(_) => {
                // Prepend, then drop the NEWEST past the cap: the operator is
                // reading old rows, so the far end (today) is what yields.
                let mut merged = rows;
                merged.append(&mut self.items);
                if merged.len() > FEED_MAX_ROWS {
                    land.dropped_newest = merged.len() - FEED_MAX_ROWS;
                    merged.truncate(FEED_MAX_ROWS);
                    self.head_attached = false;
                }
                self.items = merged;
                self.at_oldest = raw_len < FEED_PAGE;
                if let Some(f) = floor {
                    self.scan_cursor = f;
                }
            }
            PageReq::Newer(_) => {
                // Append, then drop the OLDEST past the cap: the operator is
                // reading forward, so yesterday yields. A short page means
                // the head is back in reach.
                self.items.extend(rows);
                if self.items.len() > FEED_MAX_ROWS {
                    let excess = self.items.len() - FEED_MAX_ROWS;
                    land.dropped_oldest = excess;
                    self.items.drain(0..excess);
                }
                if raw_len < FEED_PAGE {
                    self.head_attached = true;
                }
                if let Some(c) = raw_newest {
                    self.head_cursor = c;
                }
            }
            PageReq::Live(_) => {
                if self.head_attached {
                    self.items.extend(rows);
                    if self.items.len() > FEED_MAX_ROWS {
                        let excess = self.items.len() - FEED_MAX_ROWS;
                        land.dropped_oldest = excess;
                        self.items.drain(0..excess);
                    }
                    if let Some(c) = raw_newest {
                        self.head_cursor = c;
                    }
                } else {
                    // Detached: the rows exist above the operator's view.
                    // Count them; landing them would move the page.
                    self.new_count += raw_len;
                    if let Some(c) = raw_newest {
                        self.head_cursor = c;
                    }
                }
            }
        }
        land
    }

    /// Home: offset 0 when attached, else a fresh Head is wanted.
    pub fn home_is_local(&self) -> bool {
        self.head_attached
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn item(i: usize) -> FeedItem {
        FeedItem {
            ts: format!("2026-09-01T00:{:02}:00Z", i / 60),
            kind: format!("k{i:03}"),
            title: format!("row {i}"),
            cursor: format!(
                "[\"2026-09-01T00:{:02}:00Z\",\"k{i:03}\",\"\",\"\",\"\",\"row {i}\"]",
                i / 60
            ),
            area: "agents".into(),
            ..Default::default()
        }
    }

    fn page(from: usize, count: usize) -> Vec<FeedItem> {
        (from..from + count).map(item).collect()
    }

    pub(crate) fn head_replaces_and_bounds_at_history_end() {
        let mut w = FeedWindow::default();
        w.land(&PageReq::Head, page(400, FEED_PAGE), None);
        assert_eq!(w.items.len(), FEED_PAGE);
        assert!(w.head_attached);
        assert_eq!(w.head_cursor, item(400 + FEED_PAGE - 1).cursor);
        assert!(!w.at_oldest, "a full head page has history below it");
        let mut w3 = FeedWindow::default();
        w3.land(&PageReq::Head, page(400, 10), None);
        assert!(w3.at_oldest, "a short head page is history's end");
        _older_case();
        _newer_case();
        _live_case();
        let mut w2 = FeedWindow::default();
        w2.land(
            &PageReq::Head,
            page(400, FEED_PAGE),
            Some("[\"floor\"]".into()),
        );
        assert_eq!(w2.scan_cursor, "[\"floor\"]");
    }

    fn _older_case() {
        let mut w = FeedWindow::default();
        w.land(&PageReq::Head, page(400, FEED_PAGE), None);
        w.land(&PageReq::Older("x".into()), page(200, FEED_PAGE), None);
        assert_eq!(w.items.len(), 2 * FEED_PAGE);
        w.land(&PageReq::Older("y".into()), page(0, FEED_PAGE), None);
        assert_eq!(
            w.items.len(),
            3 * FEED_PAGE,
            "three full pages sit at the cap"
        );
        assert!(w.head_attached, "the cap only detaches on overflow");
        // The 601st row: the cap drops the newest, and the head detaches.
        w.land(&PageReq::Older("y2".into()), page(999, 1), None);
        assert_eq!(w.items.len(), FEED_MAX_ROWS);
        assert!(!w.head_attached, "the head detached at the cap");
        assert_eq!(w.items[0].kind, "k999", "the oldest rows lead");
        assert_eq!(
            w.items.last().unwrap().kind,
            item(FEED_MAX_ROWS - 2).kind,
            "the newest page's tail dropped"
        );
    }

    fn _newer_case() {
        let mut w = FeedWindow::default();
        w.land(&PageReq::Head, page(400, FEED_PAGE), None);
        w.land(&PageReq::Older("x".into()), page(200, FEED_PAGE), None);
        w.land(&PageReq::Older("y".into()), page(0, FEED_PAGE), None);
        w.land(&PageReq::Older("y2".into()), page(999, 1), None);
        assert!(!w.head_attached);
        // A short newer page: history caught up, the head reattaches.
        w.land(&PageReq::Newer("z".into()), page(599, 5), None);
        assert!(w.head_attached, "a short page reattaches the head");
        assert_eq!(w.items.len(), FEED_MAX_ROWS);
        assert_eq!(w.items[0].kind, item(4).kind, "the oldest dropped");
    }

    fn _live_case() {
        let mut w = FeedWindow::default();
        w.land(&PageReq::Head, page(400, FEED_PAGE), None);
        w.land(&PageReq::Live("x".into()), page(700, 2), None);
        assert_eq!(w.items.len(), FEED_PAGE + 2, "attached: rows append");
        assert_eq!(w.new_count, 0);
        w.land(&PageReq::Older("y".into()), page(200, FEED_PAGE), None);
        w.land(&PageReq::Older("y2".into()), page(0, FEED_PAGE), None);
        w.land(&PageReq::Older("y3".into()), page(999, 1), None);
        assert!(!w.head_attached, "overflow capped: the head detached");
        w.land(&PageReq::Live("z".into()), page(800, 3), None);
        assert_eq!(w.new_count, 3, "detached: the rows only count");
        assert_eq!(w.items.len(), FEED_MAX_ROWS, "the window did not move");
    }
}
