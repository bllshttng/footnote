//! `fno mux serve --share --pane <id>`: show one live pane to a viewer on
//! another network.
//!
//! The share attaches to the session the same passive way the web bridge does
//! (`web::connect_attach` forgets the socket's write half), forwards only the
//! shared pane's frames, masked by [`crate::redact`], and serves them on its
//! own loopback listener with its own secret. It mounts none of the web
//! bridge's backlog, fleet or team routes.
//!
//! The terminal that runs the share is the driver's panel. A viewer holding
//! the link knocks with a display name; the driver admits or denies each knock
//! by typing here. An admitted viewer can send two things:
//!
//! - coach: a note to the HUMAN, printed here. It never reaches the agent.
//! - steer: a note to the AGENT, only with `--steer`, and each one waits for
//!   the driver to type `y <n>`. It then goes as mail from `observer:<name>`,
//!   so the driver's agent processes it on the driver's account.
//!
//! No viewer byte reaches a pty: watch and coach carry nothing from a viewer to
//! the mux socket, and steer reaches the agent only through the mail verb after
//! consent. Each admitted viewer also holds a presence-only attach whose one
//! message is the viewer's cleaned name, so the mux can show who watches.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::mux_cli::control_roundtrip;
use crate::proto::{self, ControlVerb, Frame, ObserverHello, ObserverVia, PaneInfo, ServerMsg};
use crate::server::agent_actions::{run_mail_send, sanitize_mail_text};
use crate::server::observers::clean_label;
use crate::{redact, web};

/// Default share port: `serve --web` holds 8722 and its attention listener 8724.
pub const DEFAULT_SHARE_PORT: u16 = 8723;
/// Lines of masked history a viewer gets at admission, above the live grid.
const HISTORY_LINES: u16 = 500;
/// A viewer must knock within this window after the socket opens.
const KNOCK_WINDOW: Duration = Duration::from_secs(30);
/// Knocks that may wait for the driver at once.
const MAX_WAITING: usize = 3;
/// A wrong-link knock prints at most once per this gap, so a scan cannot
/// flood the driver's panel.
const REFUSAL_PRINT_GAP: Duration = Duration::from_secs(10);
/// The viewer drives nothing over the socket past its knock; cap what it sends.
const INBOUND_WS_CAP: usize = 64 * 1024;
const BROADCAST_CAP: usize = 256;
/// How long `--tunnel` waits for cloudflared to print its public URL.
const TUNNEL_URL_WINDOW: Duration = Duration::from_secs(30);

/// Parsed `fno mux serve --share` arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareArgs {
    pub session: String,
    pub pane: u64,
    pub steer: bool,
    pub tunnel: bool,
    pub port: u16,
    /// Run for a parent process, not a person: no terminal is needed and
    /// stdout carries `link`, `knock`, `gone` and `end` lines the parent
    /// parses, while the parent's stdin lines take the driver's commands.
    pub managed: bool,
}

// ---------------------------------------------------------------------------
// Pure decisions: the frame filter, pane identity, and the consent machine
// ---------------------------------------------------------------------------

/// The wire JSON for `msg` when it is a good frame of the shared pane, masked.
/// Every other message, `Layout` included, is dropped: nothing about the rest
/// of the session leaves the machine.
pub(crate) fn share_frame(msg: ServerMsg, pane: u64) -> Option<String> {
    let frame = masked_pane_frame(msg, pane)?;
    serde_json::to_string(&ServerMsg::Frame {
        pane_id: pane,
        frame,
    })
    .ok()
}

/// The masked frame when `msg` is a good frame of pane `pane`, else `None`.
/// The one place a pane's cells are made safe to leave the machine, shared by
/// the browser share and the ssh observer.
pub(crate) fn masked_pane_frame(msg: ServerMsg, pane: u64) -> Option<Frame> {
    let ServerMsg::Frame { pane_id, mut frame } = msg else {
        return None;
    };
    if pane_id != pane || !frame.geometry_ok() {
        return None;
    }
    frame.cells.iter_mut().for_each(crate::band_remap::remap);
    redact::redact_frame(&mut frame);
    Some(frame)
}

/// What makes a pane the same pane across a server restart, where pane ids
/// start over. The child pid comes first because it is set at spawn and never
/// changes; a harness session id can appear later, when the agent registers.
pub(crate) fn pane_identity(p: &PaneInfo) -> String {
    p.child_pid
        .map(|pid| format!("pid:{pid}"))
        .or_else(|| p.fno_id.clone())
        .unwrap_or_default()
}

/// Whether pane `pane` is still the pane the share started on.
pub(crate) fn pane_still_shared(panes: &[PaneInfo], pane: u64, identity: &str) -> bool {
    panes
        .iter()
        .any(|p| p.pane_id == pane && pane_identity(p) == identity)
}

/// The lines of `text` above the live grid: the grid's last `rows` lines are
/// already on the viewer's screen.
fn history_above_grid(text: &str, rows: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[..lines.len().saturating_sub(rows)].join("\n")
}

fn share_line(body: Value) -> String {
    json!({ "_share": body }).to_string()
}

/// What the async glue must do after a consent decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// One line on the driver's panel.
    Print(String),
    /// The share terminal's title, so the pane label shows it out of focus.
    Title(String),
    /// A text message to one viewer's socket.
    Send(u64, String),
    /// Close one viewer's socket with a policy close and this reason.
    Close(u64, &'static str),
    /// Send history and the live grid to a viewer just admitted.
    Admit(u64),
    /// A viewer began waiting (managed mode tells the parent).
    Knock { id: u64, name: String },
    /// A viewer stopped waiting: admitted, denied, or gone.
    Gone(u64),
    /// Mail an approved steer to the shared pane's agent.
    Mail {
        viewer: u64,
        steer: u64,
        from: String,
        text: String,
    },
}

/// The outcome of a knock.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Knock {
    Waiting(u64),
    Refused(&'static str),
}

enum Seat {
    Waiting,
    Admitted { token: String },
}

struct Viewer {
    name: String,
    seat: Seat,
}

struct Steer {
    viewer: u64,
    text: String,
}

/// The consent state machine. It holds who knocked, who was admitted, and
/// which steers wait, and returns effects instead of doing I/O, so tests drive
/// it with plain values.
pub(crate) struct Viewers {
    pane: u64,
    steer_on: bool,
    next_viewer: u64,
    next_steer: u64,
    viewers: BTreeMap<u64, Viewer>,
    steers: BTreeMap<u64, Steer>,
    last_refusal: Option<Instant>,
}

impl Viewers {
    pub(crate) fn new(pane: u64, steer_on: bool) -> Self {
        Viewers {
            pane,
            steer_on,
            next_viewer: 1,
            next_steer: 1,
            viewers: BTreeMap::new(),
            steers: BTreeMap::new(),
            last_refusal: None,
        }
    }

    pub(crate) fn ids(&self) -> Vec<u64> {
        self.viewers.keys().copied().collect()
    }

    fn panel(&self, mut effects: Vec<Effect>) -> Vec<Effect> {
        let watching: Vec<&str> = self
            .viewers
            .values()
            .filter(|v| matches!(v.seat, Seat::Admitted { .. }))
            .map(|v| v.name.as_str())
            .collect();
        let waiting = self.viewers.len() - watching.len();
        let names = if watching.is_empty() {
            "nobody".to_string()
        } else {
            watching.join(", ")
        };
        effects.push(Effect::Print(format!(
            "watching: {names}; waiting: {waiting}; steers waiting: {}",
            self.steers.len()
        )));
        effects.push(Effect::Title(format!(
            "share: {} watching, {waiting} waiting",
            watching.len()
        )));
        effects
    }

    pub(crate) fn knock(
        &mut self,
        secret_ok: bool,
        name: &str,
        now: Instant,
    ) -> (Knock, Vec<Effect>) {
        if !secret_ok {
            let quiet = self
                .last_refusal
                .is_some_and(|t| now.duration_since(t) < REFUSAL_PRINT_GAP);
            let mut effects = Vec::new();
            if !quiet {
                self.last_refusal = Some(now);
                effects.push(Effect::Print("refused a knock with a wrong link".into()));
            }
            return (Knock::Refused("invalid link"), effects);
        }
        let name = clean_label(name);
        if name.is_empty() {
            return (Knock::Refused("a display name is needed"), Vec::new());
        }
        let waiting = self
            .viewers
            .values()
            .filter(|v| matches!(v.seat, Seat::Waiting))
            .count();
        if waiting >= MAX_WAITING {
            return (Knock::Refused("too many waiting"), Vec::new());
        }
        let id = self.next_viewer;
        self.next_viewer += 1;
        let line = format!(
            "[{id}] {name} asks to watch pane {}. Type: a {id} to admit, d {id} to deny",
            self.pane
        );
        let knock = Effect::Knock {
            id,
            name: name.clone(),
        };
        self.viewers.insert(
            id,
            Viewer {
                name,
                seat: Seat::Waiting,
            },
        );
        let effects = vec![
            Effect::Send(id, share_line(json!({ "state": "waiting" }))),
            Effect::Print(line),
            knock,
        ];
        (Knock::Waiting(id), self.panel(effects))
    }

    pub(crate) fn admit(&mut self, id: u64, token: String) -> Vec<Effect> {
        let Some(v) = self
            .viewers
            .get_mut(&id)
            .filter(|v| matches!(v.seat, Seat::Waiting))
        else {
            return vec![Effect::Print(format!("no viewer {id} is waiting"))];
        };
        let admitted = share_line(json!({
            "state": "admitted",
            "viewer": token,
            "steer": self.steer_on,
        }));
        v.seat = Seat::Admitted { token };
        let line = format!("admitted {}", v.name);
        let effects = vec![
            Effect::Send(id, admitted),
            Effect::Admit(id),
            Effect::Gone(id),
            Effect::Print(line),
        ];
        self.panel(effects)
    }

    pub(crate) fn deny(&mut self, id: u64) -> Vec<Effect> {
        let Some(v) = self.viewers.remove(&id) else {
            return vec![Effect::Print(format!("no viewer {id}"))];
        };
        self.steers.retain(|_, s| s.viewer != id);
        let effects = vec![
            Effect::Close(id, "the driver declined"),
            Effect::Gone(id),
            Effect::Print(format!("declined {}", v.name)),
        ];
        self.panel(effects)
    }

    pub(crate) fn leave(&mut self, id: u64) -> Vec<Effect> {
        let Some(v) = self.viewers.remove(&id) else {
            return Vec::new();
        };
        self.steers.retain(|_, s| s.viewer != id);
        self.panel(vec![
            Effect::Gone(id),
            Effect::Print(format!("{} stopped watching", v.name)),
        ])
    }

    fn admitted_by_token(&self, token: &str) -> Option<(u64, &str)> {
        self.viewers.iter().find_map(|(id, v)| match &v.seat {
            Seat::Admitted { token: t }
                if web::constant_time_eq(t.as_bytes(), token.as_bytes()) =>
            {
                Some((*id, v.name.as_str()))
            }
            _ => None,
        })
    }

    pub(crate) fn coach(&self, token: &str, text: &str) -> Result<Vec<Effect>, (u16, String)> {
        let (_, name) = self
            .admitted_by_token(token)
            .ok_or((401, "unknown viewer".to_string()))?;
        let text = sanitize_mail_text(text).map_err(|e| (400, e))?;
        Ok(vec![Effect::Print(format!("coach \u{b7} {name}: {text}"))])
    }

    pub(crate) fn steer(
        &mut self,
        token: &str,
        text: &str,
    ) -> Result<(u64, Vec<Effect>), (u16, String)> {
        let (viewer, name) = self
            .admitted_by_token(token)
            .map(|(id, n)| (id, n.to_string()))
            .ok_or((401, "unknown viewer".to_string()))?;
        if !self.steer_on {
            return Err((403, "the driver has not turned steering on".to_string()));
        }
        let text = sanitize_mail_text(text).map_err(|e| (400, e))?;
        let n = self.next_steer;
        self.next_steer += 1;
        let line = format!(
            "[s{n}] {name} wants to tell the agent: \"{text}\". Type: y {n} to send, n {n} to drop"
        );
        self.steers.insert(n, Steer { viewer, text });
        Ok((n, self.panel(vec![Effect::Print(line)])))
    }

    pub(crate) fn approve(&mut self, n: u64) -> Vec<Effect> {
        let Some(s) = self.steers.remove(&n) else {
            return vec![Effect::Print(format!("no steer s{n} is waiting"))];
        };
        let name = self.viewers.get(&s.viewer).map_or("", |v| v.name.as_str());
        let mail = Effect::Mail {
            viewer: s.viewer,
            steer: n,
            from: format!("observer:{name}"),
            text: s.text,
        };
        self.panel(vec![mail])
    }

    pub(crate) fn decline(&mut self, n: u64) -> Vec<Effect> {
        let Some(s) = self.steers.remove(&n) else {
            return vec![Effect::Print(format!("no steer s{n} is waiting"))];
        };
        let effects = vec![
            Effect::Send(
                s.viewer,
                share_line(json!({ "steer": n, "state": "declined" })),
            ),
            Effect::Print(format!("dropped steer s{n}")),
        ];
        self.panel(effects)
    }
}

/// One line of driver input.
#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Admit(u64),
    Deny(u64),
    Approve(u64),
    Decline(u64),
    Quit,
}

const COMMANDS: &str =
    "commands: a <n> admit, d <n> deny, y <n> send steer, n <n> drop steer, q end the share";

fn parse_cmd(line: &str) -> Option<Cmd> {
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    if verb == "q" {
        return words.next().is_none().then_some(Cmd::Quit);
    }
    let n: u64 = words.next()?.trim_start_matches('s').parse().ok()?;
    if words.next().is_some() {
        return None;
    }
    match verb {
        "a" => Some(Cmd::Admit(n)),
        "d" => Some(Cmd::Deny(n)),
        "y" => Some(Cmd::Approve(n)),
        "n" => Some(Cmd::Decline(n)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Async glue
// ---------------------------------------------------------------------------

/// A message queued for one viewer's socket task.
enum Out {
    Text(String),
    /// Admission finished: send the latest grid and join the live stream.
    Live,
    Close(u16, String),
}

struct Hub {
    machine: Viewers,
    conns: HashMap<u64, mpsc::UnboundedSender<Out>>,
}

#[derive(Default)]
struct Snap {
    up: bool,
    frame: Option<String>,
    rows: u16,
}

#[derive(Clone)]
struct Ctx {
    hub: Arc<Mutex<Hub>>,
    secret: Arc<str>,
    tx: broadcast::Sender<String>,
    snap: Arc<Mutex<Snap>>,
    socket: Arc<PathBuf>,
    session: Arc<str>,
    pane: u64,
    page: Arc<str>,
    managed: bool,
}

fn run_effects(ctx: &Ctx, hub: &mut Hub, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::Print(line) if !ctx.managed => println!("{line}"),
            Effect::Title(title) if !ctx.managed => {
                print!("\x1b]2;{title}\x07");
                let _ = std::io::stdout().flush();
            }
            Effect::Print(_) | Effect::Title(_) => {}
            Effect::Knock { id, name } if ctx.managed => println!("knock {id} {name}"),
            Effect::Gone(id) if ctx.managed => println!("gone {id}"),
            Effect::Knock { .. } | Effect::Gone(_) => {}
            Effect::Send(id, text) => {
                if let Some(out) = hub.conns.get(&id) {
                    let _ = out.send(Out::Text(text));
                }
            }
            Effect::Close(id, reason) => {
                if let Some(out) = hub.conns.remove(&id) {
                    let _ = out.send(Out::Close(close_code::POLICY, reason.to_string()));
                }
            }
            Effect::Admit(id) => {
                if let Some(out) = hub.conns.get(&id).cloned() {
                    tokio::spawn(admit_flow(ctx.clone(), out));
                }
            }
            Effect::Mail {
                viewer,
                steer,
                from,
                text,
            } => {
                let out = hub.conns.get(&viewer).cloned();
                tokio::spawn(steer_flow(ctx.clone(), out, steer, from, text));
            }
        }
    }
}

pub(crate) fn list_panes(socket: &Path, session: &str) -> Result<Vec<PaneInfo>, String> {
    match control_roundtrip(socket, session, ControlVerb::PaneLs) {
        Ok(ServerMsg::PaneList { panes }) => Ok(panes),
        Ok(ServerMsg::Err { msg, .. }) => Err(msg),
        Ok(_) => Err("unexpected reply to pane ls".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

async fn list_panes_async(ctx: &Ctx) -> Result<Vec<PaneInfo>, String> {
    let (socket, session) = (ctx.socket.clone(), ctx.session.clone());
    tokio::task::spawn_blocking(move || list_panes(&socket, &session))
        .await
        .map_err(|e| e.to_string())?
}

fn read_history(socket: &Path, session: &str, pane: u64, lines: u16) -> Result<String, String> {
    let verb = ControlVerb::PaneRead {
        pane,
        lines: Some(lines),
        block: None,
    };
    match control_roundtrip(socket, session, verb) {
        Ok(ServerMsg::PaneText { text, .. }) => Ok(text),
        Ok(ServerMsg::Err { msg, .. }) => Err(msg),
        Ok(_) => Err("unexpected reply to pane read".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Admission: masked history above the grid, then the live stream. A history
/// error is reported to the viewer and admission goes on.
async fn admit_flow(ctx: Ctx, out: mpsc::UnboundedSender<Out>) {
    let rows = ctx.snap.lock().unwrap().rows;
    let (socket, session, pane) = (ctx.socket.clone(), ctx.session.clone(), ctx.pane);
    let lines = HISTORY_LINES.saturating_add(rows);
    let read = tokio::task::spawn_blocking(move || read_history(&socket, &session, pane, lines))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    let line = match read {
        Ok(text) => share_line(json!({
            "history": redact::redact_text(&history_above_grid(&text, rows as usize)),
        })),
        Err(e) => share_line(json!({ "history_error": e })),
    };
    let _ = out.send(Out::Text(line));
    let _ = out.send(Out::Live);
}

/// One presence-only attach per admitted viewer. The server lists the viewer
/// on the shared pane (border count, sideline) for as long as this task holds
/// the socket, and `ws_conn` aborts it when the viewer goes. A ticket the
/// server refuses costs the viewer nothing but the badge.
async fn presence_ticket(ctx: Ctx, name: String) {
    let hello = ObserverHello {
        name,
        pane: ctx.pane,
        via: ObserverVia::Share,
        frames: false,
    };
    match web::connect_attach(&ctx.socket, Some(hello)).await {
        Ok((mut reader, _)) => while proto::read_msg::<_, ServerMsg>(&mut reader).await.is_ok() {},
        Err(e) => println!("this viewer will not show on the mux: {e}"),
    }
}

/// An approved steer: resolve the pane's agent and mail it from the viewer.
async fn steer_flow(
    ctx: Ctx,
    out: Option<mpsc::UnboundedSender<Out>>,
    steer: u64,
    from: String,
    text: String,
) {
    let pane = ctx.pane;
    let agent = list_panes_async(&ctx).await.ok().and_then(|panes| {
        panes
            .into_iter()
            .find(|p| p.pane_id == pane)
            .and_then(|p| p.harness_session_id)
    });
    let (state, verdict) = match agent {
        None => (
            "dropped",
            format!("pane {pane} hosts no registered agent; steer dropped"),
        ),
        Some(id) => ("sent", run_mail_send(&id, &from, &text).await),
    };
    println!("[s{steer}] {verdict}");
    if let Some(out) = out {
        let line = share_line(json!({ "steer": steer, "state": state, "verdict": verdict }));
        let _ = out.send(Out::Text(line));
    }
}

fn publish(ctx: &Ctx, msg: ServerMsg) {
    let rows = match &msg {
        ServerMsg::Frame { frame, .. } => frame.rows,
        _ => 0,
    };
    if let Some(json) = share_frame(msg, ctx.pane) {
        {
            let mut s = ctx.snap.lock().unwrap();
            s.frame = Some(json.clone());
            s.rows = rows;
        }
        let _ = ctx.tx.send(json);
    }
}

fn set_up(ctx: &Ctx, up: bool) {
    ctx.snap.lock().unwrap().up = up;
    let _ = ctx.tx.send(web::bridge_status(if up {
        "connected"
    } else {
        "disconnected"
    }));
}

/// Attach as an observer, forward the shared pane, reconnect on EOF. After
/// every attach the pane's identity is checked again: a restarted server can
/// give the same id to a different pane, and the share never follows it.
async fn upstream(
    ctx: Ctx,
    identity: String,
    ready: oneshot::Sender<Result<(), String>>,
    end: mpsc::UnboundedSender<(i32, String)>,
) {
    let mut ready = Some(ready);
    let mut backoff = web::Backoff::new();
    loop {
        match web::connect_attach(&ctx.socket, None).await {
            Ok((mut reader, preamble)) => {
                let started = Instant::now();
                match list_panes_async(&ctx).await {
                    Ok(panes) if pane_still_shared(&panes, ctx.pane, &identity) => {}
                    Ok(_) => {
                        let _ =
                            end.send((1, format!("shared pane {} is gone; share ended", ctx.pane)));
                        return;
                    }
                    Err(e) => {
                        if let Some(r) = ready.take() {
                            let _ = r.send(Err(e));
                            return;
                        }
                        eprintln!("fno mux serve --share: pane check failed: {e}");
                        backoff.wait().await;
                        continue;
                    }
                }
                if let Some(r) = ready.take() {
                    let _ = r.send(Ok(()));
                }
                set_up(&ctx, true);
                publish(&ctx, preamble);
                while let Ok(msg) = proto::read_msg::<_, ServerMsg>(&mut reader).await {
                    publish(&ctx, msg);
                }
                set_up(&ctx, false);
                if backoff.dropped(started.elapsed()) {
                    eprintln!("fno mux serve --share: upstream dropped; backing off");
                }
            }
            Err(e) => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(e));
                    return;
                }
                eprintln!("fno mux serve --share: upstream reconnect failed: {e}");
            }
        }
        backoff.wait().await;
    }
}

fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/ws", get(ws_handler))
        .route("/coach", post(coach))
        .route("/steer", post(steer))
        .with_state(ctx)
}

/// The share fragment that replaces the page's nav placeholder: the pane name,
/// the knock prompt, the waiting banner, and the coach and steer boxes. The
/// page's own script reads `FNO_SHARE` and runs the share path.
fn share_fragment(pane: u64) -> String {
    format!(
        r#"<nav class="fno-nav share-nav" data-base="/"><span class="share-tag">shared pane {pane}</span> <span id="share-who"></span></nav>
<div id="share-wait" class="share-banner" hidden>waiting for the driver to let you in</div>
<form id="share-boxes" class="share-boxes" hidden>
  <input id="share-coach" maxlength="400" placeholder="note to the driver (never reaches the agent)">
  <button id="share-coach-send" type="button">coach</button>
  <span id="share-steer-row" hidden><input id="share-steer" maxlength="400" placeholder="note to the agent (the driver approves each one)">
  <button id="share-steer-send" type="button">steer</button></span>
  <span id="share-note"></span>
</form>
<script>window.FNO_SHARE = true;</script>"#
    )
}

async fn page(State(ctx): State<Ctx>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ctx.page.to_string(),
    )
}

async fn ws_handler(ws: WebSocketUpgrade, State(ctx): State<Ctx>) -> Response {
    ws.max_message_size(INBOUND_WS_CAP)
        .on_upgrade(move |socket| ws_conn(socket, ctx))
}

async fn first_text(socket: &mut WebSocket) -> Option<String> {
    loop {
        match socket.recv().await? {
            Ok(Message::Text(t)) => return Some(t.as_str().to_owned()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

fn parse_knock(text: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let k = v.get("knock")?;
    Some((
        k.get("k")?.as_str()?.to_string(),
        k.get("name")?.as_str()?.to_string(),
    ))
}

async fn close_with(socket: &mut WebSocket, code: u16, reason: String) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

async fn send_all(socket: &mut WebSocket, texts: Vec<String>) -> bool {
    for text in texts {
        if socket.send(Message::Text(text.into())).await.is_err() {
            return false;
        }
    }
    true
}

async fn recv_live(
    rx: &mut Option<broadcast::Receiver<String>>,
) -> Result<String, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn ws_conn(mut socket: WebSocket, ctx: Ctx) {
    let knock = tokio::time::timeout(KNOCK_WINDOW, first_text(&mut socket))
        .await
        .ok()
        .flatten()
        .and_then(|t| parse_knock(&t));
    let Some((secret, name)) = knock else {
        close_with(
            &mut socket,
            close_code::POLICY,
            "knock with the join link first".into(),
        )
        .await;
        return;
    };
    let secret_ok = web::constant_time_eq(secret.as_bytes(), ctx.secret.as_bytes());
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let outcome = {
        let mut hub = ctx.hub.lock().unwrap();
        let (knock, effects) = hub.machine.knock(secret_ok, &name, Instant::now());
        if let Knock::Waiting(id) = knock {
            hub.conns.insert(id, out_tx);
        }
        run_effects(&ctx, &mut hub, effects);
        knock
    };
    let id = match outcome {
        Knock::Waiting(id) => id,
        Knock::Refused(reason) => {
            close_with(&mut socket, close_code::POLICY, reason.to_string()).await;
            return;
        }
    };

    // A waiting socket never subscribes to the frame broadcast: it gets the
    // live stream only after `Out::Live`, which only admission sends.
    let mut live: Option<broadcast::Receiver<String>> = None;
    let mut ticket: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(Out::Text(text)) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(Out::Live) => {
                    // Subscribe before reading the snapshot, so no frame falls
                    // in the gap between them.
                    live = Some(ctx.tx.subscribe());
                    ticket.get_or_insert_with(|| {
                        tokio::spawn(presence_ticket(ctx.clone(), name.clone()))
                    });
                    let preamble = {
                        let s = ctx.snap.lock().unwrap();
                        let mut p = vec![web::bridge_status(if s.up { "connected" } else { "disconnected" })];
                        p.extend(s.frame.clone());
                        p
                    };
                    if !send_all(&mut socket, preamble).await {
                        break;
                    }
                }
                Some(Out::Close(code, reason)) => {
                    close_with(&mut socket, code, reason).await;
                    break;
                }
                None => break,
            },
            r = recv_live(&mut live) => match r {
                Ok(text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            r = socket.recv() => match r {
                // Past the knock, every inbound message is dropped.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    if let Some(ticket) = ticket {
        ticket.abort();
    }
    // A connection the hub still holds ended on the viewer's side. One the hub
    // already dropped was closed by the driver (deny) or by the share ending,
    // and needs no "stopped watching" line.
    let mut hub = ctx.hub.lock().unwrap();
    if hub.conns.remove(&id).is_some() {
        let effects = hub.machine.leave(id);
        run_effects(&ctx, &mut hub, effects);
    }
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}

fn body_text(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("text")?.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn reply(code: u16, body: Value) -> Response {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn coach(State(ctx): State<Ctx>, headers: HeaderMap, body: String) -> Response {
    let mut hub = ctx.hub.lock().unwrap();
    match hub.machine.coach(bearer(&headers), &body_text(&body)) {
        Ok(effects) => {
            run_effects(&ctx, &mut hub, effects);
            reply(200, json!({ "ok": true }))
        }
        Err((code, msg)) => reply(code, json!({ "error": msg })),
    }
}

async fn steer(State(ctx): State<Ctx>, headers: HeaderMap, body: String) -> Response {
    let mut hub = ctx.hub.lock().unwrap();
    match hub.machine.steer(bearer(&headers), &body_text(&body)) {
        Ok((n, effects)) => {
            run_effects(&ctx, &mut hub, effects);
            reply(202, json!({ "pending": n }))
        }
        Err((code, msg)) => reply(code, json!({ "error": msg })),
    }
}

/// Read the driver's lines on a plain thread (never the blocking pool, which
/// a runtime shutdown would wait on). End of input ends the share.
fn spawn_stdin(tx: mpsc::UnboundedSender<Option<String>>) {
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(Some(line)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
}

fn on_command(ctx: &Ctx, line: &str) -> bool {
    let Some(cmd) = parse_cmd(line) else {
        if !line.trim().is_empty() {
            println!("{COMMANDS}");
        }
        return true;
    };
    let mut hub = ctx.hub.lock().unwrap();
    let effects = match cmd {
        Cmd::Quit => return false,
        Cmd::Admit(n) => match web::mint_token() {
            Some(token) => hub.machine.admit(n, token),
            None => vec![Effect::Print(
                "cannot read /dev/urandom; viewer not admitted".into(),
            )],
        },
        Cmd::Deny(n) => hub.machine.deny(n),
        Cmd::Approve(n) => hub.machine.approve(n),
        Cmd::Decline(n) => hub.machine.decline(n),
    };
    run_effects(ctx, &mut hub, effects);
    true
}

/// Start `cloudflared` for a quick tunnel and wait for its public URL. The
/// child keeps draining stderr after the URL (a full pipe would stall it), and
/// its exit ends the share.
async fn start_tunnel(
    port: u16,
    end: mpsc::UnboundedSender<(i32, String)>,
) -> Result<String, (i32, String)> {
    let origin = format!("http://127.0.0.1:{port}");
    let mut cmd = crate::process_admission::tokio_command("cloudflared");
    cmd.args(["tunnel", "--no-autoupdate", "--url", origin.as_str()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match crate::process_admission::tokio_spawn(&mut cmd) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                2,
                "cloudflared not found; install it (macOS: brew install cloudflared) or run without --tunnel"
                    .into(),
            ))
        }
        Err(e) => return Err((1, format!("cannot start cloudflared: {e}"))),
    };
    let Some(stderr) = child.stderr.take() else {
        return Err((1, "cloudflared stderr unavailable".into()));
    };
    let (url_tx, url_rx) = oneshot::channel::<String>();
    tokio::spawn(async move {
        let mut url_tx = Some(url_tx);
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = tunnel_url(&line) {
                if let Some(tx) = url_tx.take() {
                    let _ = tx.send(url);
                }
            }
        }
    });
    tokio::spawn(async move {
        let _ = child.wait().await;
        let _ = end.send((1, "tunnel ended; viewers are disconnected".into()));
    });
    match tokio::time::timeout(TUNNEL_URL_WINDOW, url_rx).await {
        Ok(Ok(url)) => Ok(url),
        _ => Err((
            1,
            "cloudflared printed no trycloudflare.com URL in 30 s".into(),
        )),
    }
}

fn tunnel_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '-')))
        .unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

/// Entry point for the `mux serve --share` role; returns the exit code.
pub fn serve(args: ShareArgs) -> i32 {
    if !crate::view_store::load_experimental(crate::view_store::EXPERIMENTAL_PANE_SHARE) {
        eprintln!(
            "fno mux serve --share is experimental and off: turn on \"pane share\" in the mux sideline menu, under experimental"
        );
        return 2;
    }
    if !args.managed && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal()) {
        eprintln!(
            "fno mux serve --share needs a terminal: the driver admits viewers and approves steers by typing here"
        );
        return 2;
    }
    let socket = match proto::socket_path(&args.session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux serve --share: {e}");
            return 2;
        }
    };
    let identity = match list_panes(&socket, &args.session) {
        Ok(panes) => match panes.iter().find(|p| p.pane_id == args.pane) {
            Some(p) => pane_identity(p),
            None => {
                eprintln!(
                    "fno mux serve --share: no pane {} in session {:?}; list panes with `fno mux pane ls`",
                    args.pane, args.session
                );
                return 2;
            }
        },
        Err(e) => {
            eprintln!("fno mux serve --share: {e}");
            return 2;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno mux serve --share: cannot start runtime: {e}");
            return 1;
        }
    };
    let code = runtime.block_on(run(args, socket, identity));
    runtime.shutdown_timeout(Duration::from_millis(500));
    code
}

async fn run(args: ShareArgs, socket: PathBuf, identity: String) -> i32 {
    let Some(secret) = web::mint_token() else {
        eprintln!("fno mux serve --share: cannot read /dev/urandom to mint the share secret");
        return 1;
    };
    let (tx, _rx0) = broadcast::channel::<String>(BROADCAST_CAP);
    let page = web::PAGE.replacen("<!--fno-nav-->", &share_fragment(args.pane), 1);
    let ctx = Ctx {
        hub: Arc::new(Mutex::new(Hub {
            machine: Viewers::new(args.pane, args.steer),
            conns: HashMap::new(),
        })),
        secret: secret.clone().into(),
        tx,
        snap: Arc::new(Mutex::new(Snap::default())),
        socket: Arc::new(socket),
        session: args.session.clone().into(),
        pane: args.pane,
        page: page.into(),
        managed: args.managed,
    };

    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<(i32, String)>();
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(upstream(ctx.clone(), identity, ready_tx, end_tx.clone()));
    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("fno mux serve --share: {e}");
            return 1;
        }
        Err(_) => {
            eprintln!("fno mux serve --share: upstream task exited before attaching");
            return 1;
        }
    }

    // Loopback only: off-machine reach is the tunnel child's job, or the
    // driver's own ssh -R.
    let addr = web::bind_addr("127.0.0.1", args.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fno mux serve --share: cannot bind {addr}: {e}");
            return 1;
        }
    };
    let port = listener.local_addr().map_or(args.port, |a| a.port());
    let app = router(ctx.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("fno mux serve --share: server error: {e}");
        }
    });

    let link = if args.tunnel {
        match start_tunnel(port, end_tx.clone()).await {
            Ok(url) => format!("{url}/#k={secret}"),
            Err((code, msg)) => {
                eprintln!("fno mux serve --share: {msg}");
                return code;
            }
        }
    } else {
        format!("http://127.0.0.1:{port}/#k={secret}")
    };
    if args.managed {
        println!("link {link}");
    } else if args.tunnel {
        println!("join link: {link}");
    } else {
        println!("join link (this machine only): {link}");
        println!(
            "  for a viewer on another network, add --tunnel (needs cloudflared), or forward port {port} with ssh -R"
        );
    }
    if !args.managed {
        println!(
            "sharing pane {} only, masked{}. {COMMANDS}",
            args.pane,
            if args.steer { "; steering on" } else { "" }
        );
    }

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    spawn_stdin(cmd_tx);
    let (code, reason) = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break (0, "share ended".to_string()),
            end = end_rx.recv() => break end.unwrap_or((0, "share ended".to_string())),
            line = cmd_rx.recv() => match line {
                Some(Some(line)) => {
                    if !on_command(&ctx, &line) {
                        break (0, "share ended".to_string());
                    }
                }
                _ => break (0, "share ended".to_string()),
            },
        }
    };
    {
        let mut hub = ctx.hub.lock().unwrap();
        for id in hub.machine.ids() {
            if let Some(out) = hub.conns.remove(&id) {
                let _ = out.send(Out::Close(close_code::AWAY, reason.clone()));
            }
        }
    }
    if args.managed {
        println!("end {reason}");
    } else {
        println!("{reason}");
    }
    // Let the close frames reach the viewers before the runtime stops.
    tokio::time::sleep(Duration::from_millis(200)).await;
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Cell, Frame};

    fn frame_with(text: &str) -> Frame {
        let cols = 60u16;
        let mut chars: Vec<char> = text.chars().collect();
        chars.resize(cols as usize, ' ');
        Frame {
            rows: 1,
            cols,
            cells: chars
                .into_iter()
                .map(|c| Cell {
                    c,
                    ..Cell::default()
                })
                .collect(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            scroll_offset: 0,
        }
    }

    fn pane(id: u64, pid: Option<u32>) -> PaneInfo {
        serde_json::from_value(json!({
            "pane_id": id, "squad_id": 1, "tab_id": 1, "cwd": "/", "child_pid": pid, "title": null,
        }))
        .expect("pane row")
    }

    #[test]
    fn only_the_shared_pane_leaves_masked_and_a_reused_id_is_gone() {
        let token = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        let bye: ServerMsg = serde_json::from_value(json!({"Bye": {"reason": "x"}})).expect("bye");
        assert_eq!(share_frame(bye, 3), None);
        let other = ServerMsg::Frame {
            pane_id: 7,
            frame: frame_with("hello"),
        };
        assert_eq!(share_frame(other, 3), None);
        let mut bad = frame_with("short");
        bad.cells.pop();
        assert_eq!(
            share_frame(
                ServerMsg::Frame {
                    pane_id: 3,
                    frame: bad
                },
                3
            ),
            None
        );
        let shared = ServerMsg::Frame {
            pane_id: 3,
            frame: frame_with(&format!("tok {token}")),
        };
        let json = share_frame(shared, 3).expect("pane 3 frame forwards");
        assert!(json.contains("\"pane_id\":3"), "{json}");
        assert!(!json.contains("ghp_A1b2"), "{json}");

        let start = [pane(3, Some(41))];
        let identity = pane_identity(&start[0]);
        assert!(pane_still_shared(&start, 3, &identity));
        assert!(
            !pane_still_shared(&[pane(3, Some(99))], 3, &identity),
            "same id, other pane"
        );
        assert!(
            !pane_still_shared(&[pane(4, Some(41))], 3, &identity),
            "id gone"
        );

        assert_eq!(history_above_grid("a\nb\nc\nd", 2), "a\nb");
        assert_eq!(history_above_grid("a", 5), "");
        assert_eq!(
            tunnel_url("INF |  https://red-fox-1.trycloudflare.com  |"),
            Some("https://red-fox-1.trycloudflare.com".into())
        );
        assert_eq!(tunnel_url("see https://www.cloudflare.com/terms"), None);
    }

    fn sends_to(effects: &[Effect], id: u64) -> Vec<&str> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send(to, t) if *to == id => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn mails(effects: &[Effect]) -> Vec<&Effect> {
        effects
            .iter()
            .filter(|e| matches!(e, Effect::Mail { .. }))
            .collect()
    }

    #[test]
    fn consent_machine_rows() {
        let now = Instant::now();
        let mut m = Viewers::new(3, false);

        // Wrong link: refused, printed once per gap.
        let (k, fx) = m.knock(false, "eve", now);
        assert_eq!(k, Knock::Refused("invalid link"));
        assert_eq!(fx.len(), 1);
        let (_, fx) = m.knock(false, "eve", now + Duration::from_secs(1));
        assert!(
            fx.is_empty(),
            "a second wrong knock inside the gap stays quiet"
        );
        // A name that filters to nothing is refused.
        assert_eq!(
            m.knock(true, "``", now).0,
            Knock::Refused("a display name is needed")
        );

        // Three may wait; a fourth is refused. A waiting viewer gets no frame
        // and no admission line, only the waiting state.
        for n in 1..=3 {
            let (k, fx) = m.knock(true, "Ms. Lee`", now);
            assert_eq!(k, Knock::Waiting(n));
            assert_eq!(sends_to(&fx, n), vec![r#"{"_share":{"state":"waiting"}}"#]);
            assert!(fx.contains(&Effect::Knock {
                id: n,
                name: "Ms. Lee".into()
            }));
            assert!(!fx.iter().any(|e| matches!(e, Effect::Admit(_))));
        }
        assert_eq!(
            m.knock(true, "four", now).0,
            Knock::Refused("too many waiting")
        );

        // Before admission there is no token, so coach and steer are 401.
        assert_eq!(m.coach("", "hi").unwrap_err().0, 401);
        assert_eq!(m.steer("", "hi").unwrap_err().0, 401);

        // Deny closes the socket; admit hands out a token and starts admission.
        let fx = m.deny(2);
        assert!(fx.contains(&Effect::Close(2, "the driver declined")));
        assert!(
            fx.contains(&Effect::Gone(2)),
            "a denied knock stops waiting"
        );
        let fx = m.admit(1, "tok-1".into());
        assert!(fx.contains(&Effect::Admit(1)));
        assert!(
            fx.contains(&Effect::Gone(1)),
            "an admitted knock stops waiting"
        );
        assert!(sends_to(&fx, 1)[0].contains(r#""viewer":"tok-1""#));
        assert!(fx.contains(&Effect::Print("admitted Ms. Lee".into())));

        // Coach prints for the human and never mails.
        let fx = m.coach("tok-1", "  look at line 4 ").expect("coach");
        assert_eq!(
            fx,
            vec![Effect::Print("coach \u{b7} Ms. Lee: look at line 4".into())]
        );
        assert_eq!(m.coach("tok-1", " \x07 ").unwrap_err().0, 400);
        assert_eq!(m.coach("wrong", "hi").unwrap_err().0, 401);
        // Steer without --steer: 403, and no mail.
        assert_eq!(m.steer("tok-1", "hi").unwrap_err().0, 403);

        // Leaving prints and forgets the viewer.
        let fx = m.leave(1);
        assert!(fx.contains(&Effect::Print("Ms. Lee stopped watching".into())));
        assert_eq!(m.coach("tok-1", "hi").unwrap_err().0, 401);
    }

    #[test]
    fn steer_mails_only_after_the_driver_says_y() {
        let now = Instant::now();
        let mut m = Viewers::new(3, true);
        let (Knock::Waiting(id), _) = m.knock(true, "Ms. Lee", now) else {
            panic!("knock waits");
        };
        m.admit(id, "tok".into());

        let (n, fx) = m.steer("tok", "run the tests").expect("queued");
        assert!(mails(&fx).is_empty(), "nothing mails before consent");
        let fx = m.decline(n);
        assert!(mails(&fx).is_empty());
        assert!(sends_to(&fx, id)[0].contains("declined"));
        assert!(
            mails(&m.approve(n)).is_empty(),
            "a declined steer cannot be approved"
        );

        let (n, _) = m.steer("tok", "run the tests").expect("queued");
        let fx = m.approve(n);
        assert_eq!(
            mails(&fx),
            vec![&Effect::Mail {
                viewer: id,
                steer: n,
                from: "observer:Ms. Lee".into(),
                text: "run the tests".into(),
            }]
        );
        assert!(mails(&m.approve(n)).is_empty(), "one y sends once");

        assert_eq!(parse_cmd("y 2"), Some(Cmd::Approve(2)));
        assert_eq!(parse_cmd("n s2"), Some(Cmd::Decline(2)));
        assert_eq!(parse_cmd("a 1 2"), None);
        assert_eq!(parse_cmd("q"), Some(Cmd::Quit));
        assert_eq!(parse_cmd("x 1"), None);
    }

    #[tokio::test]
    async fn share_router_serves_only_its_own_routes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (tx, _) = broadcast::channel(4);
        let ctx = Ctx {
            hub: Arc::new(Mutex::new(Hub {
                machine: Viewers::new(3, true),
                conns: HashMap::new(),
            })),
            secret: Arc::from("s"),
            tx,
            snap: Arc::new(Mutex::new(Snap::default())),
            socket: Arc::new(PathBuf::from("/nonexistent.sock")),
            session: Arc::from("t"),
            pane: 3,
            page: Arc::from("page"),
            managed: false,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(ctx)).await.unwrap();
        });
        let ask = move |req: String| async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            reply
        };
        for path in [
            "/backlog",
            "/backlog/model.json",
            "/backlog/node.json",
            "/fleet",
            "/rundown",
            "/team",
        ] {
            let reply = ask(format!(
                "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            ))
            .await;
            assert!(reply.starts_with("HTTP/1.1 404"), "{path}: {reply}");
        }
        let reply = ask(
            "POST /backlog/act HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        for path in ["/coach", "/steer"] {
            let body = r#"{"text":"hi"}"#;
            let reply = ask(format!(
                "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
            .await;
            assert!(reply.starts_with("HTTP/1.1 401"), "{path}: {reply}");
        }
        let reply = ask("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".into()).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        server.abort();
    }
}
