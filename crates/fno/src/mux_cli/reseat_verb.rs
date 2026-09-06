//! The `fno mux thread reseat` verb: move a live pane-hosted worker into a
//! portal seat, keeping its PTY. A child module of `mux_cli` on purpose: the
//! file-budget gate keeps the parent shrink-only, and `use super::*` keeps
//! every helper (take_common_flags, resolve_session, the timeout constants,
//! the exit codes, the proto types) in exactly one place.
//!
//! One process owns the whole move (operator ruling, 2026-09-06): this verb
//! resolves the registry row, the server moves the topology keeping the PTY,
//! and this verb clears the row's `mux` ref on the receipt. The former
//! Python front door (`fno agents reseat`) drove this verb over a subprocess
//! and classified its failures by matching stderr text; both halves now live
//! here with typed errors.
//!
//! The trade, named where the operator meets it: after the move the row is no
//! longer rebuilt by `mux workspace restore`, and `fno agents rm` removes the
//! row without killing the pane (thread semantics). The work survives; the
//! geometry does not.
use super::*;

const RESEAT_USAGE: &str = "usage: fno mux thread reseat <agent-name | pane-id> [--portal N] [--session S]\n  move a live pane-hosted worker into a portal seat, keeping its PTY.\n  trade: the row stops being a squad member, so restore never rebuilds it,\n  and `fno agents rm` removes the row without killing the pane child.";

/// A typed refusal from the reseat move. Printed once at the verb's edge,
/// each with its own exit code; nothing downstream matches message text.
#[derive(Debug)]
pub(crate) enum ReseatFail {
    /// No registry row answers the name or session id.
    UnknownRow(String),
    /// The row exists but carries no `mux` ref (already a thread row).
    NotPaneHosted(String),
    /// The registry file could not be read or parsed.
    RegistryUnreadable(String),
    /// No mux server is listening on the resolved session socket.
    MuxUnreachable(String),
    /// The server refused the move and said why.
    ServerRefused(String),
    /// The move landed but the registry flip did not. `landing` is the
    /// server's receipt, kept in the message so a re-run command is provable.
    RegistryFlipFailed { landing: String, cause: String },
}

impl ReseatFail {
    fn code(&self) -> i32 {
        match self {
            ReseatFail::UnknownRow(_) | ReseatFail::NotPaneHosted(_) => EXIT_NOT_FOUND,
            ReseatFail::MuxUnreachable(_) => EXIT_NO_SERVER,
            ReseatFail::RegistryUnreadable(_)
            | ReseatFail::ServerRefused(_)
            | ReseatFail::RegistryFlipFailed { .. } => EXIT_ERROR,
        }
    }

    fn detail(&self) -> String {
        match self {
            ReseatFail::UnknownRow(token) => {
                format!("unknown row: no registry row answers {token:?}")
            }
            ReseatFail::NotPaneHosted(name) => format!(
                "not_pane_hosted: {name:?} carries no mux ref; only a pane-hosted row can re-seat"
            ),
            ReseatFail::RegistryUnreadable(cause) => {
                format!("registry_unreadable: {cause}")
            }
            ReseatFail::MuxUnreachable(cause) => format!("mux_unreachable: {cause}"),
            ReseatFail::ServerRefused(msg) => format!("server_refused: {msg}"),
            ReseatFail::RegistryFlipFailed { landing, cause } => format!(
                "registry_flip_failed: the pane moved ({landing}) but the ref was not \
                 cleared ({cause}); re-run this verb, both halves are idempotent"
            ),
        }
    }
}

/// The row half the verb resolved from the registry: which server session
/// hosts the pane, which pane moves, and which row gets its `mux` ref
/// cleared on the receipt.
#[derive(Debug)]
pub(crate) struct ReseatRow {
    pub(crate) name: String,
    pub(crate) session: String,
    pub(crate) pane: u64,
}

/// Resolve a reseat target from raw registry JSON: the row whose `name` or
/// `harness_session_id` answers `token`, carrying its `mux` (session, pane)
/// pair. Pure so the resolution rules are unit-testable without a file.
pub(crate) fn resolve_reseat_target(token: &str, reg_raw: &str) -> Result<ReseatRow, ReseatFail> {
    let doc = serde_json::from_str::<serde_json::Value>(reg_raw)
        .map_err(|e| ReseatFail::RegistryUnreadable(e.to_string()))?;
    let rows = doc
        .get("agents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ReseatFail::RegistryUnreadable("no agents array".into()))?;
    for row in rows {
        let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = row
            .get("harness_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name != token && session_id != token {
            continue;
        }
        let mux = row.get("mux");
        let pair = mux
            .and_then(|m| {
                let session = m.get("session").and_then(|v| v.as_str())?;
                let pane = m.get("pane_id").and_then(|v| v.as_u64())?;
                Some((session.to_string(), pane))
            })
            .filter(|(s, _)| !s.is_empty());
        return match pair {
            Some((session, pane)) => Ok(ReseatRow {
                name: name.to_string(),
                session,
                pane,
            }),
            None => Err(ReseatFail::NotPaneHosted(name.to_string())),
        };
    }
    Err(ReseatFail::UnknownRow(token.to_string()))
}

/// Clear the `mux` ref of every row `matches` accepts, in place: the
/// registry-wide flock at `<registry-dir>/locks/_registry.lock`, then a
/// read, mutate, `<registry>.tmp` + rename - the Python writer's protocol
/// (fno.agents.registry write_registry), so the two writers interoperate.
/// The document round-trips as raw JSON: unknown fields and `schema_version`
/// pass through untouched, and a typed row can never drop a field a newer
/// writer added. Answers how many rows were cleared; zero is success (an
/// already-flipped row means a re-run after a half-completed move).
pub(crate) fn clear_mux_refs(
    registry: &std::path::Path,
    matches: &dyn Fn(&serde_json::Value) -> bool,
) -> Result<usize, String> {
    let lock_path = registry
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("locks")
        .join("_registry.lock");
    std::fs::create_dir_all(
        lock_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    )
    .map_err(|e| format!("lock dir: {e}"))?;
    // Truncate, like the Python opener ("w"): the lock file carries no data.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .map_err(|e| format!("lock open: {e}"))?;
    lock.lock().map_err(|e| format!("lock: {e}"))?;

    let result = (|| {
        let raw = std::fs::read_to_string(registry).map_err(|e| format!("read: {e}"))?;
        let mut doc: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("parse: {e}"))?;
        let mut cleared = 0;
        let rows = doc
            .get_mut("agents")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| "no agents array".to_string())?;
        for row in rows.iter_mut() {
            if matches(row) {
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("mux".to_string(), serde_json::Value::Null);
                    cleared += 1;
                }
            }
        }
        let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("encode: {e}"))?;
        let tmp = registry.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("tmp write: {e}"))?;
        std::fs::rename(&tmp, registry).map_err(|e| format!("rename: {e}"))?;
        Ok(cleared)
    })();

    let _ = std::fs::File::unlock(&lock);
    result
}

/// `fno mux thread reseat <agent-name | pane-id> [--portal N] [--session S]`
/// (v72): the whole re-seat move in one process. A name resolves the
/// registry row first (the row names the server session hosting the pane, so
/// `--session` is refused alongside a name); a bare pane id keeps the old
/// spelling and its explicit session resolution. Idempotent: an already
/// seated pane is answered, not moved twice, and an already-cleared ref is
/// left alone.
pub fn reseat(args: &[OsString], env_session: Option<&str>) -> i32 {
    let (session_flag, _json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux thread reseat: {e}");
            return EXIT_USAGE;
        }
    };
    let mut portal: Option<u8> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let text = arg.as_str();
        if text == "-h" || text == "--help" {
            println!("{RESEAT_USAGE}");
            return EXIT_OK;
        }
        if text == "--portal" {
            let value = match it.next() {
                Some(v) => v.as_str().to_string(),
                None => {
                    eprintln!("fno mux thread reseat: --portal needs a value");
                    return EXIT_USAGE;
                }
            };
            match value.parse::<u8>() {
                Ok(n) => portal = Some(n),
                Err(_) => {
                    eprintln!("fno mux thread reseat: --portal takes an index 0-255");
                    return EXIT_USAGE;
                }
            }
            continue;
        }
        if let Some(value) = text.strip_prefix("--portal=") {
            match value.parse::<u8>() {
                Ok(n) => portal = Some(n),
                Err(_) => {
                    eprintln!("fno mux thread reseat: --portal takes an index 0-255");
                    return EXIT_USAGE;
                }
            }
            continue;
        }
        if text.starts_with("--") {
            eprintln!("fno mux thread reseat: unknown flag: {text}");
            return EXIT_USAGE;
        }
        positionals.push(text.to_string());
    }
    if positionals.len() != 1 {
        eprintln!("fno mux thread reseat: takes exactly one agent name or pane id");
        eprintln!("{RESEAT_USAGE}");
        return EXIT_USAGE;
    }
    let token = positionals[0].clone();

    // A name answers its registry row; a number is a pane id in the explicit
    // session. The row carries the mux session, so the name spelling refuses
    // a --session flag rather than silently preferring one of the two.
    let row: Option<ReseatRow> = match token.parse::<u64>() {
        Ok(_) => None,
        Err(_) => {
            if session_flag.is_some() {
                eprintln!(
                    "fno mux thread reseat: --session names the server for a pane id; \
                     an agent name already carries its session"
                );
                return EXIT_USAGE;
            }
            let registry = crate::agents_view::registry_path();
            let raw = match std::fs::read_to_string(&registry) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!(
                        "fno mux thread reseat: {}",
                        ReseatFail::RegistryUnreadable(e.to_string()).detail()
                    );
                    return EXIT_NOT_FOUND;
                }
            };
            match resolve_reseat_target(&token, &raw) {
                Ok(r) => Some(r),
                Err(fail) => {
                    eprintln!("fno mux thread reseat: {}", fail.detail());
                    return fail.code();
                }
            }
        }
    };

    let (session, pane, name_for_flip) = match &row {
        Some(r) => (r.session.clone(), r.pane, Some(r.name.clone())),
        None => {
            let session = resolve_session(
                session_flag
                    .as_deref()
                    .or(env_session)
                    .filter(|s| !s.is_empty()),
                None,
            );
            let pane = match token.parse::<u64>() {
                Ok(n) => n,
                Err(_) => unreachable!("the pane spelling is numeric by construction"),
            };
            (session, pane, None)
        }
    };

    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux thread reseat: {e}");
            return EXIT_USAGE;
        }
    };
    let stream = match proto::connect_unix_timeout(&sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "fno mux thread reseat: {}",
                ReseatFail::MuxUnreachable(e.to_string()).detail()
            );
            return EXIT_NO_SERVER;
        }
    };
    let reply = send_control(
        stream,
        ControlVerb::ThreadReseat { pane, portal },
        CONTROL_TIMEOUT,
        CONTROL_REPLY_DEADLINE,
        &session,
    );
    let notice = match reply {
        Ok(ServerMsg::Notice { text }) => text,
        Ok(ServerMsg::Err { msg, .. }) => {
            eprintln!(
                "fno mux thread reseat: {}",
                ReseatFail::ServerRefused(msg).detail()
            );
            return EXIT_ERROR;
        }
        Ok(other) => {
            eprintln!("fno mux thread reseat: unexpected reply: {other:?}");
            return EXIT_ERROR;
        }
        Err(ControlError::Unanswered(e)) => {
            eprintln!("fno mux thread reseat: {e}");
            return EXIT_CONTROL_UNANSWERED;
        }
        Err(e) => {
            eprintln!("fno mux thread reseat: {e}");
            return EXIT_ERROR;
        }
    };

    // The move landed. Clear the row's ref: by name when a row was resolved,
    // else every row whose mux pair is this (session, pane).
    let registry = crate::agents_view::registry_path();
    let flip: Result<usize, String> = match &name_for_flip {
        Some(name) => clear_mux_refs(&registry, &|row| {
            row.get("name").and_then(|v| v.as_str()) == Some(name.as_str())
        }),
        None => clear_mux_refs(&registry, &|row| {
            let mux = row.get("mux");
            mux.and_then(|m| m.get("session").and_then(|v| v.as_str())) == Some(session.as_str())
                && mux.and_then(|m| m.get("pane_id").and_then(|v| v.as_u64())) == Some(pane)
        }),
    };
    if let Err(cause) = flip {
        eprintln!(
            "fno mux thread reseat: {}",
            ReseatFail::RegistryFlipFailed {
                landing: notice.clone(),
                cause,
            }
            .detail()
        );
        return EXIT_ERROR;
    }
    println!("{notice}");
    EXIT_OK
}
