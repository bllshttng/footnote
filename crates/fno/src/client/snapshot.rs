//! `fno mux serve --snapshot`: compose one mux frame through the real
//! [`View::compose`] and write it as html, svg or png, with no terminal,
//! shell, scrollback or cursor in the picture.
//!
//! `--server <name>` names the server. The shot goes through the read-only
//! observer attach the web bridge uses: `rows: 0, cols: 0` never resizes a PTY
//! and the write half is dropped after the attach. A public shot comes from
//! `scripts/ops/mux-demo-snapshot.sh`, which builds a throwaway server with
//! invented work and shoots it here, so no real session text can leak.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{LayoutView, View};
use crate::frame_html::{self, Theme};
use crate::popup::Anchor;
use crate::proto::{self, ClientMsg, Frame, ServerMsg, BUILD_VERSION, PROTO_VERSION};

const USAGE: &str =
    "usage: fno mux serve --snapshot --server <name> --out <path> [--squad <name>] \
[--theme dark|light|macchiato] [--format html|svg|png] [--size <cols>x<rows> [--fit]] \
[--font <family>] [--message <fmail-id>] \
[--view bell|row-menu|tab-menu|sideline-menu]";

#[derive(Debug, PartialEq)]
pub enum Format {
    Html,
    Svg,
    Png,
}

/// A client-local surface a snapshot opens before composing. These live in
/// the client's own state, so the server frame never carries them; the seam
/// flips the same state the live keys flip, then paints.
#[derive(Debug, PartialEq)]
pub enum ViewKind {
    /// The notifications bell panel, with one synchronous gather like the
    /// live panel's `maybe_kick`.
    Bell,
    /// The sideline row context menu on the first agent row.
    RowMenu,
    /// The tab-strip context menu on the first tab.
    TabMenu,
    /// The sideline menu popup.
    SidelineMenu,
}

fn parse_view(v: &str) -> Option<ViewKind> {
    match v {
        "bell" => Some(ViewKind::Bell),
        "row-menu" => Some(ViewKind::RowMenu),
        "tab-menu" => Some(ViewKind::TabMenu),
        "sideline-menu" => Some(ViewKind::SidelineMenu),
        _ => None,
    }
}

#[derive(Debug)]
pub struct SnapshotArgs {
    pub out: PathBuf,
    pub server: String,
    pub squad: Option<String>,
    pub theme: Theme,
    pub format: Format,
    /// `(rows, cols)` of the whole picture; live mode defaults to the server's.
    pub size: Option<(u16, u16)>,
    /// Attach as a sizing client, so the server lays its panes out at `size`.
    /// It resizes every pane, so it is for a throwaway server only.
    pub fit: bool,
    /// The font family drawn first, ahead of the common monospace stack: an
    /// image has no terminal, so its font is named here or left to the stack.
    pub font: Option<String>,
    /// Open the Messages tab on this fmail id: the paint gate's shots of a
    /// client-local board (law d-9f18c4d5). The board gathers via a client
    /// subprocess, so the snapshot runs one gather synchronously; the id
    /// resolves to its thread and positions the cursor there.
    pub message: Option<String>,
    /// The client-local surface to open before composing, for shots of a
    /// bell panel or a menu no server frame ever carries.
    pub view: Option<ViewKind>,
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
    let mut theme = frame_html::theme_by_name("dark").expect("dark is a snapshot theme");
    let mut format = None;
    let mut size = None;
    let mut fit = false;
    let mut font = None;
    let mut message = None;
    let mut view = None;
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
            "--fit" => fit = true,
            "--font" => {
                let f = value()?;
                if f.is_empty() || f.contains(['\'', '"', '<', '>', '&']) {
                    return Err(format!(
                        "fno mux serve --snapshot: bad --font {f:?}\n{USAGE}"
                    ));
                }
                font = Some(f);
            }
            "--out" => out = Some(PathBuf::from(value()?)),
            "--message" => message = Some(value()?),
            "--view" => {
                let v = value()?;
                view = Some(parse_view(&v).ok_or_else(|| {
                    format!("fno mux serve --snapshot: unknown view {v:?}; use bell, row-menu, tab-menu or sideline-menu")
                })?)
            }
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
    let server = server.ok_or_else(|| {
        "fno mux serve --snapshot: --server is required; for a public shot with no real \
session text, run scripts/ops/mux-demo-snapshot.sh"
            .to_string()
    })?;
    if fit && size.is_none() {
        return Err(format!(
            "fno mux serve --snapshot: --fit needs --size\n{USAGE}"
        ));
    }
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
        fit,
        font,
        message,
        view,
    })
}

fn parse_size(v: &str) -> Option<(u16, u16)> {
    let (c, r) = v.split_once('x')?;
    let (cols, rows) = (c.parse::<u16>().ok()?, r.parse::<u16>().ok()?);
    (cols >= 40 && rows >= 10).then_some((rows, cols))
}

pub fn run(args: SnapshotArgs) -> i32 {
    // The chrome paints the mux theme a live client in this ground would run,
    // so its chips and badges carry that theme's own pairs.
    let chrome = match args.theme.name {
        "dark" => crate::theme::theme_footnote_superscript(),
        "light" => crate::theme::theme_footnote_paper(),
        _ => crate::theme::Theme::default_theme(),
    };
    let frame = live_frame(
        &args.server,
        args.squad.as_deref(),
        args.size,
        args.fit,
        chrome,
        args.message.as_deref(),
        args.view.as_ref(),
    );
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
    let font = |page: String| match &args.font {
        Some(f) => page.replace("'SF Mono',", &format!("'{f}','SF Mono',")),
        None => page,
    };
    let svg = || font(frame_html::frame_svg(frame, args.theme));
    let io = |e: std::io::Error| format!("cannot write {}: {e}", args.out.display());
    match args.format {
        Format::Html => {
            std::fs::write(&args.out, font(frame_html::screen_html(frame, args.theme))).map_err(io)
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
    let mut cmd = std::process::Command::new(&chrome);
    // Chrome hangs on macOS under a HOME that is not the user's, and the demo
    // script sets one so paths read `~/...`. Hand it the passwd home back.
    if let Some(home) = crate::live_store_fence::passwd_home() {
        cmd.env("HOME", home);
    }
    let status = cmd
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        // Its own profile: without this, headless Chrome races the user's
        // running Chrome for the default user-data-dir and exits 2.
        .arg(format!("--user-data-dir={}", dir.join("profile").display()))
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

/// Everything one observer attach delivers before it goes quiet.
struct Observed {
    layout: LayoutView,
    frames: HashMap<u64, Frame>,
}

fn live_frame(
    server: &str,
    squad: Option<&str>,
    size: Option<(u16, u16)>,
    fit: bool,
    chrome: crate::theme::Theme,
    message: Option<&str>,
    kind: Option<&ViewKind>,
) -> Result<Frame, String> {
    let socket = proto::socket_path(server)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A sizing client reports the content area, as a real client does.
    let dims = size
        .filter(|_| fit)
        .map(|t| View::new(t, server.into(), LayoutView::default()).content_dims());
    let mut seen = runtime.block_on(observe(&socket, cwd.clone(), dims))?;
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
            seen = runtime.block_on(observe(&socket, cwd, dims))?;
        }
    }
    let area = seen.layout.area;
    let mut view = View::new(area, server.into(), seen.layout);
    view.theme = chrome;
    // The snapshot paints what a live client in this cwd would, so the
    // card-graph config latches here the way attach_and_run does.
    view.card_graph = crate::digest_overlay::card_graph(Path::new(&cwd));
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
    runtime.block_on(super::backlog_board::fold_once(&mut view));
    // The Messages board gathers via a client-side subprocess and the
    // snapshot has no event loop, so the tab would paint empty. --message
    // opens the board on that fmail id and runs one gather synchronously;
    // the paint gate's shots come from here (law d-9f18c4d5).
    if let Some(id) = message {
        crate::client::messages_view::open_message(&mut view, id.to_string());
        let gen = view
            .messages_board
            .as_ref()
            .map(|b| b.gen)
            .unwrap_or_default();
        let mail = runtime.block_on(crate::messages_model::gather());
        crate::client::messages_view::apply_gather(&mut view, gen, mail, Err("no org tree".into()));
    }
    // The messages seam above is the shape: flip the client state the live
    // keys flip, run one synchronous gather where the live panel would kick
    // one, then compose. Menus build from the layout the server sent, so
    // they need no gather.
    match kind {
        Some(ViewKind::Bell) => {
            let gen = crate::client::bell::open(&mut view);
            let projection = runtime.block_on(crate::messages_model::gather());
            crate::client::bell::apply(&mut view, gen, projection);
        }
        Some(ViewKind::RowMenu) => {
            let i = view
                .display_rows()
                .iter()
                .position(|r| matches!(r, super::DisplayRow::Agent(_)))
                .ok_or("no agent rows to open a row menu on")?;
            view.open_row_menu(i, Anchor::Center);
        }
        Some(ViewKind::TabMenu) => {
            let tid = view
                .layout
                .squads
                .iter()
                .flat_map(|s| s.tabs.iter())
                .next()
                .map(|t| t.id)
                .ok_or("no tabs to open a tab menu on")?;
            view.open_tab_menu_by_id(tid, Anchor::Center);
        }
        Some(ViewKind::SidelineMenu) => view.open_sideline_menu(Anchor::Center),
        None => {}
    }
    crate::lattice::freeze_spin();
    Ok(view.compose())
}

/// `dims` set attaches as a sizing client at that content area and reads
/// for a settle window, so panes can redraw at the new size.
async fn observe(socket: &Path, cwd: String, dims: Option<(u16, u16)>) -> Result<Observed, String> {
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
            // (0, 0) is the observer sentinel: it never resizes a PTY.
            rows: dims.map_or(0, |d| d.0),
            cols: dims.map_or(0, |d| d.1),
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
    let settle = dims.map(|_| Instant::now() + Duration::from_millis(2500));
    loop {
        // Once the layout is in, wait only briefly for its panes' frames.
        let wait = match (&layout, settle) {
            (Some(_), Some(t)) => t.saturating_duration_since(Instant::now()),
            (None, _) => deadline.saturating_duration_since(Instant::now()),
            (Some(_), None) => Duration::from_millis(400),
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
        if let (Some(l), None) = (&layout, settle) {
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

    fn parse_extra(extra: &[&str]) -> Result<SnapshotArgs, String> {
        let mut tail: Vec<OsString> = ["--snapshot", "--server", "demo", "--out", "/tmp/s.png"]
            .iter()
            .map(OsString::from)
            .collect();
        tail.extend(extra.iter().map(OsString::from));
        parse(&tail)
    }

    #[test]
    fn view_flag_names_every_kind_and_refuses_the_rest() {
        for (name, kind) in [
            ("bell", ViewKind::Bell),
            ("row-menu", ViewKind::RowMenu),
            ("tab-menu", ViewKind::TabMenu),
            ("sideline-menu", ViewKind::SidelineMenu),
        ] {
            let args = parse_extra(&["--view", name]).expect(name);
            assert_eq!(args.view, Some(kind), "{name}");
        }
        let e = parse_extra(&["--view", "feed"]).expect_err("unknown view names the options");
        assert!(
            e.contains("bell, row-menu, tab-menu or sideline-menu"),
            "{e}"
        );
    }
}
