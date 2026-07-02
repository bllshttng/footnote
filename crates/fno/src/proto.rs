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

/// Bumped on any wire-incompatible change. The server outlives `cargo install`
/// upgrades, so both sides exchange this at Attach and refuse loudly on skew.
/// There is no automated backstop tying this to the message shapes: bump it in
/// the SAME commit as any `ClientMsg`/`ServerMsg`/`Frame` shape change.
pub const PROTO_VERSION: u32 = 1;

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
    /// handshake; `rows`/`cols` are the client's terminal size.
    Attach {
        proto: u32,
        build: String,
        rows: u16,
        cols: u16,
    },
    /// Raw keystroke bytes for the pane's PTY. Never dropped.
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Clean detach: the client is leaving; the server keeps the PTY.
    Detach,
}

/// Server -> client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMsg {
    /// A self-contained render frame (full grid + cursor). Droppable: the
    /// server keeps only the newest unsent frame per client.
    Frame(Frame),
    /// Cursor-only movement (absolute, so it composes with any prior frame).
    Cursor { row: u16, col: u16, visible: bool },
    /// The server is refusing or ending this connection; `reason` is
    /// human-facing (version skew, shutdown, ...).
    Bye { reason: String },
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

/// Create the mux dir if needed and force 0700 either way: the sockets in it
/// accept keystrokes into your shell, so group/world access is never OK.
pub fn ensure_mux_dir() -> std::io::Result<PathBuf> {
    let dir = mux_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
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
        let msg = ServerMsg::Frame(test_frame());
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
            },
            ClientMsg::Input(b"echo hello\r".to_vec()),
            ClientMsg::Resize { rows: 50, cols: 90 },
            ClientMsg::Detach,
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ClientMsg = read_msg_sync(&mut cursor).unwrap();
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
    fn proto_session_name_cannot_escape_mux_dir() {
        assert!(socket_path("../evil").is_err());
        assert!(socket_path("").is_err());
        assert!(socket_path("ok-name_1").is_ok());
    }
}
