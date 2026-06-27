//! Tile layout manager (ab-3c063856, Wave 2.2).
//!
//! Computes a row-major grid of [`TileRect`]s from the current TTY size and
//! the live pane count. Even-grid tiling is the v1 choice (Claude's
//! Discretion #2); aspect-aware packing is deferred. The footer line
//! (mode indicator + overflow label) lives at the bottom of the screen and
//! is reserved here so panes never overdraw it.
//!
//! ## Boundaries this module owns
//!
//! - **Minimum pane size.** A pane smaller than [`MIN_PANE_ROWS`] x
//!   [`MIN_PANE_COLS`] is illegible. If the terminal cannot accommodate one
//!   minimum pane plus a footer row, [`compute`] returns
//!   [`LayoutError::TerminalTooSmall`] - the compositor surfaces that to
//!   the operator instead of corrupting paint (Failure Modes / Boundaries).
//! - **Overflow.** Once the terminal cannot fit `pane_count` panes at
//!   minimum size, the surplus panes are dropped from the tile list and
//!   counted into [`Layout::overflow`] so the footer can render the
//!   `+k more` indicator (Failure Modes / Boundaries; full pagination is a
//!   tracked follow-up).
//! - **Recomputation.** This module is pure: SIGWINCH wiring lives in the
//!   compositor run loop (Wave 5.1); it calls [`compute`] again with the
//!   new TTY size and replaces the cached [`Layout`].
//!
//! ## Acceptance Criteria
//!
//! - **AC1-EDGE:** more agents than fit at min pane size render the
//!   maximum that does fit, with `+k more` indicated.
//! - **AC5-HP:** a resize re-tiles to the new TTY size without visual
//!   corruption (the recompute is deterministic; the painter does the
//!   redraw).
//! - **AC5-UI:** pane borders + titles redraw to the new tile rects and
//!   the footer stays anchored at the bottom (the footer's row is always
//!   `tty.rows - FOOTER_ROWS`).

/// Minimum legible pane height in rows. Below this the borders + a single
/// content line + the cursor cannot coexist.
pub const MIN_PANE_ROWS: u16 = 6;

/// Minimum legible pane width in columns. Below this even the pane title
/// truncates aggressively.
pub const MIN_PANE_COLS: u16 = 20;

/// Rows reserved for the global footer (mode indicator + overflow). One
/// row is sufficient for `WATCH - Enter to drive · +k more`.
pub const FOOTER_ROWS: u16 = 1;

/// The compositor's current view of the terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtySize {
    pub rows: u16,
    pub cols: u16,
}

impl TtySize {
    pub fn new(rows: u16, cols: u16) -> Self {
        TtySize { rows, cols }
    }
}

/// A single tile within the grid, in absolute terminal coordinates. `(0, 0)`
/// is the top-left of the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

impl TileRect {
    /// Half-open `(row_end, col_end)` for overlap math.
    fn end(&self) -> (u16, u16) {
        (self.row + self.rows, self.col + self.cols)
    }

    /// Returns true when this tile shares any cell with `other`.
    pub fn overlaps(&self, other: &TileRect) -> bool {
        let (r1, c1) = self.end();
        let (r2, c2) = other.end();
        !(self.row >= r2 || other.row >= r1 || self.col >= c2 || other.col >= c1)
    }
}

/// Computed layout for a single SIGWINCH frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// One tile per visible pane, in operator order (`tiles[i]` is pane `i`).
    pub tiles: Vec<TileRect>,
    /// Pane count that did not fit at minimum size; the footer renders
    /// `+overflow more`.
    pub overflow: usize,
    /// Footer rect, always anchored at the bottom.
    pub footer: TileRect,
    /// Grid shape the tiles laid out into (`rows` rows × `cols` cols of
    /// tiles). Exposed for diagnostics / tests; the renderer does not need
    /// it because tiles carry their own absolute rects.
    pub grid_rows: u16,
    pub grid_cols: u16,
}

/// Reasons [`compute`] can fail before producing any layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// Terminal is below `(MIN_PANE_ROWS + FOOTER_ROWS, MIN_PANE_COLS)` - a
    /// single minimum-size pane plus the footer cannot fit.
    TerminalTooSmall { rows: u16, cols: u16 },
    /// Zero panes is not a meaningful grid; the caller should have already
    /// exited with an "no agents" usage error.
    ZeroPanes,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::TerminalTooSmall { rows, cols } => write!(
                f,
                "terminal too small ({rows}x{cols}); need at least {}x{} for a single pane",
                MIN_PANE_ROWS + FOOTER_ROWS,
                MIN_PANE_COLS
            ),
            LayoutError::ZeroPanes => f.write_str("no panes to lay out"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Integer ceiling division. Used for both the row-count and column-count
/// derivations in [`compute`].
///
/// Delegates to the stdlib `usize::div_ceil` (stable since Rust 1.73), which is
/// itself overflow-safe (it does NOT use the `(a + b - 1) / b` form that can
/// overflow near `usize::MAX`). The `b == 0` guard returns 0 rather than
/// panicking, which `usize::div_ceil` would do on a zero divisor (PR #370 /
/// #408 review, gemini-code-assist).
const fn div_ceil_usize(a: usize, b: usize) -> usize {
    if b == 0 {
        return 0;
    }
    a.div_ceil(b)
}

/// Smallest integer `n` such that `n * n >= x`. Used as the starting point
/// for picking a square-ish grid shape.
///
/// The comparison `mid * mid >= x` is rewritten as `mid >= x / mid` so the
/// product cannot overflow on a 32-bit `usize` when `x > sqrt(usize::MAX)`.
/// Equivalent semantically (both sides are non-negative integers, mid is
/// positive after the early-return guard). Flagged in PR #370 review
/// (gemini-code-assist).
fn isqrt_ceil(x: usize) -> usize {
    if x <= 1 {
        return x;
    }
    let mut lo = 1usize;
    let mut hi = x;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        // mid >= 1 (guaranteed by lo >= 1), so division is well-defined.
        if mid >= x.div_ceil(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// Compute the layout for `pane_count` panes inside `tty`. Returns
/// [`LayoutError`] if the terminal is too small or `pane_count == 0`.
///
/// The algorithm:
///
/// 1. Reject if `tty < (MIN_PANE_ROWS + FOOTER_ROWS) x MIN_PANE_COLS`.
/// 2. Compute the max panes that fit at minimum size:
///    `max_panes_at_min = max_rows_at_min * max_cols_at_min`.
/// 3. `visible = min(pane_count, max_panes_at_min)` and
///    `overflow = pane_count - visible`.
/// 4. Pick `grid_cols >= max(ceil(sqrt(visible)), ceil(visible / max_rows_at_min))`,
///    clamped to `max_cols_at_min`. This guarantees
///    `ceil(visible / grid_cols) <= max_rows_at_min`.
/// 5. `grid_rows = ceil(visible / grid_cols)`.
/// 6. Tile dimensions are even-grid: `tile_h = usable_rows / grid_rows`,
///    `tile_w = usable_cols / grid_cols`. Remainder rows/cols are left
///    unused at the bottom-right margin; aspect-aware distribution is
///    Discretion #2 deferred.
pub fn compute(tty: TtySize, pane_count: usize) -> Result<Layout, LayoutError> {
    if pane_count == 0 {
        return Err(LayoutError::ZeroPanes);
    }
    if tty.rows < MIN_PANE_ROWS + FOOTER_ROWS || tty.cols < MIN_PANE_COLS {
        return Err(LayoutError::TerminalTooSmall {
            rows: tty.rows,
            cols: tty.cols,
        });
    }
    let usable_rows = tty.rows - FOOTER_ROWS;
    let usable_cols = tty.cols;

    let max_rows_at_min = (usable_rows / MIN_PANE_ROWS) as usize;
    let max_cols_at_min = (usable_cols / MIN_PANE_COLS) as usize;
    // The size check above guarantees both are >= 1.
    debug_assert!(max_rows_at_min >= 1 && max_cols_at_min >= 1);
    let max_panes_at_min = max_rows_at_min * max_cols_at_min;

    let visible = pane_count.min(max_panes_at_min);
    let overflow = pane_count - visible;

    // Pick grid_cols. The lower bound is the larger of:
    //   - ceil(sqrt(visible))                - pushes toward "square"
    //   - ceil(visible / max_rows_at_min)     - guarantees grid_rows fits
    let lower_bound = isqrt_ceil(visible).max(div_ceil_usize(visible, max_rows_at_min));
    let grid_cols = lower_bound.min(max_cols_at_min).max(1);
    let grid_rows = div_ceil_usize(visible, grid_cols);
    debug_assert!(grid_rows <= max_rows_at_min);

    let tile_h = usable_rows / (grid_rows as u16);
    let tile_w = usable_cols / (grid_cols as u16);
    debug_assert!(tile_h >= MIN_PANE_ROWS);
    debug_assert!(tile_w >= MIN_PANE_COLS);

    let mut tiles = Vec::with_capacity(visible);
    for i in 0..visible {
        let gr = (i / grid_cols) as u16;
        let gc = (i % grid_cols) as u16;
        tiles.push(TileRect {
            row: gr * tile_h,
            col: gc * tile_w,
            rows: tile_h,
            cols: tile_w,
        });
    }

    let footer = TileRect {
        row: usable_rows,
        col: 0,
        rows: FOOTER_ROWS,
        cols: tty.cols,
    };

    Ok(Layout {
        tiles,
        overflow,
        footer,
        grid_rows: grid_rows as u16,
        grid_cols: grid_cols as u16,
    })
}

// ── Pagination (fu-grid-pagination / ab-82dddd5f) ─────────────────────────
//
// v1 dropped surplus panes into `Layout::overflow` and rendered a `+k more`
// footer label. Pagination replaces that: the fleet is sliced into discrete
// **pages** of `capacity = C` panes each, and the operator flips between them.
// All page math derives from the same fit computation `compute` already does;
// these helpers expose it without changing `compute`'s v1 contract (the
// existing `Layout`/`overflow` path stays for back-compat and as the geometry
// engine here).

/// Page capacity `C`: the maximum panes that fit at minimum size for this
/// terminal. This is the per-page tile count pagination slices the fleet
/// into (`page_count = ceil(pane_count / C)`). Returns the same
/// [`LayoutError`] as [`compute`] when the terminal cannot fit even one
/// minimum pane plus the footer.
pub fn capacity(tty: TtySize) -> Result<usize, LayoutError> {
    if tty.rows < MIN_PANE_ROWS + FOOTER_ROWS || tty.cols < MIN_PANE_COLS {
        return Err(LayoutError::TerminalTooSmall {
            rows: tty.rows,
            cols: tty.cols,
        });
    }
    let usable_rows = tty.rows - FOOTER_ROWS;
    let max_rows_at_min = (usable_rows / MIN_PANE_ROWS) as usize;
    let max_cols_at_min = (tty.cols / MIN_PANE_COLS) as usize;
    Ok(max_rows_at_min * max_cols_at_min)
}

/// Number of pages needed to show `pane_count` panes at capacity `cap`.
/// Always `>= 1` (a single full page is the degenerate `pane_count <= cap`
/// case == v1). `cap == 0` is treated as 1 defensively; [`capacity`] never
/// returns 0 for a terminal that passed its size check.
pub fn page_count_for(pane_count: usize, cap: usize) -> usize {
    div_ceil_usize(pane_count, cap.max(1)).max(1)
}

/// A pagination-aware layout for one page of the fleet.
///
/// Unlike [`Layout`] (whose `tiles` cover the visible fit-subset of the WHOLE
/// fleet), [`PageLayout::tiles`] cover only the panes on [`current_page`]. The
/// global pane index of `tiles[slot]` is `page_start + slot`. The renderer
/// maps slot → global index to pull the right pane snapshot / name / state.
///
/// Geometry rule: in the multi-page case every page reuses one uniform `C`
/// tile geometry (`compute(tty, C)`), so a partial last page renders its
/// panes at the SAME "normal tile size" as a full page (AC1-EDGE) and flips
/// need no resize (warm). In the single-page degenerate case the geometry is
/// the v1 `compute(tty, pane_count)` layout and no pagination chrome renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageLayout {
    /// Tiles for the panes visible on [`current_page`], in page-slot order.
    /// Global index of `tiles[slot]` == `page_start + slot`.
    pub tiles: Vec<TileRect>,
    /// Footer rect, always anchored at the bottom.
    pub footer: TileRect,
    pub grid_rows: u16,
    pub grid_cols: u16,
    /// Total number of pages (`>= 1`).
    pub page_count: usize,
    /// The page this layout renders, clamped into `[0, page_count - 1]`.
    pub current_page: usize,
    /// Page capacity `C`.
    pub capacity: usize,
    /// Global index of the first pane on this page (`current_page * C`).
    pub page_start: usize,
    /// Total pane count this layout was computed for.
    pub pane_count: usize,
}

impl PageLayout {
    /// True when no pagination chrome should render (degenerate v1 case).
    pub fn is_single_page(&self) -> bool {
        self.page_count <= 1
    }

    /// Map a 0-indexed screen coordinate to the global pane index of the tile
    /// containing it, or `None` if `(col, row)` falls outside every visible
    /// tile (the footer row, or an inter-tile remainder gutter). The result is
    /// global — `page_start + slot`, matching `tiles[slot]`'s pane — so a click
    /// on a later page resolves to the right absolute pane. (E5a mouse-native.)
    pub fn pane_at(&self, col: u16, row: u16) -> Option<usize> {
        self.tiles
            .iter()
            .position(|t| {
                let (row_end, col_end) = t.end();
                row >= t.row && row < row_end && col >= t.col && col < col_end
            })
            .map(|slot| self.page_start + slot)
    }

    /// The uniform inner `(rows, cols)` every pane (including off-screen ones)
    /// should be sized to in the multi-page case, so a page flip is warm. The
    /// inner size strips the 1-cell border on each edge. `None` in the
    /// single-page case, where each visible pane carries its own tile size.
    pub fn uniform_pane_inner(&self) -> Option<(u16, u16)> {
        if self.is_single_page() {
            return None;
        }
        self.tiles
            .first()
            .map(|t| (t.rows.saturating_sub(2), t.cols.saturating_sub(2)))
    }
}

/// Compute the layout for one page of `pane_count` panes inside `tty`.
///
/// `current_page` is clamped into `[0, page_count - 1]` (a resize that shrank
/// the page count never paints an out-of-range slice - Domain Pitfall). The
/// returned [`PageLayout::tiles`] hold only the panes visible on the clamped
/// page; `page_start` gives their global offset.
pub fn compute_page(
    tty: TtySize,
    pane_count: usize,
    current_page: usize,
) -> Result<PageLayout, LayoutError> {
    if pane_count == 0 {
        return Err(LayoutError::ZeroPanes);
    }
    let cap = capacity(tty)?;
    let page_count = page_count_for(pane_count, cap);
    // saturating_sub: page_count_for guarantees >= 1, but stay underflow-safe.
    let current_page = current_page.min(page_count.saturating_sub(1));
    let page_start = current_page * cap;
    let page_pane_count = (pane_count - page_start).min(cap);

    // Geometry: single page tiles to the exact pane_count (v1); multi-page
    // tiles to the uniform C grid and slices the first page_pane_count of it.
    let geometry_panes = if page_count == 1 { pane_count } else { cap };
    let base = compute(tty, geometry_panes)?;
    let tiles: Vec<TileRect> = base.tiles.into_iter().take(page_pane_count).collect();

    Ok(PageLayout {
        tiles,
        footer: base.footer,
        grid_rows: base.grid_rows,
        grid_cols: base.grid_cols,
        page_count,
        current_page,
        capacity: cap,
        page_start,
        pane_count,
    })
}

// ── Rail layout variant (ab-1fab1fdf, Phase 1) ───────────────────────────────
//
// `compute_with_rail` reserves `rail_cols` on the left of the terminal and
// runs the existing `compute` on the narrowed main area. The rail is a single
// `TileRect` column spanning the usable height (all rows minus the footer).
//
// Width gate: when `tty.cols < rail_cols + MIN_PANE_COLS`, the rail cannot
// coexist with even a minimum-width main pane. In that case the caller should
// degrade to the railless grid (`compute_page`). `compute_with_rail` returns
// `LayoutError::TerminalTooSmall` to signal this, reusing the same error type.
//
// Invariant: `rail.cols + main_cols == tty.cols` (no gap, no overlap).
// The `TtySize` passed to the inner `compute` has `cols = tty.cols - rail_cols`,
// so the inner layout occupies exactly the right portion.

/// Default width of the navigation rail in columns.
pub const RAIL_COLS: u16 = 18;

/// Layout produced by `compute_with_rail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailLayout {
    /// The left-side rail (full usable height, `rail_cols` wide).
    pub rail: TileRect,
    /// The main area layout (right of the rail, same height, narrowed width).
    pub main: Layout,
    /// Footer rect spanning the full terminal width.
    pub footer: TileRect,
}

/// Compute a rail layout for `main_pane_count` panes. Reserves `rail_cols`
/// columns on the left; the remaining width is handed to the existing
/// `compute(...)` engine. Returns `LayoutError::TerminalTooSmall` if the
/// terminal is too narrow to fit both the rail and one minimum-width pane.
///
/// Invariant: `rail.cols + main.tiles[i].col_end <= tty.cols` for all tiles.
pub fn compute_with_rail(
    tty: TtySize,
    rail_cols: u16,
    main_pane_count: usize,
) -> Result<RailLayout, LayoutError> {
    // Width gate: rail + at least MIN_PANE_COLS must fit.
    let rail_cols = rail_cols.max(1);
    if tty.cols < rail_cols + MIN_PANE_COLS {
        return Err(LayoutError::TerminalTooSmall {
            rows: tty.rows,
            cols: tty.cols,
        });
    }
    // Height gate: same as compute.
    if tty.rows < MIN_PANE_ROWS + FOOTER_ROWS {
        return Err(LayoutError::TerminalTooSmall {
            rows: tty.rows,
            cols: tty.cols,
        });
    }

    let main_cols = tty.cols - rail_cols;
    let main_tty = TtySize::new(tty.rows, main_cols);

    // The inner layout computes tile coords in main_tty space (col 0 = start of
    // main area). We need to offset every tile's `col` by `rail_cols` to place
    // it in absolute terminal coordinates.
    let mut main = compute(main_tty, main_pane_count)?;
    for tile in &mut main.tiles {
        tile.col += rail_cols;
    }
    // The footer in the inner layout spans `main_cols`; replace it with a
    // full-width footer that covers both rail and main.
    let footer = TileRect {
        row: tty.rows - FOOTER_ROWS,
        col: 0,
        rows: FOOTER_ROWS,
        cols: tty.cols,
    };
    main.footer = footer;

    let usable_rows = tty.rows - FOOTER_ROWS;
    let rail = TileRect {
        row: 0,
        col: 0,
        rows: usable_rows,
        cols: rail_cols,
    };

    Ok(RailLayout { rail, main, footer })
}

// ── Rail + GroupTile pagination (ab-6aed6905, Phase 2 / US3) ──────────────────
//
// GroupTile tiles the selected group's members in the main area. A group too
// large to fit at minimum pane size must paginate *inside* its tile (AC3-ERR),
// independent of the railless grid's top-level `PageLayout` (which is inactive
// in rail mode, Locked Decision 4). `compute_with_rail_page` is to `compute_page`
// what `compute_with_rail` is to `compute`: it reserves `rail_cols` on the left
// and runs the page-aware engine on the narrowed main area, offsetting every
// tile by `rail_cols` into absolute terminal coordinates.
//
// Reusing `compute_page` means the rendered page is always clamped into range:
// `current_page` is pinned to `[0, page_count - 1]` so an out-of-range request
// (e.g. a derived page from a stale selection) paints the last page rather than
// an empty slice. NB: today the rail's group membership is frozen per session
// (Phase 1's `rail_rows`), so a group does not actually shrink mid-session; this
// clamp is the render-time safety that also covers a future live-refresh path.

/// Layout produced by [`compute_with_rail_page`]: a left rail plus a *paginated*
/// main area showing one page of a (possibly oversized) group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailPageLayout {
    /// The left-side rail (full usable height, `rail_cols` wide).
    pub rail: TileRect,
    /// The page-aware main-area layout (right of the rail). `main.tiles` cover
    /// only the members on `main.current_page`; `main.page_start` gives their
    /// offset within the group's member list.
    pub main: PageLayout,
    /// Footer rect spanning the full terminal width.
    pub footer: TileRect,
}

/// Compute a rail layout whose main area tiles one page of `pane_count` panes.
/// Reserves `rail_cols` on the left and runs [`compute_page`] on the narrowed
/// main area. Returns [`LayoutError::TerminalTooSmall`] if the rail plus one
/// minimum-width pane does not fit (the caller degrades to the railless grid),
/// and [`LayoutError::ZeroPanes`] if `pane_count == 0` (the caller must avoid
/// this by short-circuiting an empty/absent group - AC1-EDGE).
///
/// Invariant: `rail.cols + main.tiles[i].col_end <= tty.cols` for all tiles.
pub fn compute_with_rail_page(
    tty: TtySize,
    rail_cols: u16,
    pane_count: usize,
    current_page: usize,
) -> Result<RailPageLayout, LayoutError> {
    // Width gate: rail + at least MIN_PANE_COLS must fit (same as compute_with_rail).
    let rail_cols = rail_cols.max(1);
    if tty.cols < rail_cols + MIN_PANE_COLS {
        return Err(LayoutError::TerminalTooSmall {
            rows: tty.rows,
            cols: tty.cols,
        });
    }
    // Height gate: compute_page's capacity() already rejects a too-short
    // terminal, but mirror compute_with_rail's explicit check here so the two
    // rail variants share identical width+height gates (a GroupTile fall-through
    // to the Single render never disagrees on whether the rail fits).
    if tty.rows < MIN_PANE_ROWS + FOOTER_ROWS {
        return Err(LayoutError::TerminalTooSmall {
            rows: tty.rows,
            cols: tty.cols,
        });
    }

    let main_cols = tty.cols - rail_cols;
    let main_tty = TtySize::new(tty.rows, main_cols);

    // Page-aware main layout in main_tty space; offset tiles into absolute coords.
    let mut main = compute_page(main_tty, pane_count, current_page)?;
    for tile in &mut main.tiles {
        tile.col += rail_cols;
    }
    // Full-width footer spanning both rail and main (replaces the main-only one).
    let footer = TileRect {
        row: tty.rows - FOOTER_ROWS,
        col: 0,
        rows: FOOTER_ROWS,
        cols: tty.cols,
    };
    main.footer = footer;

    let usable_rows = tty.rows - FOOTER_ROWS;
    let rail = TileRect {
        row: 0,
        col: 0,
        rows: usable_rows,
        cols: rail_cols,
    };

    Ok(RailPageLayout { rail, main, footer })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tty(rows: u16, cols: u16) -> TtySize {
        TtySize::new(rows, cols)
    }

    /// AC1-HP / single pane: a one-pane grid fills the whole usable area.
    #[test]
    fn single_pane_fills_usable_area() {
        let layout = compute(tty(24, 80), 1).unwrap();
        assert_eq!(layout.tiles.len(), 1);
        assert_eq!(
            layout.tiles[0],
            TileRect {
                row: 0,
                col: 0,
                rows: 23,
                cols: 80
            }
        );
        assert_eq!(layout.overflow, 0);
        assert_eq!(
            layout.footer,
            TileRect {
                row: 23,
                col: 0,
                rows: 1,
                cols: 80
            }
        );
        assert_eq!((layout.grid_rows, layout.grid_cols), (1, 1));
    }

    /// Two panes on a wide terminal lay side by side.
    #[test]
    fn two_panes_lay_side_by_side() {
        let layout = compute(tty(24, 80), 2).unwrap();
        assert_eq!(layout.tiles.len(), 2);
        assert_eq!((layout.grid_rows, layout.grid_cols), (1, 2));
        // Each tile = 40 wide x 23 tall.
        assert_eq!(
            layout.tiles[0],
            TileRect {
                row: 0,
                col: 0,
                rows: 23,
                cols: 40
            }
        );
        assert_eq!(
            layout.tiles[1],
            TileRect {
                row: 0,
                col: 40,
                rows: 23,
                cols: 40
            }
        );
    }

    /// Four panes form a 2x2 grid (sqrt(4)=2).
    #[test]
    fn four_panes_form_two_by_two() {
        let layout = compute(tty(40, 100), 4).unwrap();
        assert_eq!((layout.grid_rows, layout.grid_cols), (2, 2));
        // usable rows = 39; tile_h = 19, tile_w = 50.
        let expected = [
            TileRect {
                row: 0,
                col: 0,
                rows: 19,
                cols: 50,
            },
            TileRect {
                row: 0,
                col: 50,
                rows: 19,
                cols: 50,
            },
            TileRect {
                row: 19,
                col: 0,
                rows: 19,
                cols: 50,
            },
            TileRect {
                row: 19,
                col: 50,
                rows: 19,
                cols: 50,
            },
        ];
        for (got, want) in layout.tiles.iter().zip(expected.iter()) {
            assert_eq!(got, want);
        }
    }

    /// AC5-UI: the footer is always anchored at the bottom, regardless of
    /// grid shape.
    #[test]
    fn footer_anchored_at_bottom() {
        for (r, c, n) in [(24, 80, 1), (24, 80, 3), (60, 200, 8)] {
            let layout = compute(tty(r, c), n).unwrap();
            assert_eq!(layout.footer.row, r - FOOTER_ROWS);
            assert_eq!(layout.footer.rows, FOOTER_ROWS);
            assert_eq!(layout.footer.col, 0);
            assert_eq!(layout.footer.cols, c);
        }
    }

    /// Failure Modes / Boundaries: a terminal smaller than one min pane +
    /// footer is rejected with TerminalTooSmall.
    #[test]
    fn terminal_too_small_rejected() {
        // rows below MIN_PANE_ROWS + FOOTER_ROWS = 7
        let err = compute(tty(5, 80), 1).unwrap_err();
        assert!(matches!(
            err,
            LayoutError::TerminalTooSmall { rows: 5, cols: 80 }
        ));

        // cols below MIN_PANE_COLS = 20
        let err = compute(tty(40, 10), 1).unwrap_err();
        assert!(matches!(
            err,
            LayoutError::TerminalTooSmall { rows: 40, cols: 10 }
        ));
    }

    /// Zero panes is a usage error.
    #[test]
    fn zero_panes_rejected() {
        let err = compute(tty(24, 80), 0).unwrap_err();
        assert_eq!(err, LayoutError::ZeroPanes);
    }

    /// Exactly minimum terminal size: a single pane just fits.
    #[test]
    fn exactly_minimum_terminal_size_fits_one_pane() {
        let layout = compute(tty(MIN_PANE_ROWS + FOOTER_ROWS, MIN_PANE_COLS), 1).unwrap();
        assert_eq!(layout.tiles.len(), 1);
        assert_eq!(layout.tiles[0].rows, MIN_PANE_ROWS);
        assert_eq!(layout.tiles[0].cols, MIN_PANE_COLS);
    }

    /// AC1-EDGE: more panes than fit at min size → visible = max-that-fit,
    /// overflow = rest.
    #[test]
    fn overflow_when_more_panes_than_fit() {
        // 80x24 → usable_rows=23 → max_rows_at_min=3 (23/6 floored).
        //         cols=80          → max_cols_at_min=4 (80/20).
        // max_panes_at_min = 3*4 = 12. Ask for 20.
        let layout = compute(tty(24, 80), 20).unwrap();
        assert_eq!(layout.tiles.len(), 12);
        assert_eq!(layout.overflow, 8);
        assert_eq!((layout.grid_rows, layout.grid_cols), (3, 4));
    }

    /// AC1-EDGE corollary: when exactly at the max-fit threshold, overflow
    /// is zero.
    #[test]
    fn exactly_max_at_min_size_no_overflow() {
        let layout = compute(tty(24, 80), 12).unwrap();
        assert_eq!(layout.tiles.len(), 12);
        assert_eq!(layout.overflow, 0);
        assert_eq!((layout.grid_rows, layout.grid_cols), (3, 4));
    }

    /// All tile rects must be pairwise non-overlapping. The renderer
    /// depends on this - overlapping tiles would corrupt the paint.
    #[test]
    fn tiles_are_pairwise_non_overlapping() {
        for n in 1..=12 {
            let layout = compute(tty(40, 120), n).unwrap();
            for i in 0..layout.tiles.len() {
                for j in (i + 1)..layout.tiles.len() {
                    assert!(
                        !layout.tiles[i].overlaps(&layout.tiles[j]),
                        "tiles {i} and {j} overlap at n={n}: {:?} vs {:?}",
                        layout.tiles[i],
                        layout.tiles[j]
                    );
                }
            }
        }
    }

    /// Tiles never extend past the terminal bounds or into the footer row.
    #[test]
    fn tiles_stay_within_usable_area() {
        for (r, c, n) in [(24, 80, 1), (24, 80, 4), (24, 80, 12), (60, 200, 8)] {
            let layout = compute(tty(r, c), n).unwrap();
            for (i, tile) in layout.tiles.iter().enumerate() {
                let (er, ec) = tile.end();
                assert!(
                    er <= r - FOOTER_ROWS,
                    "tile {i} bleeds past footer at n={n}"
                );
                assert!(ec <= c, "tile {i} bleeds past right edge at n={n}");
            }
        }
    }

    /// AC5-HP: a resize re-tiles to the new TTY size. The layout function is
    /// pure, so this is just calling compute() again with the new size.
    #[test]
    fn resize_retiles_to_new_size() {
        let before = compute(tty(24, 80), 4).unwrap();
        let after = compute(tty(40, 120), 4).unwrap();
        assert_ne!(before.tiles, after.tiles, "resize should change tile sizes");
        // grid shape stays 2x2 for 4 panes regardless of terminal aspect at v1.
        assert_eq!((after.grid_rows, after.grid_cols), (2, 2));
        // Each new tile is larger.
        for (b, a) in before.tiles.iter().zip(after.tiles.iter()) {
            assert!(a.rows >= b.rows);
            assert!(a.cols >= b.cols);
        }
    }

    /// Three panes on a square-ish terminal pick the closest-to-square grid
    /// shape (2x2 with one slot empty), since `ceil(sqrt(3)) = 2`.
    #[test]
    fn three_panes_use_two_by_two_grid_with_one_empty_slot() {
        let layout = compute(tty(40, 120), 3).unwrap();
        assert_eq!((layout.grid_rows, layout.grid_cols), (2, 2));
        assert_eq!(layout.tiles.len(), 3);
    }

    // ── Pagination tests (fu-grid-pagination) ────────────────────────────

    /// `capacity` matches the `max_panes_at_min` `compute` derives internally.
    /// 24x80 → usable_rows=23 → 3 rows · 4 cols = 12.
    #[test]
    fn capacity_matches_fit_subset() {
        assert_eq!(capacity(tty(24, 80)).unwrap(), 12);
        // 40x100 → usable 39 → 6 rows · 5 cols = 30.
        assert_eq!(capacity(tty(40, 100)).unwrap(), 30);
    }

    /// `capacity` rejects a too-small terminal with the same error as compute.
    #[test]
    fn capacity_rejects_too_small() {
        assert!(matches!(
            capacity(tty(5, 80)),
            Err(LayoutError::TerminalTooSmall { .. })
        ));
    }

    /// `page_count_for`: ceil division, always at least 1.
    #[test]
    fn page_count_for_boundaries() {
        assert_eq!(page_count_for(0, 4), 1); // degenerate: still one page
        assert_eq!(page_count_for(3, 4), 1); // under capacity → single page
        assert_eq!(page_count_for(4, 4), 1); // exactly capacity → single page
        assert_eq!(page_count_for(5, 4), 2); // C+1 → two pages
        assert_eq!(page_count_for(8, 4), 2); // exactly 2 full pages
        assert_eq!(page_count_for(9, 4), 3); // spills to a third page
        assert_eq!(page_count_for(10, 1), 10); // one-pane-per-page
    }

    /// AC1-UI: pane_count <= C is the single-page degenerate case (no chrome).
    /// 3 agents in a terminal that fits 12 → one page, geometry == v1 compute.
    #[test]
    fn single_page_when_under_capacity() {
        let paged = compute_page(tty(24, 80), 3, 0).unwrap();
        assert_eq!(paged.page_count, 1);
        assert!(paged.is_single_page());
        assert_eq!(paged.tiles.len(), 3);
        assert_eq!(paged.page_start, 0);
        // Geometry must equal the v1 single-page layout for 3 panes.
        let v1 = compute(tty(24, 80), 3).unwrap();
        assert_eq!(paged.tiles, v1.tiles);
        // No uniform off-screen size in the single-page case.
        assert_eq!(paged.uniform_pane_inner(), None);
    }

    /// AC1-EDGE: 5 agents with C=4 → 2 pages; page 2 holds exactly 1 pane at
    /// the SAME (uniform) tile size as a page-1 pane ("normal tile size").
    #[test]
    fn capacity_plus_one_makes_two_pages_last_page_one_pane() {
        // Find a tty whose capacity is exactly 4: 24x40 → usable 23 → 3 rows,
        // 40/20 = 2 cols → C = 6. Need C=4. 13x40 → usable 12 → 2 rows · 2 cols
        // = 4.
        let t = tty(13, 40);
        assert_eq!(capacity(t).unwrap(), 4);
        let p0 = compute_page(t, 5, 0).unwrap();
        assert_eq!(p0.page_count, 2);
        assert_eq!(p0.capacity, 4);
        assert_eq!(p0.tiles.len(), 4); // full first page
        let p1 = compute_page(t, 5, 1).unwrap();
        assert_eq!(p1.current_page, 1);
        assert_eq!(p1.page_start, 4);
        assert_eq!(p1.tiles.len(), 1); // last page: exactly one pane
                                       // Uniform tile size: the single pane on page 2 has the same dims as a
                                       // page-1 pane (NOT a full-screen pane).
        assert_eq!(p1.tiles[0].rows, p0.tiles[0].rows);
        assert_eq!(p1.tiles[0].cols, p0.tiles[0].cols);
    }

    /// `compute_page` clamps an out-of-range `current_page` (resize shrank the
    /// page count) to the last valid page rather than painting a blank slice.
    #[test]
    fn compute_page_clamps_out_of_range_page() {
        let t = tty(13, 40); // C = 4
                             // 5 panes → 2 pages (0,1). Ask for page 7.
        let p = compute_page(t, 5, 7).unwrap();
        assert_eq!(p.current_page, 1, "out-of-range page clamps to last valid");
        assert_eq!(p.tiles.len(), 1);
    }

    /// AC4-EDGE: resize so only one pane fits per page (C=1) → each agent its
    /// own page, full-screen tile.
    #[test]
    fn one_pane_per_page_when_capacity_is_one() {
        // 7x20 → usable 6 → 1 row, 20/20 = 1 col → C = 1.
        let t = tty(7, 20);
        assert_eq!(capacity(t).unwrap(), 1);
        let paged = compute_page(t, 4, 2).unwrap();
        assert_eq!(paged.page_count, 4);
        assert_eq!(paged.current_page, 2);
        assert_eq!(paged.tiles.len(), 1);
        assert_eq!(paged.page_start, 2);
    }

    /// Multi-page exposes a uniform off-screen pane size; every page's slot-0
    /// tile is identical, so flips need no resize.
    #[test]
    fn multi_page_uniform_pane_size() {
        let t = tty(13, 40); // C = 4
        let p0 = compute_page(t, 9, 0).unwrap();
        assert!(!p0.is_single_page());
        let inner = p0
            .uniform_pane_inner()
            .expect("multi-page has uniform size");
        // Inner = tile minus 1-cell border on each edge.
        assert_eq!(inner.0, p0.tiles[0].rows - 2);
        assert_eq!(inner.1, p0.tiles[0].cols - 2);
        // The middle page uses the same geometry.
        let p1 = compute_page(t, 9, 1).unwrap();
        assert_eq!(p1.tiles[0], p0.tiles[0]);
    }

    /// Zero panes is a usage error in the paginated path too.
    #[test]
    fn compute_page_zero_panes_rejected() {
        assert_eq!(
            compute_page(tty(24, 80), 0, 0).unwrap_err(),
            LayoutError::ZeroPanes
        );
    }

    /// E5a hit-test: a screen coord inside tile slot `i` maps to the global
    /// pane index `page_start + i`; the footer row and coords outside every
    /// tile map to `None`.
    #[test]
    fn pane_at_maps_coord_to_global_pane_index() {
        let paged = compute_page(tty(24, 80), 4, 0).unwrap();
        assert_eq!(paged.page_count, 1, "4 panes fit a single page at 24x80");
        for (slot, t) in paged.tiles.iter().enumerate() {
            // The tile's own origin hits its own slot.
            assert_eq!(paged.pane_at(t.col, t.row), Some(paged.page_start + slot));
            // A cell strictly inside the tile also hits it.
            let mid_col = t.col + t.cols / 2;
            let mid_row = t.row + t.rows / 2;
            assert_eq!(
                paged.pane_at(mid_col, mid_row),
                Some(paged.page_start + slot)
            );
        }
        // The footer row belongs to no pane.
        assert_eq!(paged.pane_at(paged.footer.col, paged.footer.row), None);
        // Far outside the grid → None.
        assert_eq!(paged.pane_at(9999, 9999), None);
    }

    /// On a later page, slot 0 maps to the global index `page_start`, not 0.
    #[test]
    fn pane_at_is_page_relative() {
        // 7x20 → capacity 1 → 4 panes across 4 pages; render page index 2.
        let paged = compute_page(tty(7, 20), 4, 2).unwrap();
        assert_eq!(paged.capacity, 1);
        assert_eq!(paged.page_start, 2);
        let t = &paged.tiles[0];
        assert_eq!(paged.pane_at(t.col, t.row), Some(2));
    }

    /// `isqrt_ceil` is well-defined for the edge cases we hit.
    #[test]
    fn isqrt_ceil_boundary_values() {
        assert_eq!(isqrt_ceil(0), 0);
        assert_eq!(isqrt_ceil(1), 1);
        assert_eq!(isqrt_ceil(2), 2);
        assert_eq!(isqrt_ceil(3), 2);
        assert_eq!(isqrt_ceil(4), 2);
        assert_eq!(isqrt_ceil(5), 3);
        assert_eq!(isqrt_ceil(9), 3);
        assert_eq!(isqrt_ceil(10), 4);
    }

    // ── compute_with_rail tests (ab-1fab1fdf, Phase 1) ───────────────────

    /// AC2-EDGE: rail_cols + main_cols == tty.cols (no gap, no overlap).
    #[test]
    fn rail_layout_width_invariant() {
        let t = tty(24, 80);
        let rl = compute_with_rail(t, RAIL_COLS, 1).unwrap();
        assert_eq!(
            rl.rail.cols + (rl.main.tiles[0].cols),
            t.cols - RAIL_COLS + rl.rail.cols,
            "rail + main tile cols together should equal total"
        );
        // More precisely: rail occupies RAIL_COLS, main tile starts at RAIL_COLS,
        // and its right edge is at RAIL_COLS + main_cols = tty.cols.
        assert_eq!(rl.rail.cols, RAIL_COLS);
        assert_eq!(rl.rail.col, 0);
        // main tile col starts right after the rail.
        assert_eq!(rl.main.tiles[0].col, RAIL_COLS);
        // main tile right edge does not exceed tty.cols.
        let main_tile = &rl.main.tiles[0];
        assert!(
            main_tile.col + main_tile.cols <= t.cols,
            "main tile stays within tty.cols"
        );
    }

    /// AC2-EDGE: the usable column budget splits exactly: no gap, no overlap.
    #[test]
    fn rail_cols_plus_main_cols_equals_tty_cols() {
        for &total_cols in &[40u16, 80, 120, 200] {
            let t = tty(24, total_cols);
            let rl = compute_with_rail(t, RAIL_COLS, 2).unwrap();
            // Rail occupies [0, RAIL_COLS).
            assert_eq!(rl.rail.col, 0);
            assert_eq!(rl.rail.cols, RAIL_COLS);
            // Footer spans full width.
            assert_eq!(rl.footer.cols, total_cols);
            assert_eq!(rl.footer.col, 0);
            // Every main tile starts at >= RAIL_COLS.
            for tile in &rl.main.tiles {
                assert!(
                    tile.col >= RAIL_COLS,
                    "main tile col {} < RAIL_COLS {} at total_cols={}",
                    tile.col,
                    RAIL_COLS,
                    total_cols
                );
                // And does not exceed tty.cols.
                assert!(
                    tile.col + tile.cols <= total_cols,
                    "main tile bleeds past right edge at total_cols={}",
                    total_cols
                );
            }
        }
    }

    /// Width gate: terminal too narrow to hold rail + MIN_PANE_COLS.
    #[test]
    fn rail_layout_degrades_when_too_narrow() {
        // RAIL_COLS = 18; MIN_PANE_COLS = 20 -> need at least 38 cols.
        let too_narrow = tty(24, RAIL_COLS + MIN_PANE_COLS - 1);
        let err = compute_with_rail(too_narrow, RAIL_COLS, 1).unwrap_err();
        assert!(
            matches!(err, LayoutError::TerminalTooSmall { .. }),
            "too-narrow terminal must degrade"
        );
    }

    /// Exactly the minimum width: fits one minimum-width main pane.
    #[test]
    fn rail_layout_exactly_min_width_fits() {
        let exact = tty(24, RAIL_COLS + MIN_PANE_COLS);
        let rl = compute_with_rail(exact, RAIL_COLS, 1).unwrap();
        assert_eq!(rl.main.tiles.len(), 1);
        // Main tile has exactly MIN_PANE_COLS columns.
        assert_eq!(rl.main.tiles[0].cols, MIN_PANE_COLS);
    }

    /// Rail does not overlap with any main pane tile (overlap invariant).
    #[test]
    fn rail_does_not_overlap_main_tiles() {
        let t = tty(40, 120);
        let rl = compute_with_rail(t, RAIL_COLS, 4).unwrap();
        for (i, tile) in rl.main.tiles.iter().enumerate() {
            assert!(
                !rl.rail.overlaps(tile),
                "rail overlaps with main tile {i}: rail={:?} tile={:?}",
                rl.rail,
                tile
            );
        }
    }

    /// Footer spans full tty width and is anchored at the bottom row.
    #[test]
    fn rail_layout_footer_full_width_anchored() {
        let t = tty(24, 100);
        let rl = compute_with_rail(t, RAIL_COLS, 3).unwrap();
        assert_eq!(rl.footer.col, 0);
        assert_eq!(rl.footer.cols, t.cols);
        assert_eq!(rl.footer.row, t.rows - FOOTER_ROWS);
        assert_eq!(rl.footer.rows, FOOTER_ROWS);
    }

    /// ZeroPanes propagates from the inner compute.
    #[test]
    fn rail_layout_zero_panes_rejected() {
        let err = compute_with_rail(tty(24, 80), RAIL_COLS, 0).unwrap_err();
        assert_eq!(err, LayoutError::ZeroPanes);
    }

    /// Rail spans the full usable height (rows - FOOTER_ROWS).
    #[test]
    fn rail_rect_spans_usable_height() {
        let t = tty(30, 80);
        let rl = compute_with_rail(t, RAIL_COLS, 1).unwrap();
        assert_eq!(rl.rail.row, 0);
        assert_eq!(rl.rail.rows, t.rows - FOOTER_ROWS);
    }

    // ── compute_with_rail_page (ab-6aed6905, Phase 2 / US3) ───────────────

    /// AC3-HP: a small group tiles on one page; main tiles sit right of the rail
    /// and the layout reports a single page (no pagination chrome).
    #[test]
    fn rail_page_small_group_single_page() {
        let t = tty(40, 120);
        let rp = compute_with_rail_page(t, RAIL_COLS, 3, 0).unwrap();
        assert_eq!(rp.main.tiles.len(), 3, "all 3 members tile on one page");
        assert!(
            rp.main.is_single_page(),
            "no pagination chrome for a small group"
        );
        assert_eq!(rp.rail.cols, RAIL_COLS);
        for tile in &rp.main.tiles {
            assert!(tile.col >= RAIL_COLS, "main tile must clear the rail");
            assert!(
                tile.col + tile.cols <= t.cols,
                "main tile must not bleed past edge"
            );
        }
    }

    /// AC3-EDGE: a single-member group fills the main area as one tile.
    #[test]
    fn rail_page_single_member_fills_main() {
        let t = tty(30, 80);
        let rp = compute_with_rail_page(t, RAIL_COLS, 1, 0).unwrap();
        assert_eq!(rp.main.tiles.len(), 1);
        assert!(rp.main.is_single_page());
        // The one tile starts at the rail boundary and runs to the right edge.
        let tile = &rp.main.tiles[0];
        assert_eq!(tile.col, RAIL_COLS);
        assert_eq!(tile.col + tile.cols, t.cols);
    }

    /// AC3-ERR: a group too large for one screen paginates. Page count > 1 and
    /// each page shows at most `capacity` tiles; page 0 starts at member 0.
    #[test]
    fn rail_page_large_group_paginates() {
        let t = tty(24, 80); // narrow main area -> small per-page capacity
        let main_cols = t.cols - RAIL_COLS;
        let cap = capacity(tty(t.rows, main_cols)).unwrap();
        let group_size = cap * 2 + 1; // spills onto a third page

        let rp = compute_with_rail_page(t, RAIL_COLS, group_size, 0).unwrap();
        assert!(rp.main.page_count >= 3, "oversized group spans >= 3 pages");
        assert_eq!(rp.main.current_page, 0);
        assert_eq!(rp.main.page_start, 0);
        assert!(
            rp.main.tiles.len() <= cap,
            "a page shows at most capacity tiles"
        );

        // Last page is clamped and reachable; page_start lands on the right offset.
        let last = rp.main.page_count - 1;
        let rp_last = compute_with_rail_page(t, RAIL_COLS, group_size, last).unwrap();
        assert_eq!(rp_last.main.current_page, last);
        assert_eq!(rp_last.main.page_start, last * rp_last.main.capacity);
    }

    /// A stale / out-of-range page (e.g. derived from a selection that no longer
    /// fits) clamps into range rather than painting an empty slice or erroring.
    #[test]
    fn rail_page_out_of_range_page_clamps() {
        let t = tty(24, 80);
        // Ask for page 99 of a 2-member group (1 page): clamps to page 0.
        let rp = compute_with_rail_page(t, RAIL_COLS, 2, 99).unwrap();
        assert_eq!(rp.main.current_page, 0);
        assert_eq!(rp.main.page_start, 0);
        assert_eq!(rp.main.tiles.len(), 2);
    }

    /// Width gate + ZeroPanes propagate exactly as for compute_with_rail.
    #[test]
    fn rail_page_width_gate_and_zero_panes() {
        let too_narrow = tty(24, RAIL_COLS + MIN_PANE_COLS - 1);
        assert!(matches!(
            compute_with_rail_page(too_narrow, RAIL_COLS, 1, 0).unwrap_err(),
            LayoutError::TerminalTooSmall { .. }
        ));
        let zero = compute_with_rail_page(tty(24, 80), RAIL_COLS, 0, 0).unwrap_err();
        assert_eq!(
            zero,
            LayoutError::ZeroPanes,
            "0-member group must be rejected"
        );
    }

    /// rail.cols + main_cols == tty.cols invariant holds for the paged variant.
    #[test]
    fn rail_page_width_invariant() {
        for &total_cols in &[40u16, 80, 120, 200] {
            let t = tty(30, total_cols);
            let rp = compute_with_rail_page(t, RAIL_COLS, 5, 0).unwrap();
            assert_eq!(rp.rail.col, 0);
            assert_eq!(rp.rail.cols, RAIL_COLS);
            assert_eq!(rp.footer.cols, total_cols);
            for (i, tile) in rp.main.tiles.iter().enumerate() {
                assert!(!rp.rail.overlaps(tile), "rail overlaps main tile {i}");
                assert!(
                    tile.col + tile.cols <= total_cols,
                    "tile {i} bleeds past edge"
                );
            }
        }
    }
}
