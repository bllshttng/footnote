//! Wire protocol + socket lifecycle for the fno mux.
//!
//! Length-prefixed (u32 big-endian) JSON messages over a Unix socket at
//! `~/.fno/mux/<session>.sock`. The socket dir is 0700 - it accepts keystrokes
//! into your shell, so it is a security boundary. There is no lockfile: the
//! socket bind IS the lock, liveness is a connect-probe, and a stale socket is
//! unlinked at bind time.
//!
//! Channel discipline (epic Locked Decision): client->server input/control is
//! reliable and never dropped; only server->client render frames are
//! droppable, and a droppable frame is always SELF-CONTAINED (a `Frame`
//! carries the full grid + cursor, never a delta over a possibly-dropped
//! predecessor).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::tree::{Dir, Rect};

/// Bumped on any wire-incompatible change. The server outlives `cargo install`
/// upgrades, so both sides exchange this at Attach and refuse loudly on skew.
/// There is no automated backstop tying this to the message shapes: bump it in
/// the SAME commit as any `ClientMsg`/`ServerMsg`/`Frame` shape change.
///
/// v2 (Phase 2 layout): `Attach` gains `cwd`; `Frame`s are pane-tagged;
/// `Command`/`Layout`/`ModeSync` added; the never-sent `ServerMsg::Cursor`
/// variant is removed (the cursor rides INSIDE `Frame`).
///
/// v3 (Phase 3 multi-client/sessions): `TabMeta` gains a stable `id`;
/// `Command::SelectTab` selects by that id (u64), not by index; `Layout`
/// gains `area` (the clamped content-area its rects were computed for);
/// pre-Attach `ClientMsg::{Query, KillServer}` + `ServerMsg::Info` added
/// (wire-shape FROZEN - see the variants).
pub const PROTO_VERSION: u32 = 3;

/// The crate version, carried in the handshake purely for the error message.
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Refuse frames larger than this. A full 500x500 styled grid serializes to a
/// few MB of JSON; 32MB is far above any real frame, low enough that a
/// corrupt length prefix cannot OOM the reader.
pub const MAX_MSG_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("message of {0} bytes exceeds the {MAX_MSG_BYTES}-byte cap")]
    TooLarge(u32),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("peer closed the connection")]
    Closed,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Client -> server. Everything here rides the reliable channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMsg {
    /// First message on a fresh connection. `proto`/`build` drive the version
    /// handshake; `rows`/`cols` are the client's CONTENT-AREA viewport (its
    /// terminal minus client-local chrome: sideline panel, tab bar). `cwd` is
    /// the directory the client was launched from - the server resolves it to
    /// a canonical repo root to select or create the squad (squad.rs).
    Attach {
        proto: u32,
        build: String,
        rows: u16,
        cols: u16,
        cwd: String,
    },
    /// Raw keystroke bytes for the focused pane's PTY. Never dropped.
    Input(Vec<u8>),
    /// The client's CONTENT-AREA viewport changed.
    Resize { rows: u16, cols: u16 },
    /// Clean detach: the client is leaving; the server keeps the PTYs.
    Detach,
    /// A layout/tab/squad command from the client's leader-key layer
    /// (keys.rs). Reliable; a refused command comes back as a one-line
    /// notice, never a dropped connection.
    Command(Command),
    /// Sent INSTEAD of `Attach` as the first message on a fresh connection:
    /// ask who this server is (`fno mux ls`). The server answers with one
    /// [`ServerMsg::Info`] and closes; no client is registered.
    ///
    /// Wire shape FROZEN forever: pre-Attach messages bypass the version
    /// handshake (Invariants, Phase 3 plan), so every past and future build
    /// must parse this identically. Changing it means a NEW variant.
    Query,
    /// Sent INSTEAD of `Attach` as the first message on a fresh connection:
    /// shut the session down (`fno mux kill-server`). The server Byes every
    /// client, kills every pane child, and exits 0.
    ///
    /// Wire shape FROZEN forever: pre-Attach, bypasses the version handshake
    /// (Invariants, Phase 3 plan). Changing it means a NEW variant.
    KillServer,
}

/// Layout mutations the client can request. Interpreted (leader-key table)
/// client-side; executed on the server's core loop, which owns the tree.
/// `SelectTab` names a stable [`TabMeta::id`] from the last `Layout`'s
/// catalog; `SelectSquad` names a squad id from the same catalog - the
/// server rejects stale values fail-closed (BEL + notice), so a client racing
/// a layout change can never corrupt state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Command {
    SplitH,
    SplitV,
    ClosePane,
    FocusDir(Dir),
    ResizeDir(Dir),
    NewTab,
    SelectTab(u64),
    NextTab,
    PrevTab,
    CloseTab,
    SelectSquad(u64),
}

/// Server -> client.
///
/// Channel discipline (Locked Decision 4): `Layout`/`ModeSync`/`Bye` ride the
/// per-client RELIABLE channel (awaited, never dropped - a dropped Layout is
/// a protocol bug, not a degraded mode); only pane-tagged self-contained
/// `Frame`s are droppable (per-(client, pane) newest-wins). v1's `Cursor`
/// variant is gone: it was never sent (the cursor rides inside `Frame`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMsg {
    /// A self-contained render frame (full grid + cursor) for ONE pane.
    /// Droppable: the server keeps only the newest unsent frame per
    /// (client, pane), so a flooded pane coalesces without starving its
    /// siblings. `pane_id` lives on the variant, not in [`Frame`]: the VT
    /// grid (`vt::Pane`) does not know its mux pane id - the server's pane
    /// registry tags the frame at send time.
    Frame { pane_id: u64, frame: Frame },
    /// The squad/tab catalog + computed rects for the receiving client's
    /// viewed tab, relative to the CONTENT AREA. The server sends rects,
    /// never the tree; the client never runs the layout algorithm. Reliable.
    /// `area` is the clamped (rows, cols) the rects were computed for
    /// (view-scoped smallest-client clamp); a client larger than `area`
    /// letterboxes client-side without inferring the bound from the rects.
    Layout {
        squads: Vec<SquadMeta>,
        active_squad: u64,
        panes: Vec<(u64, Rect)>,
        focus: u64,
        area: (u16, u16),
    },
    /// Escape bytes syncing the client terminal to the newly focused pane's
    /// negotiated modes (bracketed paste, mouse reporting, DECCKM, ...).
    /// Applied verbatim to the client TTY. Reliable, and ordered BEFORE the
    /// `Layout`/frames that assume those modes.
    ModeSync { bytes: Vec<u8> },
    /// A one-line human-facing notice (refused command, failed split, ...)
    /// the client renders as transient feedback + BEL. Reliable.
    Notice { text: String },
    /// The server is refusing or ending this connection; `reason` is
    /// human-facing (version skew, shutdown, session ended, ...).
    Bye { reason: String },
    /// The answer to a pre-Attach [`ClientMsg::Query`] (`fno mux ls`).
    ///
    /// Wire shape FROZEN forever: pre-Attach traffic bypasses the version
    /// handshake (Invariants, Phase 3 plan). Changing it means a NEW variant.
    Info {
        session: String,
        clients: u32,
        squads: u32,
        panes: u32,
    },
}

/// One tab's catalog entry inside [`ServerMsg::Layout`]. `id` is the stable
/// session-scoped tab identity (monotonic u64, never reused - Locked
/// Decision 6 extended to tabs); `Command::SelectTab` names it, so a
/// selection can never race a catalog change onto the wrong tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabMeta {
    pub id: u64,
    pub name: String,
}

/// One squad's catalog entry inside [`ServerMsg::Layout`]. Identity is the
/// server-scoped `id` (monotonic, never reused); `canonical_cwd` is the
/// resolved repo root the squad is keyed by; `name` is display-only (the
/// root's basename, disambiguated by a parent segment when needed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadMeta {
    pub id: u64,
    pub name: String,
    pub canonical_cwd: String,
    pub tabs: Vec<TabMeta>,
    pub active_tab: usize,
}

/// A complete rendered screen: `rows * cols` cells in row-major order plus the
/// cursor. Self-contained by construction - drawing a `Frame` never requires
/// any earlier message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Frame {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Cell>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
}

impl Frame {
    /// The load-bearing invariant: exactly `rows * cols` cells. Serde cannot
    /// enforce it (a short `cells` deserializes cleanly), so the compositor
    /// checks this at the trust boundary before doing any slice math - a
    /// mismatched frame is treated like a malformed message, never drawn.
    pub fn geometry_ok(&self) -> bool {
        self.cells.len() == self.rows as usize * self.cols as usize
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        }
    }
}

/// Style bits carried per cell (`Cell::flags`).
pub mod cell_flags {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const INVERSE: u8 = 1 << 3;
    pub const DIM: u8 = 1 << 4;
    /// The second cell of a wide (CJK/emoji) glyph. Compositors skip it so
    /// the glyph's right half is never overdrawn.
    pub const WIDE_SPACER: u8 = 1 << 5;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The version-handshake decision, factored pure so it is unit-testable. On a
/// mismatch the message names BOTH versions and how to recover, because the
/// operator seeing it is mid-upgrade and the server is the stale side.
pub fn check_attach_version(client_proto: u32, client_build: &str) -> Result<(), String> {
    if client_proto == PROTO_VERSION {
        return Ok(());
    }
    Err(format!(
        "protocol version mismatch: client {client_build} speaks v{client_proto}, \
         server {BUILD_VERSION} speaks v{PROTO_VERSION}. The running server predates \
         your fno upgrade - stop it (it keeps running across upgrades by design) \
         and re-run fno to start a fresh one."
    ))
}

// ---------------------------------------------------------------------------
// Codec: u32-BE length prefix + JSON body
// ---------------------------------------------------------------------------

/// Encode one message with its length prefix.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len()).map_err(|_| ProtoError::TooLarge(u32::MAX))?;
    if len > MAX_MSG_BYTES {
        return Err(ProtoError::TooLarge(len));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Decode a length-checked body. `Err(Malformed)` on any parse failure - the
/// caller must close the connection loudly, never act on a half-frame.
fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    Ok(serde_json::from_slice(body)?)
}

fn check_len(len: u32) -> Result<usize, ProtoError> {
    if len > MAX_MSG_BYTES {
        return Err(ProtoError::TooLarge(len));
    }
    Ok(len as usize)
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), ProtoError>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = encode(msg)?;
    w.write_all(&buf).await?;
    Ok(())
}

pub async fn read_msg<R, T>(r: &mut R) -> Result<T, ProtoError>
where
    R: tokio::io::AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ProtoError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = check_len(u32::from_be_bytes(len_buf))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    decode_body(&body)
}

/// Sync twin of [`write_msg`] for plain `std` streams (tests, simple tools).
pub fn write_msg_sync<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), ProtoError> {
    let buf = encode(msg)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Sync twin of [`read_msg`].
pub fn read_msg_sync<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, ProtoError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ProtoError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = check_len(u32::from_be_bytes(len_buf))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    decode_body(&body)
}

// ---------------------------------------------------------------------------
// Socket lifecycle
// ---------------------------------------------------------------------------

/// The mux socket directory: `$FNO_MUX_DIR` when set (tests point this at a
/// tempdir), else `~/.fno/mux`.
pub fn mux_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FNO_MUX_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".fno").join("mux")
}

/// Create `dir` (and parents) born 0700, then force 0700 on a pre-existing
/// one. `DirBuilder::mode` makes fresh directories private atomically -
/// `create_dir_all` + `set_permissions` leaves a window where the dir exists
/// with umask-loosened permissions (gemini security-medium). The follow-up
/// `set_permissions` only tightens a dir that already existed.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Create the mux dir if needed and force 0700 either way: the sockets in it
/// accept keystrokes into your shell, so group/world access is never OK.
pub fn ensure_mux_dir() -> std::io::Result<PathBuf> {
    let dir = mux_dir();
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Socket path for a session name. Path separators are rejected rather than
/// sanitized so a session name can never escape the 0700 mux dir.
pub fn socket_path(session: &str) -> Result<PathBuf, String> {
    if session.is_empty() || session.contains('/') || session.contains('\0') {
        return Err(format!("invalid session name: {session:?}"));
    }
    Ok(mux_dir().join(format!("{session}.sock")))
}

pub const DEFAULT_SESSION: &str = "main";

/// Outcome of [`bind_or_probe`].
pub enum BindOutcome {
    /// We own the socket: this process should run the server.
    Bound(UnixListener),
    /// A live server already owns it: attach instead.
    AlreadyRunning,
}

/// Bind the session socket, treating the bind itself as the lock.
///
/// - Fresh path: bind wins atomically.
/// - `AddrInUse`: connect-probe. A successful connect means a live server
///   (`AlreadyRunning`). Refused/failed connects (retried briefly, so a server
///   between its bind and listen syscalls is not misread as dead) mean a stale
///   socket from a dead server: unlink it and bind again.
///
/// ponytail: unlink-then-rebind has a tiny two-racers-over-a-stale-socket
/// window (both probe dead, both unlink+bind; the second unlink can orphan the
/// first winner's socket). The plan locks "no lockfile - bind is the lock";
/// the cold-start race (AC4-EDGE, no stale socket) is fully atomic, and the
/// stale+simultaneous case needs a dead server AND a photo-finish start. If it
/// ever bites, the upgrade is an O_EXCL sidecar lock around the unlink.
pub fn bind_or_probe(path: &Path) -> std::io::Result<BindOutcome> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(BindOutcome::Bound(l)),
        Err(e) if socket_in_use(&e) => {
            if probe_alive(path) {
                return Ok(BindOutcome::AlreadyRunning);
            }
            // Stale socket from a dead server: take the name over.
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            match UnixListener::bind(path) {
                Ok(l) => Ok(BindOutcome::Bound(l)),
                Err(e) if socket_in_use(&e) => {
                    // Someone else won the rebind race; they are the server.
                    Ok(BindOutcome::AlreadyRunning)
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Bind failed because the path is taken. Linux reports `EADDRINUSE`
/// (`AddrInUse`); macOS reports `EEXIST` (`AlreadyExists`) when the socket
/// file already exists. Both mean the same thing here.
fn socket_in_use(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
    )
}

/// True if something accepts connections at `path`. Retries a few times so a
/// server that has bound but not yet reached `listen` is not declared dead.
fn probe_alive(path: &Path) -> bool {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if UnixStream::connect(path).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame() -> Frame {
        let mut cells = vec![Cell::default(); 2 * 3];
        cells[0] = Cell {
            c: 'h',
            fg: Color::Indexed(2),
            bg: Color::Rgb(10, 20, 30),
            flags: cell_flags::BOLD | cell_flags::UNDERLINE,
        };
        Frame {
            rows: 2,
            cols: 3,
            cells,
            cursor_row: 1,
            cursor_col: 2,
            cursor_visible: true,
        }
    }

    #[test]
    fn proto_frame_roundtrips_through_codec() {
        let msg = ServerMsg::Frame {
            pane_id: 7,
            frame: test_frame(),
        };
        let bytes = encode(&msg).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let decoded: ServerMsg = read_msg_sync(&mut cursor).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn proto_client_msgs_roundtrip() {
        for msg in [
            ClientMsg::Attach {
                proto: PROTO_VERSION,
                build: BUILD_VERSION.into(),
                rows: 40,
                cols: 120,
                cwd: "/home/user/code/footnote".into(),
            },
            ClientMsg::Input(b"echo hello\r".to_vec()),
            ClientMsg::Resize { rows: 50, cols: 90 },
            ClientMsg::Detach,
            ClientMsg::Command(Command::SplitH),
            ClientMsg::Command(Command::FocusDir(Dir::Left)),
            ClientMsg::Command(Command::ResizeDir(Dir::Down)),
            ClientMsg::Command(Command::SelectTab(3)),
            ClientMsg::Command(Command::SelectSquad(42)),
            ClientMsg::Query,
            ClientMsg::KillServer,
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ClientMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn proto_v3_server_msgs_roundtrip() {
        // Every new/changed v3 server message survives the codec (mirrors the
        // Phase 1/2 roundtrip discipline): Layout carries `area`, TabMeta a
        // stable `id`, and the pre-Attach `Info` answer parses back exactly.
        for msg in [
            ServerMsg::Layout {
                squads: vec![SquadMeta {
                    id: 1,
                    name: "footnote".into(),
                    canonical_cwd: "/code/footnote/footnote".into(),
                    tabs: vec![
                        TabMeta {
                            id: 7,
                            name: "1".into(),
                        },
                        TabMeta {
                            id: 12,
                            name: "2".into(),
                        },
                    ],
                    active_tab: 1,
                }],
                active_squad: 1,
                panes: vec![
                    (
                        4,
                        Rect {
                            x: 0,
                            y: 0,
                            rows: 24,
                            cols: 40,
                        },
                    ),
                    (
                        9,
                        Rect {
                            x: 41,
                            y: 0,
                            rows: 24,
                            cols: 39,
                        },
                    ),
                ],
                focus: 9,
                area: (24, 80),
            },
            ServerMsg::ModeSync {
                bytes: b"\x1b[?2004h\x1b[?1000l".to_vec(),
            },
            ServerMsg::Notice {
                text: "split refused: pane too small".into(),
            },
            ServerMsg::Bye {
                reason: "session ended".into(),
            },
            ServerMsg::Info {
                session: "work".into(),
                clients: 2,
                squads: 1,
                panes: 3,
            },
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ServerMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn proto_reader_rejects_oversized_length_prefix() {
        let mut bytes = (MAX_MSG_BYTES + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"junk");
        let mut cursor = std::io::Cursor::new(bytes);
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::TooLarge(_))), "{res:?}");
    }

    #[test]
    fn proto_reader_surfaces_malformed_body_as_error() {
        // Valid length prefix, garbage body: must error, never yield a value.
        let body = b"not json at all";
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(bytes);
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::Malformed(_))), "{res:?}");
    }

    #[test]
    fn proto_clean_eof_reads_as_closed() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::Closed)), "{res:?}");
    }

    #[test]
    fn proto_version_match_is_accepted() {
        assert!(check_attach_version(PROTO_VERSION, BUILD_VERSION).is_ok());
    }

    #[test]
    fn proto_version_mismatch_names_both_versions() {
        let err = check_attach_version(PROTO_VERSION + 1, "9.9.9").unwrap_err();
        assert!(err.contains("9.9.9"), "{err}");
        assert!(
            err.contains(&format!("v{}", PROTO_VERSION + 1)),
            "client proto version missing: {err}"
        );
        assert!(
            err.contains(&format!("v{PROTO_VERSION}")),
            "server proto version missing: {err}"
        );
        assert!(err.contains(BUILD_VERSION), "server build missing: {err}");
    }

    #[test]
    fn proto_frame_geometry_check_catches_cell_count_mismatch() {
        let mut f = test_frame();
        assert!(f.geometry_ok());
        f.cells.pop();
        assert!(!f.geometry_ok(), "short cells vec must fail the check");
        f.cells.clear();
        assert!(!f.geometry_ok());
    }

    #[test]
    fn proto_pre_attach_wire_shapes_are_frozen() {
        // Query/KillServer/Info bypass the version handshake, so their JSON
        // encodings are FROZEN forever (Invariants). This pins the exact
        // bytes: if this test breaks, you changed a frozen shape - add a new
        // variant instead.
        assert_eq!(
            serde_json::to_string(&ClientMsg::Query).unwrap(),
            r#""Query""#
        );
        assert_eq!(
            serde_json::to_string(&ClientMsg::KillServer).unwrap(),
            r#""KillServer""#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::Info {
                session: "s".into(),
                clients: 1,
                squads: 2,
                panes: 3,
            })
            .unwrap(),
            r#"{"Info":{"session":"s","clients":1,"squads":2,"panes":3}}"#
        );
    }

    #[test]
    fn proto_session_name_cannot_escape_mux_dir() {
        assert!(socket_path("../evil").is_err());
        assert!(socket_path("").is_err());
        assert!(socket_path("ok-name_1").is_ok());
    }
}
