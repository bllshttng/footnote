//! `fno mux serve --snapshot`: compose one mux frame through the real
//! [`View::compose`] and write it as html, svg or png, with no terminal,
//! shell, scrollback or cursor in the picture.
//!
//! The default source is a staged demo fleet, so a public shot carries no real
//! session text. `--server <name>` shoots a live server instead, through the
//! same read-only observer attach the web bridge uses: `rows: 0, cols: 0`
//! never resizes a PTY and the write half is dropped after the attach.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{section_key, LayoutView, SectionView, View};
use crate::frame_html::{self, Theme};
use crate::proto::{
    self, AgentBadge, AgentRow, Cell, ClientMsg, Color, Frame, Reach, ServerMsg, SquadMeta,
    TabMeta, BUILD_VERSION, PROTO_VERSION,
};
use crate::tree::Rect;

const USAGE: &str =
    "usage: fno mux serve --snapshot --out <path> [--server <name>] [--squad <name>] \
[--theme dark|light|macchiato] [--format html|svg|png] [--size <cols>x<rows>]";

#[derive(Debug, PartialEq)]
pub enum Format {
    Html,
    Svg,
    Png,
}

#[derive(Debug)]
pub struct SnapshotArgs {
    pub out: PathBuf,
    /// `None` shoots the staged demo fleet.
    pub server: Option<String>,
    pub squad: Option<String>,
    pub theme: Theme,
    pub format: Format,
    /// `(rows, cols)` of the whole picture; live mode defaults to the server's.
    pub size: Option<(u16, u16)>,
}

/// True when a `serve` tail asks for a snapshot rather than the web bridge.
pub fn wants_snapshot(tail: &[OsString]) -> bool {
    tail.iter().any(|a| a == "--snapshot")
}

/// Parse the `serve --snapshot` tail. `Err` is the usage line (exit 2).
pub fn parse(tail: &[OsString]) -> Result<SnapshotArgs, String> {
    let mut out = None;
    let mut server = None;
    let mut squad = None;
    let mut theme = frame_html::DARK;
    let mut format = None;
    let mut size = None;
    let mut it = tail.iter();
    while let Some(a) = it.next() {
        let a = a.to_str().ok_or_else(|| USAGE.to_string())?;
        let mut value = || -> Result<String, String> {
            it.next()
                .and_then(|v| v.to_str())
                .map(str::to_string)
                .ok_or_else(|| format!("fno mux serve --snapshot: {a} needs a value\n{USAGE}"))
        };
        match a {
            "--snapshot" => {}
            "--out" => out = Some(PathBuf::from(value()?)),
            tok @ ("--server" | "--session") => {
                crate::mux_cli::note_server_flag(tok);
                server = Some(value()?)
            }
            "--squad" => squad = Some(value()?),
            "--theme" => {
                let v = value()?;
                theme = frame_html::theme_by_name(&v).ok_or_else(|| {
                    format!("fno mux serve --snapshot: unknown theme {v:?}; use dark, light or macchiato")
                })?
            }
            "--format" => {
                format = Some(match value()?.as_str() {
                    "html" => Format::Html,
                    "svg" => Format::Svg,
                    "png" => Format::Png,
                    v => {
                        return Err(format!(
                            "fno mux serve --snapshot: unknown format {v:?}; use html, svg or png"
                        ))
                    }
                })
            }
            "--size" => {
                let v = value()?;
                size = Some(parse_size(&v).ok_or_else(|| {
                    format!("fno mux serve --snapshot: --size wants <cols>x<rows>, got {v:?}")
                })?)
            }
            other => {
                return Err(format!(
                    "fno mux serve --snapshot: unknown flag {other:?}\n{USAGE}"
                ))
            }
        }
    }
    let out = out.ok_or_else(|| format!("fno mux serve --snapshot: --out is required\n{USAGE}"))?;
    // The extension names the format when the flag does not.
    let format = match format {
        Some(f) => f,
        None => match out.extension().and_then(|e| e.to_str()) {
            Some("html") => Format::Html,
            Some("png") => Format::Png,
            _ => Format::Svg,
        },
    };
    Ok(SnapshotArgs {
        out,
        server,
        squad,
        theme,
        format,
        size,
    })
}

fn parse_size(v: &str) -> Option<(u16, u16)> {
    let (c, r) = v.split_once('x')?;
    let (cols, rows) = (c.parse::<u16>().ok()?, r.parse::<u16>().ok()?);
    (cols >= 40 && rows >= 10).then_some((rows, cols))
}

pub fn run(args: SnapshotArgs) -> i32 {
    let frame = match &args.server {
        None => {
            let store = isolated_store();
            let frame = demo_frame(args.size.unwrap_or((34, 150)));
            let _ = std::fs::remove_dir_all(store);
            Ok(frame)
        }
        Some(server) => live_frame(server, args.squad.as_deref(), args.size),
    };
    match frame.and_then(|f| write(&f, &args)) {
        Ok(()) => {
            println!("{}", args.out.display());
            0
        }
        Err(e) => {
            eprintln!("fno mux serve --snapshot: {e}");
            1
        }
    }
}

fn write(frame: &Frame, args: &SnapshotArgs) -> Result<(), String> {
    let svg = || frame_html::frame_svg(frame, args.theme);
    let io = |e: std::io::Error| format!("cannot write {}: {e}", args.out.display());
    match args.format {
        Format::Html => {
            std::fs::write(&args.out, frame_html::screen_html(frame, args.theme)).map_err(io)
        }
        Format::Svg => std::fs::write(&args.out, svg()).map_err(io),
        Format::Png => png(&svg(), frame, &args.out),
    }
}

/// Rasterize through headless Chrome: the svg in a zero-margin page, a window
/// the exact size of the image, at 2x.
fn png(svg: &str, frame: &Frame, out: &Path) -> Result<(), String> {
    let chrome = find_chrome()
        .ok_or("png needs Chrome or Chromium: set FNO_CHROME to its binary, or use --format svg")?;
    let w = (frame.cols as f64 * frame_html::SVG_CELL_W).ceil() as u32;
    let h = (frame.rows as f64 * frame_html::SVG_CELL_H).ceil() as u32;
    let dir = std::env::temp_dir().join(format!("fno-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
    let page = dir.join("shot.html");
    std::fs::write(
        &page,
        format!("<!doctype html><style>html,body{{margin:0}}svg{{display:block}}</style>{svg}"),
    )
    .map_err(|e| format!("temp page: {e}"))?;
    let out_abs = std::path::absolute(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let status = std::process::Command::new(&chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        .arg("--force-device-scale-factor=2")
        .arg(format!("--window-size={w},{h}"))
        .arg(format!("--screenshot={}", out_abs.display()))
        .arg(format!("file://{}", page.display()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {}: {e}", chrome.display()));
    let _ = std::fs::remove_dir_all(&dir);
    match status? {
        s if s.success() && out_abs.exists() => Ok(()),
        s => Err(format!("{} exited {s} and wrote no png", chrome.display())),
    }
}

fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FNO_CHROME").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let mac = Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    if mac.exists() {
        return Some(mac.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    ["google-chrome", "chromium", "chromium-browser"]
        .iter()
        .flat_map(|name| std::env::split_paths(&path).map(move |d| d.join(name)))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// the staged demo fleet
// ---------------------------------------------------------------------------

const PANE_ID: u64 = 1;

/// The staged fleet, composed at `(rows, cols)`. Every name and line here is
/// invented, so nothing from a real session can reach a public page.
///
/// The sideline's saved widths and folds live in the user's view store, so
/// the caller points the store at an empty dir first ([`isolated_store`]).
fn demo_frame(term: (u16, u16)) -> Frame {
    let squads = vec![
        squad(1, "web", &["api", "checkout"]),
        squad(2, "mobile", &["release"]),
        squad(3, "docs", &["guides"]),
    ];
    let agents = vec![
        agent(1, "archer", "claude", AgentBadge::Working, Some(PANE_ID)),
        agent(1, "scout", "codex", AgentBadge::Working, None),
        agent(1, "reviewer", "opencode", AgentBadge::Blocked, None),
        agent(2, "builder", "claude", AgentBadge::Working, None),
        agent(2, "tester", "pi", AgentBadge::Done, None),
        agent(3, "scribe", "codex", AgentBadge::Working, None),
    ];
    let mut view = View::new(
        term,
        "demo".into(),
        LayoutView {
            squads: squads.clone(),
            active_squad: 1,
            panes: Vec::new(),
            focus: PANE_ID,
            area: (0, 0),
            agents,
            focus_node: None,
        },
    );
    for s in &squads {
        view.section_view
            .insert(section_key(s), SectionView::Expanded);
    }
    let (rows, cols) = view.content_dims();
    view.layout.panes = vec![(
        PANE_ID,
        Rect {
            x: 0,
            y: 0,
            rows,
            cols,
        },
    )];
    view.layout.area = (rows, cols);
    view.frames.insert(PANE_ID, demo_pane(rows, cols));
    view.compose()
}

/// An empty view store, so the demo is identical on every machine. Called
/// once, before any thread starts.
fn isolated_store() -> PathBuf {
    let store = std::env::temp_dir().join(format!("fno-snapshot-store-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&store);
    std::env::set_var("FNO_AGENTS_HOME", &store);
    store
}

fn squad(id: u64, name: &str, tabs: &[&str]) -> SquadMeta {
    SquadMeta {
        id,
        name: name.into(),
        canonical_cwd: format!("/code/{name}"),
        tabs: tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabMeta {
                id: i as u64,
                name: (*t).to_string(),
                named: true,
                panes: Vec::new(),
            })
            .collect(),
        active_tab: 0,
        panes: tabs.len(),
    }
}

fn agent(squad: u64, name: &str, harness: &str, badge: AgentBadge, pane: Option<u64>) -> AgentRow {
    AgentRow {
        squad: Some(squad),
        name: name.into(),
        harness: Some(harness.into()),
        model: None,
        route: None,
        pane_id: pane,
        portal: None,
        badge: Some(badge),
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        liveness_measured_at: None,
        harness_title: None,
        answerable: None,
        attach_id: None,
        external: false,
        seen: false,
        tab: None,
        cwd_base: None,
        tombstone: false,
        subline: None,
        account: None,
        updated_at: None,
        pr: None,
        pr_session_short: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        crown_title: None,
        spawned_by_session: None,
        lineage_kind: None,
        spawned_by_name: None,
        lineage_reason: None,
        harness_session_id: None,
        basis: None,
        last_activity_age_s: None,
        resumable: false,
        no_pane_reason: None,
        pane_activity: None,
        reach: Reach::Locate,
    }
}

/// The focused pane: an agent mid-task, in plain lines with a few accents.
fn demo_pane(rows: u16, cols: u16) -> Frame {
    const GREEN: Color = Color::Indexed(2);
    const CYAN: Color = Color::Indexed(6);
    const DIMMED: Color = Color::Indexed(8);
    let lines: [(&str, Color); 12] = [
        ("> add rate limiting to the checkout api", CYAN),
        ("", Color::Default),
        ("  Reading src/checkout/handler.ts", DIMMED),
        ("  Reading src/middleware/limits.ts", DIMMED),
        ("", Color::Default),
        (
            "  The handler has no limit today. I will add a token bucket",
            Color::Default,
        ),
        (
            "  per api key, 60 requests a minute, and a 429 with Retry-After.",
            Color::Default,
        ),
        ("", Color::Default),
        ("  Edited src/middleware/limits.ts  +42 -3", GREEN),
        ("  Edited src/checkout/handler.ts   +6 -1", GREEN),
        ("", Color::Default),
        ("  Running npm test -- limits ... 14 passed", GREEN),
    ];
    let mut cells = vec![Cell::default(); rows as usize * cols as usize];
    for (r, (text, fg)) in lines.iter().enumerate().take(rows as usize) {
        for (c, ch) in text.chars().enumerate().take(cols as usize) {
            cells[r * cols as usize + c] = Cell {
                c: ch,
                fg: *fg,
                bg: Color::Default,
                flags: 0,
            };
        }
    }
    Frame {
        rows,
        cols,
        cells,
        cursor_row: 0,
        cursor_col: 0,
        cursor_visible: false,
        scroll_offset: 0,
    }
}

// ---------------------------------------------------------------------------
// the live source
// ---------------------------------------------------------------------------

/// Everything one observer attach delivers before it goes quiet.
struct Observed {
    layout: LayoutView,
    frames: HashMap<u64, Frame>,
}

fn live_frame(
    server: &str,
    squad: Option<&str>,
    size: Option<(u16, u16)>,
) -> Result<Frame, String> {
    let socket = proto::socket_path(server)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut seen = runtime.block_on(observe(&socket, cwd))?;
    if let Some(name) = squad {
        let target = seen
            .layout
            .squads
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                let names: Vec<&str> = seen.layout.squads.iter().map(|s| s.name.as_str()).collect();
                format!(
                    "server {server:?} has no squad {name:?}; it has {}",
                    names.join(", ")
                )
            })?;
        if target.id != seen.layout.active_squad {
            // The server picks the squad from the attach cwd.
            let cwd = target.canonical_cwd.clone();
            seen = runtime.block_on(observe(&socket, cwd))?;
        }
    }
    let area = seen.layout.area;
    let mut view = View::new(area, server.into(), seen.layout);
    view.frames = seen.frames;
    view.term = match size {
        Some(t) => t,
        None => {
            // Grow the picture by exactly the chrome, so the content area
            // matches the rects the server computed. Twice, because the
            // sideline width depends on the terminal width.
            for _ in 0..2 {
                let (cr, cc) = view.content_dims();
                view.term = (
                    view.term.0 + area.0.saturating_sub(cr),
                    view.term.1 + area.1.saturating_sub(cc),
                );
            }
            view.term
        }
    };
    Ok(view.compose())
}

async fn observe(socket: &Path, cwd: String) -> Result<Observed, String> {
    let stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(socket),
    )
    .await
    .map_err(|_| format!("connect to {} timed out", socket.display()))?
    .map_err(|e| {
        format!(
            "cannot connect to {}: {e}; list servers with `fno mux ls`",
            socket.display()
        )
    })?;
    let (mut reader, mut writer) = stream.into_split();
    proto::write_msg(
        &mut writer,
        &ClientMsg::Attach {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            // The observer sentinel: never resizes a PTY.
            rows: 0,
            cols: 0,
            cwd,
        },
    )
    .await
    .map_err(|e| format!("attach write failed: {e}"))?;
    // Read-only from here: release the write half without the half-close
    // that would make the server detach us.
    writer.forget();

    let mut layout = None;
    let mut frames = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Once the layout is in, wait only briefly for its panes' frames.
        let wait = match &layout {
            None => deadline.saturating_duration_since(Instant::now()),
            Some(_) => Duration::from_millis(400),
        };
        let msg =
            match tokio::time::timeout(wait, proto::read_msg::<_, ServerMsg>(&mut reader)).await {
                Ok(Ok(m)) => m,
                Ok(Err(e)) => return Err(format!("read failed: {e}")),
                Err(_) if layout.is_some() => break,
                Err(_) => return Err("server sent no layout within 10s".into()),
            };
        match msg {
            ServerMsg::Layout {
                squads,
                active_squad,
                panes,
                focus,
                area,
                agents,
                focus_node,
                ..
            } => {
                layout = Some(LayoutView {
                    squads,
                    active_squad,
                    panes,
                    focus,
                    area,
                    agents,
                    focus_node,
                })
            }
            ServerMsg::Frame { pane_id, frame } if frame.geometry_ok() => {
                frames.insert(pane_id, frame);
            }
            ServerMsg::Bye { reason } => {
                return Err(format!("server refused the attach: {reason}"))
            }
            _ => {}
        }
        if let Some(l) = &layout {
            if l.panes.iter().all(|(id, _)| frames.contains_key(id)) {
                break;
            }
        }
    }
    let layout = layout.expect("loop exits with a layout");
    frames.retain(|id, _| layout.panes.iter().any(|(p, _)| p == id));
    Ok(Observed { layout, frames })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// The default source is the staged fleet: its names are on the picture,
    /// in both themes, and no cursor block is drawn.
    #[test]
    fn snapshot_demo_fleet_renders_in_both_themes() {
        let dir = tempfile::tempdir().unwrap();
        crate::view_store::set_test_path(dir.path());
        let frame = demo_frame((34, 150));
        crate::view_store::clear_test_path();
        let text = crate::vt::frame_text(&frame);
        for name in ["archer", "reviewer", "checkout", "rate limiting"] {
            assert!(text.contains(name), "{name} missing:\n{text}");
        }
        for theme in [frame_html::DARK, frame_html::LIGHT] {
            let svg = frame_html::frame_svg(&frame, theme);
            assert!(svg.contains("archer"));
        }
    }

    #[test]
    fn snapshot_parse_names_bad_values() {
        let ok = parse(&os(&["--snapshot", "--out", "x.png", "--theme", "light"])).unwrap();
        assert_eq!(ok.format, Format::Png);
        assert_eq!(ok.theme, frame_html::LIGHT);
        assert!(ok.server.is_none());
        let bad = parse(&os(&["--snapshot", "--out", "x", "--theme", "neon"])).unwrap_err();
        assert!(bad.contains("dark, light or macchiato"), "{bad}");
        let bad = parse(&os(&["--snapshot", "--out", "x", "--format", "gif"])).unwrap_err();
        assert!(bad.contains("html, svg or png"), "{bad}");
        assert!(parse(&os(&["--snapshot"]))
            .unwrap_err()
            .contains("--out is required"));
    }
}
