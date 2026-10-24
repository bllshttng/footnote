//! Keyboard scroll mode (v118, prefix+PgUp): the client half, split out of
//! `client.rs` (file budget). The mode owns the keyboard like search: stdin
//! diverts to [`scroll_mode_keys`], each move rides `ClientMsg::PaneScroll` /
//! `PaneScrollEdge` onto the SHARED server-side offset the wheel drives (so
//! co-viewers track), and the bottom row shows the `scroll · line N of M`
//! readout.

use super::*;

/// One folded scroll-mode key. `Ctrl-u`/`Ctrl-d` ride as plain `Byte`s
/// (0x15 / 0x04); PageUp/PageDown are the folded `ESC [ 5 ~` / `ESC [ 6 ~`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScrollKey {
    Byte(u8),
    Esc,
    PageUp,
    PageDown,
}

/// Fold scroll-mode bytes into [`ScrollKey`]s, carrying escape state in `esc`
/// ACROSS reads (the `fold_search_input` contract: a CSI split at a read
/// boundary never leaks its tail into the pane and never half-exits the mode).
/// PageUp/PageDown surface as tokens; every other complete CSI is swallowed
/// whole; a bare Esc releases on an empty read (the quiet-window flush).
fn fold_scroll_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<ScrollKey> {
    if bytes.is_empty() {
        return if crate::keys::take_lone_esc(esc) {
            vec![ScrollKey::Esc]
        } else {
            Vec::new()
        };
    }
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(ScrollKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b);
                } else {
                    // The pending ESC was a bare Esc press: surface it, then
                    // reprocess this byte fresh.
                    esc.clear();
                    keys.push(ScrollKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b);
                    } else {
                        keys.push(ScrollKey::Byte(b));
                    }
                }
            }
            // Inside a CSI: PageUp/PageDown complete on the `~` final; every
            // other sequence is swallowed whole (whole-sequence leak safety).
            _ => {
                if b == 0x1b {
                    // ESC aborts an in-progress sequence and starts a fresh one.
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) {
                    if esc.as_slice() == b"\x1b[5" && b == b'~' {
                        keys.push(ScrollKey::PageUp);
                    } else if esc.as_slice() == b"\x1b[6" && b == b'~' {
                        keys.push(ScrollKey::PageDown);
                    }
                    esc.clear();
                } else if esc.len() >= input_folds::MAX_ESC_CARRY {
                    esc.clear();
                } else {
                    esc.push(b);
                }
            }
        }
    }
    keys
}

/// The focused pane's viewport height in rows, for the page step. A pane with
/// no frame yet (never painted) still gets a usable page of one row.
fn page_rows(view: &View, pane: u64) -> i32 {
    view.frames
        .get(&pane)
        .map(|f| f.rows as i32)
        .unwrap_or(1)
        .max(1)
}

/// Scroll-mode keys: `j`/`k` one line, PgUp/PgDn one page, Ctrl-u/Ctrl-d a
/// half page, `g`/`G` the top/bottom, `q`/Esc exits. Each move resolves the
/// CURRENTLY focused pane (like BlockJump) and sends one wire message; the
/// shared offset means every co-viewer tracks. Any other key is consumed -
/// the mode owns the keyboard (the answer-overlay invariant), so nothing
/// leaks into the pane mid-read.
pub(super) async fn scroll_mode_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.scroll_esc);
    let keys = fold_scroll_keys(&mut esc, bytes);
    view.scroll_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk exits, and the rest of
        // the chunk must be swallowed, never forwarded.
        if !view.scroll_mode {
            break;
        }
        let pane = view.layout.focus;
        let rows = page_rows(view, pane);
        let half = (rows / 2).max(1);
        match key {
            ScrollKey::Esc | ScrollKey::Byte(b'q') => {
                view.scroll_mode = false;
                view.scroll_esc.clear();
                break;
            }
            ScrollKey::Byte(b'k') => {
                write_msg(sock_w, &ClientMsg::PaneScroll { pane, delta: 1 })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::Byte(b'j') => {
                write_msg(sock_w, &ClientMsg::PaneScroll { pane, delta: -1 })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::PageUp => {
                write_msg(sock_w, &ClientMsg::PaneScroll { pane, delta: rows })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::PageDown => {
                write_msg(sock_w, &ClientMsg::PaneScroll { pane, delta: -rows })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::Byte(0x15) => {
                write_msg(sock_w, &ClientMsg::PaneScroll { pane, delta: half })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::Byte(0x04) => {
                write_msg(sock_w, &ClientMsg::PaneScroll { pane, delta: -half })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::Byte(b'g') => {
                write_msg(sock_w, &ClientMsg::PaneScrollEdge { pane, top: true })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::Byte(b'G') => {
                write_msg(sock_w, &ClientMsg::PaneScrollEdge { pane, top: false })
                    .await
                    .map_err(|e| format!("scroll send failed: {e}"))?;
            }
            ScrollKey::Byte(_) => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// The scroll-mode readout on the bottom row: `scroll · line N of M` plus the
/// key map, drawn over whatever the row held (the search-line precedent - no
/// reserved row, so entering the mode never triggers a Resize/reflow). N is
/// the viewport's TOP line, 1-based from the grid top: at the live bottom it
/// reads `M - rows + 1`, at the oldest history line `1`.
pub(super) fn draw_scroll_line(view: &View, cells: &mut [Cell], rows: usize, cols: usize) {
    let r = rows - 1;
    for c in 0..cols {
        cells[r * cols + c] = Cell::default();
    }
    let (n, total) = view
        .frames
        .get(&view.layout.focus)
        .map(|f| {
            let total = f.history_lines as i64;
            let n = total - f.rows as i64 - f.scroll_offset as i64 + 1;
            (n.max(1), total)
        })
        .unwrap_or((1, 1));
    let text = format!(
        " scroll · line {n} of {total} · j/k line · pgup/pgdn page · ctrl-u/d half · g/G ends · q/Esc leaves "
    );
    for (i, ch) in text.chars().take(cols).enumerate() {
        cells[r * cols + i] = Cell {
            c: ch,
            fg: Color::Default,
            bg: Color::Default,
            flags: cell_flags::INVERSE,
        };
    }
}

// Scroll-mode key folding: the split-sequence contract the other folds hold.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_surfaces_page_keys_and_bare_esc() {
        let mut esc = Vec::new();
        assert_eq!(
            fold_scroll_keys(&mut esc, b"\x1b[5~"),
            vec![ScrollKey::PageUp]
        );
        // A PageDown split across reads still folds; nothing leaks mid-way.
        assert_eq!(
            fold_scroll_keys(&mut esc, b"\x1b[6"),
            Vec::<ScrollKey>::new()
        );
        assert_eq!(fold_scroll_keys(&mut esc, b"~"), vec![ScrollKey::PageDown]);
        assert!(esc.is_empty(), "no carry after a complete sequence");
        // Plain bytes ride through; a lone ESC waits for the next byte.
        assert_eq!(
            fold_scroll_keys(&mut esc, b"kq"),
            vec![ScrollKey::Byte(b'k'), ScrollKey::Byte(b'q')]
        );
        assert_eq!(fold_scroll_keys(&mut esc, b"\x1b"), Vec::<ScrollKey>::new());
        // The held ESC plus a real key: the Esc surfaces first, then the key.
        assert_eq!(
            fold_scroll_keys(&mut esc, b"j"),
            vec![ScrollKey::Esc, ScrollKey::Byte(b'j')]
        );
        assert!(esc.is_empty());
    }
}
