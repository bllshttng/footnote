//! The file viewer: a clicked file shown beside the panes (`panel`) or over
//! them (`modal`), per `mux.viewer`. A client surface only. The server never
//! sees it, so two attached clients can show different files.
//!
//! It paints one framed block through the shared chrome frame, so the esc
//! chip, the footer's `esc close` words, the scroll bar and the close-chip
//! hit test all come from the same code every other overlay uses. The panel
//! takes its columns from the content area (the server resizes the panes), and
//! the feed panel yields the right edge while the viewer holds it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use super::backlog_style::{to_body_line, BLine};
use super::input_folds::{fold_modal_keys, ModalKey};
use super::node_detail::backlog_md::md_lines;
use super::overlay_paint::{draw_overlay_layout, family_b_origin, OverlayAnchor, OverlayLayout};
use super::*;
use crate::chrome;
use crate::open_router::ViewerShape;
use crate::popup::Anchor;

const DEFAULT_PCT: u16 = 40;
const STEP_PCT: i16 = 5;
const MIN_PCT: i16 = 20;
const MAX_PCT: i16 = 80;
/// Below this the panel is not worth drawing: the viewer shows as a modal.
const MIN_PANEL_W: u16 = 24;
const MIN_MODAL: (usize, usize) = (40, 8);
/// The most bytes read from a file. A file past it shows its first MiB.
const READ_CAP: usize = 1 << 20;
const SNIFF_BYTES: usize = 8192;
const WHEEL_ROWS: isize = 3;

/// What the viewer shows for one file.
pub(crate) enum Body {
    Text {
        lines: Vec<String>,
        truncated: bool,
    },
    /// A `.md` file: the cleaned source is kept whole and rendered through
    /// the backlog pane's markdown renderer at paint width, so a resize
    /// re-wraps the prose.
    Markdown {
        src: String,
        truncated: bool,
    },
    Binary,
    Failed(String),
}

/// A file read off the UI loop, ready to open.
pub(crate) struct Loaded {
    path: PathBuf,
    line: Option<u32>,
    shape: ViewerShape,
    body: Body,
}

/// Read `path` for the viewer. Blocking: call it off the UI loop.
pub(crate) fn load(path: PathBuf, line: Option<u32>, shape: ViewerShape) -> Loaded {
    let body = read_body(&path);
    Loaded {
        path,
        line,
        shape,
        body,
    }
}

fn read_body(path: &Path) -> Body {
    use std::io::Read;
    // A FIFO would park this blocking thread on open, so only a regular file reads.
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return Body::Failed("not a regular file".to_string());
    }
    let mut buf = Vec::new();
    let read =
        std::fs::File::open(path).and_then(|f| f.take(READ_CAP as u64 + 1).read_to_end(&mut buf));
    if let Err(e) = read {
        return Body::Failed(e.to_string());
    }
    let truncated = buf.len() > READ_CAP;
    buf.truncate(READ_CAP);
    if buf.iter().take(SNIFF_BYTES).any(|&b| b == 0) {
        return Body::Binary;
    }
    let text = String::from_utf8_lossy(&buf);
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
    {
        // The renderer wraps at paint width, so the cleaned source lines are
        // kept whole instead of pre-split.
        let src = text.lines().map(clean).collect::<Vec<_>>().join("\n");
        return Body::Markdown { src, truncated };
    }
    Body::Text {
        lines: text.lines().map(clean).collect(),
        truncated,
    }
}

/// One source line made safe to paint: a tab becomes four spaces and any
/// other control character a dot, so file bytes can never reach the terminal
/// as an escape sequence.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('\u{b7}'),
            c => out.push(c),
        }
    }
    out
}

/// The open viewer, or none.
#[derive(Default)]
pub(crate) struct ViewerState {
    open: Option<FileViewer>,
    /// Panel width as a percent of the terminal; 0 reads as the default. It
    /// outlives a close, so a resize sticks for the session.
    pct: u16,
}

impl ViewerState {
    fn pct(&self) -> u16 {
        if self.pct == 0 {
            DEFAULT_PCT
        } else {
            self.pct
        }
    }
}

struct FileViewer {
    path: PathBuf,
    shape: ViewerShape,
    body: Body,
    /// The first source line in the window.
    scroll: usize,
    /// The 0-based line a `path:line` click named.
    target: Option<usize>,
    /// Whether the viewer takes keys. A click on a pane takes them back.
    focused: bool,
    /// The markdown body rendered at the last paint width. Paint paths hold
    /// `&View`, so the cache sits in a `RefCell`; nothing re-enters it while
    /// a borrow is out, and a resize re-renders once.
    md: RefCell<Option<(usize, Vec<BLine>)>>,
    esc: Vec<u8>,
}

impl FileViewer {
    fn lines(&self) -> Option<&[String]> {
        match &self.body {
            Body::Text { lines, .. } if !lines.is_empty() => Some(lines),
            _ => None,
        }
    }

    fn count(&self) -> usize {
        self.lines().map_or(0, <[String]>::len)
    }

    /// The rows a body with no numbered lines prints.
    fn notes(&self) -> Vec<String> {
        match &self.body {
            Body::Text { .. } | Body::Markdown { .. } => vec!["(empty file)".into()],
            Body::Binary => vec![
                "no preview: this is a binary file".into(),
                String::new(),
                "press o to open it outside the mux".into(),
            ],
            Body::Failed(why) => vec!["cannot read this file".into(), String::new(), why.clone()],
        }
    }
}

impl View {
    /// Columns the panel takes from the right edge. 0 when no viewer is open,
    /// when it is a modal, or when the terminal cannot spare the columns (the
    /// viewer then shows as a modal).
    pub(super) fn viewer_panel_w(&self) -> u16 {
        let Some(v) = &self.viewer.open else {
            return 0;
        };
        if v.shape != ViewerShape::Panel {
            return 0;
        }
        let cols = self.term.1;
        let room = cols.saturating_sub(self.panel_w() + MIN_CONTENT_COLS);
        if room < MIN_PANEL_W {
            return 0;
        }
        let want = u32::from(cols) * u32::from(self.viewer.pct()) / 100;
        (want as u16).clamp(MIN_PANEL_W, room)
    }

    /// Whether the viewer is open and holds the keyboard.
    pub(super) fn viewer_focused(&self) -> bool {
        self.viewer.open.as_ref().is_some_and(|v| v.focused)
    }

    pub(super) fn viewer_open(&self) -> bool {
        self.viewer.open.is_some()
    }
}

/// Where the viewer paints and how much room its text has.
struct Plan {
    origin: (usize, usize),
    w: usize,
    h: usize,
    chrome: chrome::Chrome,
    budget: usize,
    text_w: usize,
}

impl Plan {
    /// Columns for a source line's text, after its number gutter.
    fn area(&self, v: &FileViewer) -> usize {
        self.text_w.saturating_sub(digits(v.count()) + 1).max(1)
    }
}

fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

fn plan(view: &View, v: &FileViewer) -> Plan {
    let panel = view.viewer_panel_w() as usize;
    let (origin, w, h) = if panel > 0 {
        let rows = view.content_dims().0 as usize;
        (
            (TAB_BAR_ROWS as usize, view.term.1 as usize - panel),
            panel,
            rows,
        )
    } else {
        let (base, (rows, cols)) = view.overlay_viewport();
        let w = (cols * 8 / 10).max(MIN_MODAL.0).min(cols);
        let h = (rows * 8 / 10).max(MIN_MODAL.1).min(rows);
        (
            family_b_origin(OverlayAnchor::Center, w, h, base, (rows, cols)),
            w,
            h,
        )
    };
    let chrome = chrome_for(v, w, panel > 0);
    Plan {
        origin,
        w,
        h,
        budget: h.saturating_sub(chrome.rows_overhead()),
        // Two border columns, one for the scroll bar, two of side pad.
        text_w: w.saturating_sub(chrome::Chrome::FRAME_COLS + 1 + 2).max(1),
        chrome,
    }
}

fn chrome_for(v: &FileViewer, w: usize, panel: bool) -> chrome::Chrome {
    // The widest the chrome may be: the frame's borders and the scroll column
    // come off the width, so the block never outgrows the width it was given.
    let room = w.saturating_sub(chrome::Chrome::FRAME_COLS + 1);
    let name = v.path.file_name().and_then(|n| n.to_str());
    let cut = matches!(
        &v.body,
        Body::Text {
            truncated: true,
            ..
        } | Body::Markdown {
            truncated: true,
            ..
        }
    );
    let first = if cut { "first 1 MiB · " } else { "" };
    let keys = if panel {
        "-/+ size · o open"
    } else {
        "space page · o open outside"
    };
    chrome::Chrome::new(
        chrome::fit_ellipsis(name.unwrap_or("file"), room.saturating_sub(9)),
        Anchor::Center,
    )
    .flat()
    .footer(format!("{first}j/k scroll · {keys} · esc close"))
    .fit_to(room.max(1))
}

/// Split `text` into rows of at most `w` columns, keeping its spaces: source
/// indent survives. A wide character never straddles a break.
fn hard_wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = chrome::char_cols(c);
        if used > 0 && used + cw > w {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(c);
        used += cw;
    }
    rows.push(row);
    rows
}

/// The first line a window can start on so the last line still shows.
fn max_scroll(lines: &[String], area: usize, budget: usize) -> usize {
    let mut rows = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        rows += hard_wrap(line, area).len();
        if rows > budget {
            return (i + 1).min(lines.len() - 1);
        }
    }
    0
}

struct Window {
    rows: Vec<chrome::BodyLine>,
    /// Source lines the window started.
    started: usize,
}

/// The markdown body rendered at the paint width, cached per width; none for
/// other bodies.
fn rendered<'a>(v: &'a FileViewer, plan: &Plan) -> Option<std::cell::Ref<'a, [BLine]>> {
    let Body::Markdown { src, .. } = &v.body else {
        return None;
    };
    // Refresh the cache in a scoped mutable borrow so the returned read
    // borrow starts after it ends (RefMut::downgrade is nightly-only).
    {
        let mut cache = v.md.borrow_mut();
        if cache.as_ref().is_none_or(|(w, _)| *w != plan.text_w) {
            *cache = Some((plan.text_w, md_lines(src, plan.text_w)));
        }
    }
    let slot = std::cell::Ref::filter_map(v.md.borrow(), Option::as_ref);
    slot.map(|c| std::cell::Ref::map(c, |p| p.1.as_slice()))
        .ok()
}

fn window(v: &FileViewer, plan: &Plan, md: Option<&[BLine]>) -> Window {
    let mut rows = Vec::new();
    if let Some(rendered) = md.filter(|r| !r.is_empty()) {
        let first = v.scroll.min(rendered.len() - 1);
        let mut started = 0;
        for line in rendered.iter().skip(first) {
            if rows.len() >= plan.budget {
                break;
            }
            // md_lines wrapped every row at paint width, so one rendered row
            // is one painted row and no line-number gutter applies.
            rows.push(to_body_line(line));
            started += 1;
        }
        return Window { rows, started };
    }
    let Some(lines) = v.lines() else {
        for note in v.notes() {
            for piece in hard_wrap(&note, plan.text_w) {
                rows.push(chrome::BodyLine::plain(piece));
            }
        }
        return Window { rows, started: 0 };
    };
    let g = digits(lines.len());
    let area = plan.area(v);
    let first = v.scroll.min(lines.len() - 1);
    let mut started = 0;
    for (i, text) in lines.iter().enumerate().skip(first) {
        if rows.len() >= plan.budget {
            break;
        }
        started += 1;
        for (k, piece) in hard_wrap(text, area).into_iter().enumerate() {
            if rows.len() >= plan.budget {
                break;
            }
            let gutter = if k == 0 {
                format!("{:>g$} ", i + 1)
            } else {
                " ".repeat(g + 1)
            };
            let mut row = chrome::BodyLine::plain(format!("{gutter}{piece}"));
            row.segs = vec![(0, g + 1, crate::theme::Role::BodyDim)];
            if v.target == Some(i) {
                row.sel_span = Some((g + 1, piece.chars().count().max(1)));
            }
            rows.push(row);
        }
    }
    Window { rows, started }
}

fn layout(plan: &Plan, v: &FileViewer, md: Option<&[BLine]>) -> OverlayLayout {
    let win = window(v, plan, md);
    let total = md.map_or_else(|| v.count(), <[BLine]>::len);
    let first = v.scroll.min(total.saturating_sub(1));
    let has_scroll = first > 0 || first + win.started < total;
    let mut body = win.rows;
    // A fixed-size block: the panel runs the full height, the modal keeps its
    // box as the file scrolls.
    body.resize(plan.budget.max(body.len()), chrome::BodyLine::plain(""));
    let scroll = has_scroll.then_some(chrome::Scroll {
        pos: first,
        total,
        visible: win.started,
    });
    let body_w = plan
        .w
        .saturating_sub(chrome::Chrome::FRAME_COLS + usize::from(has_scroll));
    OverlayLayout {
        origin: plan.origin,
        framed: chrome::frame(&body, &plan.chrome, body_w, scroll),
        window: (first, win.started),
        body_top: plan.chrome.rows_above(),
    }
}

/// Paint the viewer into the frame: after the panes, before the modals.
pub(super) fn draw(view: &View, cells: &mut [Cell], rows: usize, cols: usize) {
    let Some(v) = &view.viewer.open else {
        return;
    };
    let p = plan(view, v);
    let md = rendered(v, &p);
    let layout = layout(&p, v, md.as_deref());
    draw_overlay_layout(cells, rows, cols, &layout, &view.theme);
}

fn max_for(view: &View) -> usize {
    let Some(v) = &view.viewer.open else {
        return 0;
    };
    let plan = plan(view, v);
    if let Some(rendered) = rendered(v, &plan) {
        return rendered.len().saturating_sub(plan.budget);
    }
    v.lines()
        .map_or(0, |lines| max_scroll(lines, plan.area(v), plan.budget))
}

fn scroll_by(view: &mut View, delta: isize) {
    let max = max_for(view) as isize;
    if let Some(v) = view.viewer.open.as_mut() {
        v.scroll = (v.scroll as isize + delta).clamp(0, max) as usize;
    }
}

fn page(view: &View) -> isize {
    view.viewer.open.as_ref().map_or(1, |v| {
        plan(view, v).budget.saturating_sub(1).max(1) as isize
    })
}

async fn send_resize(
    view: &View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let (rows, cols) = view.content_dims();
    write_msg(sock_w, &ClientMsg::Resize { rows, cols })
        .await
        .map_err(|e| format!("resize send failed: {e}"))
}

/// Open a loaded file, replacing any viewer already open. A panel takes
/// columns from the panes, so the new content area travels to the server.
pub(super) async fn open(
    view: &mut View,
    loaded: Loaded,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let was_panel = view.viewer_panel_w() > 0;
    // A rendered markdown doc has no source rows, so a `path:line` hint on a
    // `.md` click cannot name a row to highlight.
    let target = match &loaded.body {
        Body::Markdown { .. } => None,
        _ => loaded.line.map(|l| (l as usize).saturating_sub(1)),
    };
    view.viewer.open = Some(FileViewer {
        path: loaded.path,
        shape: loaded.shape,
        body: loaded.body,
        scroll: 0,
        target,
        focused: true,
        md: RefCell::new(None),
        esc: Vec::new(),
    });
    view.region_owner = region_focus::RegionOwner::Pane;
    let budget = view
        .viewer
        .open
        .as_ref()
        .map_or(0, |v| plan(view, v).budget);
    let target = view.viewer.open.as_ref().and_then(|v| v.target);
    if let Some(t) = target {
        scroll_by(view, t.saturating_sub(budget / 3) as isize);
    }
    if was_panel || view.viewer_panel_w() > 0 {
        send_resize(view, sock_w).await?;
    }
    Ok(())
}

/// Close the viewer and give the panel's columns back to the panes.
pub(super) async fn close(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let was_panel = view.viewer_panel_w() > 0;
    view.viewer.open = None;
    if was_panel {
        send_resize(view, sock_w).await?;
    }
    Ok(())
}

async fn resize(
    view: &mut View,
    delta: i16,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if view.viewer_panel_w() == 0 {
        return Ok(());
    }
    let pct = (view.viewer.pct() as i16 + delta).clamp(MIN_PCT, MAX_PCT) as u16;
    if pct == view.viewer.pct() {
        return Ok(());
    }
    view.viewer.pct = pct;
    scroll_by(view, 0);
    send_resize(view, sock_w).await
}

async fn open_outside(view: &mut View) {
    let Some(path) = view.viewer.open.as_ref().map(|v| v.path.clone()) else {
        return;
    };
    let shown = path.display().to_string();
    let outcome = tokio::task::spawn_blocking(move || crate::link::open_fno_path(&path))
        .await
        .unwrap_or_else(|_| Err("opener task failed".to_string()));
    match outcome {
        Ok(()) => view.set_notice(format!("opened {shown}")),
        Err(e) => view.set_notice(format!("open failed: {e}")),
    }
}

/// The focused viewer's keys. Esc and q close; j/k and the arrows scroll a
/// line; space, b, PgDn and PgUp a page; g and G jump to the ends; - and +
/// resize the panel; o hands the file to the platform opener.
pub(super) async fn keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(v) = view.viewer.open.as_mut() else {
        return Ok(StdinFlow::Continue);
    };
    let mut carry = std::mem::take(&mut v.esc);
    let toks = fold_modal_keys(&mut carry, bytes);
    v.esc = carry;
    for tok in toks {
        if view.viewer.open.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc | ModalKey::Byte(b'q') => {
                close(view, sock_w).await?;
                break;
            }
            ModalKey::Up | ModalKey::Byte(b'k') => scroll_by(view, -1),
            ModalKey::Down | ModalKey::Byte(b'j') => scroll_by(view, 1),
            ModalKey::PageUp | ModalKey::Byte(b'b') => scroll_by(view, -page(view)),
            ModalKey::PageDown | ModalKey::Byte(b' ') => scroll_by(view, page(view)),
            ModalKey::Byte(b'g') => scroll_by(view, isize::MIN / 2),
            ModalKey::Byte(b'G') => scroll_by(view, isize::MAX / 2),
            ModalKey::Byte(b'-') => resize(view, -STEP_PCT, sock_w).await?,
            ModalKey::Byte(b'+' | b'=') => resize(view, STEP_PCT, sock_w).await?,
            ModalKey::Byte(b'o') => open_outside(view).await,
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// One mouse report while the viewer is open. `true` means the viewer took
/// it. A modal owns the pointer outright. A panel owns only its own columns:
/// a click inside focuses it, a click anywhere else hands the keyboard back
/// and falls through to what is under the pointer.
pub(super) fn mouse(view: &mut View, rep: crate::mouse::MouseReport) -> bool {
    let Some(v) = &view.viewer.open else {
        return false;
    };
    let modal = view.viewer_panel_w() == 0;
    let p = plan(view, v);
    let (row, col) = (rep.row as usize, rep.col as usize);
    let inside =
        row >= p.origin.0 && row < p.origin.0 + p.h && col >= p.origin.1 && col < p.origin.1 + p.w;
    if !inside && !modal {
        if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
            if let Some(v) = view.viewer.open.as_mut() {
                v.focused = false;
            }
        }
        return false;
    }
    match rep.kind {
        MouseKind::WheelUp => scroll_by(view, -WHEEL_ROWS),
        MouseKind::WheelDown => scroll_by(view, WHEEL_ROWS),
        MouseKind::Press(MouseButton::Left) if inside => {
            if let Some(v) = view.viewer.open.as_mut() {
                v.focused = true;
            }
        }
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::tests::view_with_agents;
    use super::*;

    fn file(n: usize) -> Loaded {
        let lines = (1..=n).map(|i| format!("line {i}")).collect();
        Loaded {
            path: PathBuf::from("/work/src/lib.rs"),
            line: None,
            shape: ViewerShape::Panel,
            body: Body::Text {
                lines,
                truncated: false,
            },
        }
    }

    fn bare_view() -> View {
        let mut v = view_with_agents(vec![]);
        v.term = (30, 120);
        v.panel_on = false;
        v
    }

    async fn opened(loaded: Loaded) -> (View, Vec<u8>) {
        let mut v = bare_view();
        let mut sock = Vec::new();
        open(&mut v, loaded, &mut sock).await.unwrap();
        (v, sock)
    }

    fn screen(v: &View) -> String {
        crate::vt::frame_text(&v.compose())
    }

    fn press(row: u16, col: u16) -> crate::mouse::MouseReport {
        crate::mouse::MouseReport {
            row,
            col,
            kind: MouseKind::Press(MouseButton::Left),
            shift: false,
            alt: false,
        }
    }

    #[test]
    fn load_reads_text_sniffs_binary_and_never_passes_a_control_byte() {
        let dir = std::env::temp_dir().join(format!("fno-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            load(p, None, ViewerShape::Panel).body
        };
        match read("a.txt", b"one\r\n\ttwo \x1b[31mred\nthree") {
            Body::Text { lines, truncated } => {
                assert_eq!(lines, ["one", "    two \u{b7}[31mred", "three"]);
                assert!(!truncated);
            }
            _ => panic!("text"),
        }
        assert!(matches!(read("b.bin", b"MZ\0\0\x01"), Body::Binary));
        assert!(matches!(read("c.md", b"# title\n"), Body::Markdown { .. }));
        match read("big.txt", &vec![b'x'; READ_CAP + 10]) {
            Body::Text { truncated, .. } => assert!(truncated),
            _ => panic!("big"),
        }
        let absent = load(dir.join("absent"), None, ViewerShape::Panel);
        assert!(matches!(absent.body, Body::Failed(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn panel_takes_columns_from_the_panes_and_the_feed_yields() {
        let mut v = bare_view();
        v.feed = Some(super::super::feed_view::open_overlay(None, 0));
        let before = v.content_dims().1;
        assert!(v.feed_panel_w() > 0 && before == 120 - v.feed_panel_w());
        let mut sock = Vec::new();
        open(&mut v, file(3), &mut sock).await.unwrap();
        assert_eq!(v.viewer_panel_w(), 48, "40% of 120");
        assert_eq!(v.feed_panel_w(), 0, "the viewer holds the right edge");
        assert_eq!(v.content_dims().1, 120 - 48);
        assert!(!sock.is_empty(), "the new content area travels");
        close(&mut v, &mut sock).await.unwrap();
        assert_eq!(v.content_dims().1, before, "closing gives the edge back");
    }

    #[tokio::test]
    async fn keys_scroll_resize_and_close() {
        let (mut v, mut sock) = opened(file(200)).await;
        let mut key = async |v: &mut View, b: &[u8]| {
            keys(v, b, &mut sock).await.unwrap();
        };
        let scroll = |v: &View| v.viewer.open.as_ref().unwrap().scroll;
        key(&mut v, b"jj").await;
        assert_eq!(scroll(&v), 2);
        key(&mut v, b"G").await;
        assert!(scroll(&v) > 100, "G reaches the end");
        assert!(screen(&v).contains("200 line 200"), "{}", screen(&v));
        key(&mut v, b"g").await;
        assert_eq!(scroll(&v), 0);
        key(&mut v, b"+").await;
        assert_eq!(v.viewer_panel_w(), 54, "45% of 120");
        for _ in 0..20 {
            key(&mut v, b"-").await;
        }
        assert_eq!(v.viewer_panel_w(), 24, "the floor is 20%");
        key(&mut v, b"q").await;
        assert!(v.viewer.open.is_none());
    }

    #[tokio::test]
    async fn paints_the_target_line_and_says_so_for_a_file_it_cannot_show() {
        let mut l = file(60);
        l.line = Some(40);
        let (v, _) = opened(l).await;
        let s = screen(&v);
        assert!(s.contains("lib.rs") && s.contains("40 line 40"), "{s}");
        assert!(!s.contains("line 1 "), "the window moved past line 1");
        for (body, shown) in [
            (Body::Binary, "no preview"),
            (Body::Failed("denied".into()), "denied"),
        ] {
            let mut l = file(1);
            l.body = body;
            assert!(screen(&opened(l).await.0).contains(shown));
        }
    }

    #[tokio::test]
    async fn a_markdown_doc_renders_through_md_lines_and_scrolls_as_rendered() {
        let doc = format!(
            "# Title\n\nintro **bold** [link](http://x/y)\n\n- one\n- two\n\n\
             ```rust\nlet x = 1;\n```\n\n{}tail marker\n",
            "mid paragraph\n\n".repeat(20)
        );
        let mut l = file(0);
        l.body = Body::Markdown {
            src: doc,
            truncated: false,
        };
        let (mut v, mut sock) = opened(l).await;
        assert!(
            v.viewer.open.as_ref().unwrap().target.is_none(),
            "a line hint names no rendered row"
        );
        let s = screen(&v);
        assert!(s.contains("Title") && !s.contains("# Title"), "{s}");
        assert!(s.contains("intro bold link"), "{s}");
        assert!(s.contains('\u{2022}') && s.contains("one"), "{s}");
        assert!(s.contains("let x = 1;") && !s.contains("```"), "{s}");
        assert!(!s.contains("tail marker"), "the tail is past the window");
        let mut key = async |v: &mut View, b: &[u8]| {
            keys(v, b, &mut sock).await.unwrap();
        };
        key(&mut v, b"G").await;
        assert!(screen(&v).contains("tail marker"), "G reaches the tail");
    }

    #[tokio::test]
    async fn an_empty_markdown_file_says_so() {
        let mut l = file(0);
        l.body = Body::Markdown {
            src: String::new(),
            truncated: false,
        };
        assert!(screen(&opened(l).await.0).contains("(empty file)"));
    }

    #[tokio::test]
    async fn a_modal_swallows_the_pointer_and_a_panel_leaves_it_to_the_panes() {
        let mut l = file(5);
        l.shape = ViewerShape::Modal;
        let (mut v, _) = opened(l).await;
        assert_eq!(v.content_dims().1, 120, "a modal takes no columns");
        assert!(mouse(&mut v, press(0, 0)), "an outside click is swallowed");
        assert!(v.viewer_focused());
        assert_eq!(super::super::esc_close::tap_chip(&mut v).await, 1);
        assert!(v.viewer.open.is_none(), "the chip closes a modal");

        let (mut v, _) = opened(file(5)).await;
        assert!(!mouse(&mut v, press(5, 2)), "a pane click passes on");
        assert!(!v.viewer_focused(), "and hands the keyboard back");
        assert!(mouse(&mut v, press(5, 100)) && v.viewer_focused());
        assert!(!mouse(&mut v, press(5, 2)));
        // A panel's close sends the new content area, so the chip tap that
        // `tap_chip` asserts silent is called directly here.
        let (mut scanner, mut sock) = (Scanner::default(), Vec::new());
        super::super::esc_close::tap(&mut v, &mut scanner, &mut sock)
            .await
            .unwrap();
        assert!(
            v.viewer.open.is_none(),
            "the chip closes an unfocused panel"
        );
        assert!(!sock.is_empty(), "and the panes get their columns back");
    }
}
