//! `mail-remote`: the cross-host relay command.
//!
//! The front door claims an `@host` target on `agents mail send` and
//! `agents peek` and execs this command on the sibling `fno-agents` binary. The
//! store path writes one bus row (see [`crate::mail_bus`]); each host's
//! daemon pull arm hands the payload to the receiver's own local send, so
//! lane selection, holds, and receipts stay the local command's. When no
//! `store.remote_url` is configured, send and peek relay over ssh: the
//! remote machine's own `fno` answers, and its exit code and receipt pass
//! through byte-verbatim.

use crate::mail_bus;
use crate::store_remote::{self, Remote};
use std::ffi::OsString;
use std::process::Command;

/// The state-root files this module owns: the sender's local queue when the
/// primary is unreachable, and the receiver's injected-id ledger.
pub fn outbox_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("mail_outbox.jsonl")
}

pub fn seen_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("mail_seen.jsonl")
}

/// The pull arm's retry ladder memory, beside the seen ledger: one JSON
/// line per still-backing-off msg_id carrying its attempt count and next
/// due time. Persisted so a daemon restart never resets a ladder.
pub fn retry_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("mail_retry.jsonl")
}

/// What `split_target` refused.
pub fn target_error(target: &str) -> String {
    format!(
        "cross-host target {target:?}: the form is <name>@<host>, one @, \
both sides nonempty; a host alias in ssh_config covers user@host spellings"
    )
}

/// Split `<name>@<host>` at its single `@`. Both sides nonempty; two `@`
/// refuse (an ssh user@host spelling is not carried; an ssh_config Host
/// alias covers those).
pub fn split_target(target: &str) -> Result<(String, String), String> {
    let at_count = target.matches('@').count();
    if at_count != 1 {
        return Err(target_error(target));
    }
    let (name, host) = target.split_once('@').unwrap_or(("", ""));
    if name.is_empty() || host.is_empty() {
        return Err(target_error(target));
    }
    Ok((name.to_string(), host.to_string()))
}

/// Whether a send target without `@` is a plain name this command may resolve:
/// letters/digits/dots/dashes/underscores, 2+ chars. Session shapes stay out
/// (an 8-hex short id, a 32+-char hex or dashed uuid is a full id, a path
/// separator is a typo), so the resolution below never rewrites an
/// address a human meant literally.
pub fn is_plain_name_target(token: &str) -> bool {
    if token.len() < 2
        || token.starts_with('-')
        || token.contains('/')
        || token.contains(':')
        || token
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
    {
        return false;
    }
    // A dashed uuid strips to one hex run, so the hex test reads the
    // dash-stripped token; the length bounds catch the bare 8-hex short id
    // and the 32-hex full id, dashed or not.
    let stripped: String = token.chars().filter(|c| *c != '-').collect();
    let hexish = stripped.chars().all(|c| c.is_ascii_hexdigit());
    if hexish && (stripped.len() == 8 || stripped.len() >= 32) {
        return false;
    }
    true
}

/// Whether a LIVE registry row names `token` on this machine. The conservative
/// pre-filter for bare-name sends: a hit forwards byte-verbatim to the local
/// command, whose resolution ladder stays the authority.
fn local_live_row_names(token: &str) -> bool {
    let path = crate::paths::AgentsHome::from_env().registry_json();
    let Ok(Some(registry)) = crate::state::try_load_registry(&path) else {
        return false;
    };
    registry
        .entries
        .iter()
        .any(|row| row.name.eq_ignore_ascii_case(token) && !crate::lead_state::is_terminal(row))
}

/// The bare-name resolution: a clean local miss consults the shared store's
/// name-to-host binding (the bus itself, `latest_host_for_name`), and a hit
/// re-enters this command with the `@host` target spelled. No store, an
/// unreachable store, or no binding forwards byte-verbatim, so the local
/// command's refusal (exit 16) stays the only voice for a name it knows.
fn cmd_send_bare(target: &str, rest: &[String]) -> i32 {
    if local_live_row_names(target) {
        return exec_forward(&local_send_pass_argv(target, rest), "fno agents mail", true);
    }
    let bound = match store_path() {
        StorePath::Primary(remote) => {
            match crate::mail_bus::latest_host_for_name(&remote, target) {
                Ok(host) => host,
                Err(_) => None,
            }
        }
        _ => None,
    };
    let Some(host) = bound else {
        return exec_forward(&local_send_pass_argv(target, rest), "fno agents mail", true);
    };
    let rewritten = format!("{target}@{host}");
    let mut reentered = vec![rewritten];
    reentered.extend(rest.iter().cloned());
    cmd_send(&reentered)
}

/// The byte-verbatim local forward for a bare-name target: the same argv the
/// caller typed, exec'd through the `fno` front door so the local command's
/// ladder, holds, and refusal voice stay the only path.
fn local_send_pass_argv(target: &str, rest: &[String]) -> Vec<OsString> {
    let mut argv = vec![fno_binary()];
    argv.push(OsString::from("agents"));
    argv.push(OsString::from("mail"));
    argv.push(OsString::from("send"));
    argv.push(OsString::from(target));
    for word in rest {
        argv.push(OsString::from(word));
    }
    argv
}

/// The ssh fallback's remote argv, with the sender resolved and stamped on
/// the SENDING machine in the full `<user>:<host>/<name>` form the bus lane
/// passes (`local_send_argv`, consumed at mail_arm.rs:457), so the remote
/// registry read degrades nothing: the @host part carries provenance. Pure,
/// so the both-lanes-same-form contract is testable.
fn relay_words_with_sender(
    name: &str,
    rest: &[String],
    from_name: &FromName,
    user: &str,
    host: &str,
) -> Vec<String> {
    let name_part = match from_name {
        FromName::Given(v) | FromName::SelfName(v) => v.clone(),
        FromName::OsUser(v) => v.clone(),
    };
    let mut words = vec![
        "agents".to_string(),
        "mail".to_string(),
        "send".to_string(),
        name.to_string(),
        "--from-name".to_string(),
        sender_id(user, host, &name_part),
    ];
    words.extend(rest.iter().cloned());
    words
}

/// Quote one word for the remote sh that parses the line ssh hands it: ssh
/// joins argv with spaces before the remote shell reads it, so every word
/// with anything but plain characters rides inside single quotes. Message
/// text is never interpolated into a command string - it rides as a quoted
/// positional argument.
pub fn sh_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The ssh relay command: the remote login shell resolves `fno` via its
/// profile (the cargo bin dir), then execs the command with the words as
/// positional arguments. `$0` is `sh`, so `"$@"` is the command tail. Every
/// element rides sh-quoted, including the script itself: ssh joins argv
/// with spaces and the remote shell parses the joined line.
pub fn ssh_argv(host: &str, words: &[String]) -> Vec<String> {
    let mut argv = vec![
        "ssh".to_string(),
        host.to_string(),
        "--".to_string(),
        "sh".to_string(),
        "-lc".to_string(),
        sh_quote("exec fno \"$@\""),
        "sh".to_string(),
    ];
    argv.extend(words.iter().map(|w| sh_quote(w)));
    argv
}

/// Which identity a send stamps as `from_id`'s name part.
#[derive(Debug, PartialEq, Eq)]
pub enum FromName {
    /// `--from-name <value>` won.
    Given(String),
    /// The ambient session's own name, from `--from-self` or the ambient
    /// value, or the session's registry row: the replier's role name.
    SelfName(String),
    /// No identity above the OS user resolved: the plain-shell form.
    OsUser(String),
}

/// Read the passthrough flags a send carries. The tail rides the relay (or
/// the bus row) verbatim; only identity and raw-ness are lifted out.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct SendTail {
    pub from_name: Option<String>,
    pub from_self: bool,
    pub raw: bool,
    pub rest: Vec<String>,
}

pub fn parse_send_tail(args: &[String]) -> Result<SendTail, String> {
    let mut tail = SendTail::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from-name" => {
                i += 1;
                match args.get(i) {
                    Some(v) => tail.from_name = Some(v.clone()),
                    None => return Err("--from-name needs a value".to_string()),
                }
            }
            "--from-self" => tail.from_self = true,
            "--raw" => tail.raw = true,
            other => tail.rest.push(other.to_string()),
        }
        i += 1;
    }
    Ok(tail)
}

/// Resolve the from_id name part from the proven own session id, so the
/// rule is testable without env mutation. It is the local command's rule
/// (`stamp_from`): `--from-self` is fail-loud and takes the canonical
/// handle of the proven session. `--from-name` wins verbatim, except an
/// 8-hex session handle that is not this session's own. Then the proven
/// own handle, then the OS user.
pub fn resolve_from_name_with(
    own_session: Option<&str>,
    tail: &SendTail,
) -> Result<FromName, String> {
    let own = own_session
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(crate::identity::canonical_handle);
    if tail.from_self {
        return match own {
            Some(handle) => Ok(FromName::SelfName(handle)),
            None => Err(
                "--from-self: no ambient harness identity - cannot self-stamp \
(exit 2, the same floor the local command applies)"
                    .to_string(),
            ),
        };
    }
    if let Some(given) = &tail.from_name {
        let is_handle = given.len() == 8 && given.chars().all(|c| c.is_ascii_hexdigit());
        if let Some(own) = &own {
            if is_handle && !given.eq_ignore_ascii_case(own) {
                return Err(format!(
                    "--from-name {given} names another session; this session is {own}. \
Omit --from-name to self-stamp."
                ));
            }
        }
        return Ok(FromName::Given(given.clone()));
    }
    if let Some(handle) = own {
        return Ok(FromName::SelfName(handle));
    }
    Ok(FromName::OsUser(os_user()))
}

/// The CLI form: reads the proven own session id from the same resolver
/// the local command stamps from.
pub fn resolve_from_name(tail: &SendTail) -> Result<FromName, String> {
    let own = crate::mail_cli::send::own_identity().map(|(sid, _harness)| sid);
    resolve_from_name_with(own.as_deref(), tail)
}

fn os_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// `<user>:<host>/<name>`: the sender identity stamped into `from_id`
/// (informational provenance; a reply addresses `<name>@<host>`). The
/// colon keeps the label to no `@`, so it never reads as a send target.
pub fn sender_id(user: &str, host: &str, name: &str) -> String {
    format!("{user}:{}/{}", mail_bus::normalize_host(host), name)
}

/// Resolve the `fno` front door to exec on this machine. The daemon's pull
/// arm shares the scrape helper (FNO_BIN, sibling of this binary, then PATH).
pub fn fno_binary() -> OsString {
    crate::scrape::fno_bin()
}

/// The local send the same-host rewrite and the daemon pull arm both run:
/// the receiver's own command answers, so lane selection, holds and receipts
/// stay the local path's. `from` is the bus row's sender identity, stamped
/// with `--from-name` so the envelope carries the real sender, not the
/// delivering machine.
pub fn local_send_argv(name: &str, payload: &str, raw: bool, from: &str) -> Vec<OsString> {
    let mut argv = vec![fno_binary()];
    argv.push(OsString::from("agents"));
    argv.push(OsString::from("mail"));
    argv.push(OsString::from("send"));
    argv.push(OsString::from(name));
    if raw {
        argv.push(OsString::from("--raw"));
    }
    argv.push(OsString::from("--from-name"));
    argv.push(OsString::from(from));
    argv.push(OsString::from(payload));
    argv
}

enum StorePath {
    /// The primary this machine dials out to.
    Primary(Remote),
    /// `store.remote_url` unset: relay over ssh.
    None,
    /// A malformed value: refuse, never fall back in silence.
    Malformed(String),
}

fn store_path() -> StorePath {
    match store_remote::configured() {
        Ok(Some(remote)) => StorePath::Primary(remote),
        Ok(None) => StorePath::None,
        Err(e) => StorePath::Malformed(e),
    }
}

/// The ssh fallback's refusal of `--from-self`: that flag resolves identity
/// on whatever machine runs the command, and the fallback runs it remotely.
pub fn fallback_from_self_refusal() -> String {
    "--from-self over the ssh fallback would stamp the remote machine's \
identity; pass --from-name instead"
        .to_string()
}

fn cmd_send(args: &[String]) -> i32 {
    let words: Vec<String> = args.to_vec();
    let Some(target) = words.first() else {
        eprintln!("usage: fno agents mail send <name>@<host> [args...] <message>");
        return 2;
    };
    if !target.contains('@') && is_plain_name_target(target) {
        return cmd_send_bare(target, &words[1..]);
    }
    let Ok((name, host)) = split_target(target) else {
        eprintln!("{}", target_error(target));
        return 2;
    };
    let tail = match parse_send_tail(&words[1..]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let local_host = mail_bus::normalize_host(&crate::claims::hostname());
    if mail_bus::normalize_host(&host) == local_host {
        // A same-host target needs no bus row and no ssh hop: forward the
        // stripped argv to the local command, every flag the caller passed
        // intact.
        let mut argv = vec![fno_binary()];
        argv.push(OsString::from("agents"));
        argv.push(OsString::from("mail"));
        argv.push(OsString::from("send"));
        argv.push(OsString::from(name));
        for word in &words[1..] {
            argv.push(OsString::from(word));
        }
        return exec_forward(&argv, "fno agents mail", true);
    }

    match store_path() {
        StorePath::Malformed(e) => {
            eprintln!("fno agents mail send: {e}");
            2
        }
        StorePath::None => {
            if tail.from_self {
                eprintln!("{}", fallback_from_self_refusal());
                return 2;
            }
            // Message shape: the caller passes the message as the last
            // positional arg (the local command's shape), and it rides the relay
            // verbatim. The sender is resolved HERE, on the sending machine,
            // and stamped in the full `<user>:<host>/<name>` form the bus
            // lane passes (local_send_argv); the remote registry read then
            // degrades nothing - the @host part carries provenance.
            let from_name = match resolve_from_name(&tail) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            let relay_words =
                relay_words_with_sender(&name, &tail.rest, &from_name, &os_user(), &local_host);
            // The original message arg: everything after the target that is
            // not a flag was kept in rest in order, so the message is there.
            relay(&host, &relay_words)
        }
        StorePath::Primary(remote) => {
            let from_name = match resolve_from_name(&tail) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            let name_part = match &from_name {
                FromName::Given(v) | FromName::SelfName(v) => v.clone(),
                FromName::OsUser(v) => v.clone(),
            };
            let payload = match payload_of(&tail) {
                Some(p) if !p.is_empty() => p,
                _ => {
                    eprintln!(
                        "fno agents mail send: a cross-host store send carries \
the message directly after the target; none was found"
                    );
                    return 2;
                }
            };
            if tail.rest.len() > 1 {
                // A store row carries the payload, the raw bit, and the
                // sender identity - nothing else. Refuse rather than drop:
                // the other flags (--subject, --harness, --origin) belong to
                // the local and fallback lanes.
                eprintln!(
                    "fno agents mail send: a cross-host store send carries only \
--from-name/--from-self/--raw and the message; found extra words after it: {:?}",
                    &tail.rest[1..]
                );
                return 2;
            }
            let from_id = sender_id(&os_user(), &local_host, &name_part);
            // Sender-side style check: the receiver's door refuses an
            // unvetted body after transit, where only the sender's `mail
            // status` shows the loss. A refused body exits 1 having
            // written no row; a door that cannot run reads clean.
            if let Some(report) = crate::mail_arm::style_door_report(&payload) {
                eprint!("{report}");
                eprintln!(
                    "fno agents mail send: the receiver's style door refuses this \
body; nothing was sent"
                );
                return 1;
            }
            send_via_store(
                &remote,
                &from_id,
                &local_host,
                &name,
                &host,
                &payload,
                tail.raw,
                None,
            )
        }
    }
}

/// The message body: the first word after the target (the local command's
/// shape is `mail send <target> <message> [flags...]`), so a flag value can
/// never be mistaken for the payload. `None` when that word is missing or
/// is itself a flag; the caller refuses naming the shape.
fn payload_of(tail: &SendTail) -> Option<String> {
    match tail.rest.first() {
        Some(word) if !word.starts_with('-') => Some(word.clone()),
        _ => None,
    }
}

fn send_via_store(
    remote: &Remote,
    from_id: &str,
    from_host: &str,
    name: &str,
    host: &str,
    payload: &str,
    raw: bool,
    subject: Option<&str>,
) -> i32 {
    if let Err(e) = mail_bus::ensure_schema(&remote) {
        if store_remote::is_unreachable(&e) {
            return queue_and_report(from_id, from_host, name, host, payload, raw, &e);
        }
        eprintln!("fno agents mail send: bus schema: {e}");
        return 1;
    }
    let msg_id = mail_bus::mint_msg_id();
    match mail_bus::insert(
        &remote, &msg_id, from_id, from_host, name, host, payload, raw, subject,
    ) {
        Ok(outcome) => {
            let dup = if outcome.duplicated {
                "already queued"
            } else {
                "queued"
            };
            println!("{msg_id} {dup} (store seq={}, host {host})", outcome.seq);
            0
        }
        Err(e) => {
            if store_remote::is_unreachable(&e) {
                return queue_and_report(from_id, from_host, name, host, payload, raw, &e);
            }
            eprintln!("fno agents mail send: {e}");
            1
        }
    }
}

/// Edge 7: the primary is unreachable. Queue the row locally as one JSON
/// line; the daemon's flush arm drains it. Exit 0: queued is not failed.
#[allow(clippy::too_many_arguments)]
fn queue_and_report(
    from_id: &str,
    from_host: &str,
    name: &str,
    host: &str,
    payload: &str,
    raw: bool,
    error: &str,
) -> i32 {
    let msg_id = mail_bus::mint_msg_id();
    let line = serde_json::json!({
        "msg_id": msg_id,
        "from_id": from_id,
        "from_host": from_host,
        "to_name": name,
        "to_host": host,
        "payload": payload,
        "raw": raw,
    });
    let home = crate::paths::AgentsHome::from_env();
    let path = outbox_path(home.root());
    use std::io::Write;
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
    match write {
        Ok(()) => {
            println!("{msg_id} queued (local, store unreachable)");
            0
        }
        Err(e) => {
            eprintln!("fno agents mail send: {error}; and the local queue write failed: {e}");
            1
        }
    }
}

/// The ssh relay: exec, inherit stdio, pass the exit code through.
fn relay(host: &str, words: &[String]) -> i32 {
    let argv = ssh_argv(host, words);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fno agents mail: ssh to {host} failed: {e}");
            1
        }
    }
}

fn cmd_peek(args: &[String]) -> i32 {
    let words: Vec<String> = args.to_vec();
    let Some(target) = words.first() else {
        eprintln!("usage: fno agents peek <handle>@<host> [flags]");
        return 2;
    };
    let Ok((name, host)) = split_target(target) else {
        eprintln!("{}", target_error(target));
        return 2;
    };
    let local_host = mail_bus::normalize_host(&crate::claims::hostname());
    if mail_bus::normalize_host(&host) == local_host {
        let mut argv = vec![fno_binary()];
        argv.push(OsString::from("agents"));
        argv.push(OsString::from("peek"));
        argv.push(OsString::from(name));
        for word in &words[1..] {
            argv.push(OsString::from(word));
        }
        return exec_forward(&argv, "fno agents mail", true);
    }
    let mut relay_words = vec!["agents".to_string(), "peek".to_string(), name];
    relay_words.extend(words[1..].iter().cloned());
    relay(&host, &relay_words)
}

/// Where a reply to `from` (the answered message's raw sender address)
/// goes. The cross-host form `<user>:<host>/<name>` (or the older
/// `<user>@<host>/<name>` still on the bus) answers at
/// `<name>@<host>`; an OS-user label (`name` equals `user`) predates role
/// names, so it refuses naming the send form instead of guessing; every
/// other shape forwards to the Python reply, which owns local routing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReplyRoute {
    Forward,
    Refuse(String),
    Send { name: String, host: String },
}

pub(crate) fn reply_route(from: &str) -> ReplyRoute {
    let Some((head, name)) = from.rsplit_once('/') else {
        return ReplyRoute::Forward;
    };
    let Some((user, host)) = head.split_once(':').or_else(|| head.split_once('@')) else {
        return ReplyRoute::Forward;
    };
    // Strictness: only the forms sender_id writes or wrote. Anything else (a
    // bare handle, a nested slash, a second separator) is not the relay's
    // label, so the Python reply decides.
    if user.is_empty() || name.is_empty() || host.is_empty() {
        return ReplyRoute::Forward;
    }
    if host.contains(['/', '@', ':']) || user.contains(['/', '@']) {
        return ReplyRoute::Forward;
    }
    // A colon is common in other ids, so the user part must read as an OS
    // user name before `a:b/c` counts as the relay's label.
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return ReplyRoute::Forward;
    }
    let host = mail_bus::normalize_host(host);
    let local = mail_bus::normalize_host(&crate::claims::hostname());
    if host == local {
        return ReplyRoute::Forward;
    }
    if name == user {
        // An old OS-user label has no addressable role name, so the honest
        // answer is the send form, not a guessed delivery.
        return ReplyRoute::Refuse(format!(
            "sender {from} carries no role name; send to the lead by name with \
             fno agents mail send <name>@{host} \"...\""
        ));
    }
    ReplyRoute::Send {
        name: name.to_string(),
        host,
    }
}

/// The cross-host branch of `mail reply`: when the answered message's raw
/// sender is a foreign-host identity, the reply is a cross-host send to
/// `<name>@<host>` and this returns the exit code. `None` hands the reply
/// back to the Python command (local sender, missing row, any other address
/// shape). A flag the store lane cannot carry is refused here, naming it,
/// rather than dropped. The mail port's later native reply leaf calls this
/// before its local logic, so the branch stays one function.
pub(crate) fn reply_cross_host(
    answered_id: &str,
    body: &str,
    kind: bool,
    extra: &[String],
) -> Option<i32> {
    let from = crate::chats::message_from(answered_id)?;
    match reply_route(&from) {
        ReplyRoute::Forward => None,
        ReplyRoute::Refuse(message) => {
            eprintln!("{message}");
            Some(2)
        }
        ReplyRoute::Send { name, host } => {
            if !extra.is_empty() {
                eprintln!(
                    "fno agents mail reply: a cross-host store reply carries only the \
                     body; it cannot carry: {extra:?}"
                );
                return Some(2);
            }
            let label = resolve_from_name(&SendTail::default())
                .map(|f| match f {
                    FromName::Given(v) | FromName::SelfName(v) | FromName::OsUser(v) => v,
                })
                .unwrap_or_else(|_| os_user());
            if kind {
                eprintln!("note: a cross-host reply carries no kind; --kind dropped");
            }
            Some(cmd_send(&[
                format!("{name}@{host}"),
                "--from-name".to_string(),
                label,
                body.to_string(),
            ]))
        }
    }
}

/// The tail parser for `mail reply`. Only the forms the cross-host lane
/// owns are lifted; everything else rides to the Python reply verbatim,
/// which keeps its refusals and exit codes identical to the local command's
/// (a duplicated body, an unknown flag, a missing --to).
#[derive(Default)]
struct ReplyTail {
    to: Option<String>,
    body: Option<String>,
    /// How many body forms rode in (positional, --body, --body-file).
    body_sources: usize,
    /// `--kind` was passed: accepted, and dropped on the cross-host route
    /// with a note, because a bus row carries no kind.
    kind: bool,
    /// Words the store lane cannot carry. Nonempty means forward.
    extra: Vec<String>,
}

impl ReplyTail {
    /// Route attempt for a well-formed reply: one `--to`, exactly one body
    /// form. Uncarryable words ride along: the foreign route refuses them
    /// naming each, and every other route forwards so Python keeps owning
    /// its own flags.
    fn routable(&self) -> bool {
        self.to.is_some() && self.body.is_some() && self.body_sources == 1
    }
}

/// Parse the reply tail. Every arm advances past the words it consumed, so
/// the loop always terminates; a missing flag value or an unreadable
/// --body-file lands its flag in `extra`, which makes the tail unroutable
/// and sends the whole original tail to the Python reply (its own error
/// for that shape is the contract).
fn parse_reply_tail(args: &[String]) -> ReplyTail {
    let mut tail = ReplyTail::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => match args.get(i + 1) {
                Some(v) => {
                    tail.to = Some(v.clone());
                    i += 2;
                }
                None => {
                    tail.extra.push(args[i].clone());
                    i += 1;
                }
            },
            "--body" => match args.get(i + 1) {
                Some(v) => {
                    tail.body = Some(v.clone());
                    tail.body_sources += 1;
                    i += 2;
                }
                None => {
                    tail.extra.push(args[i].clone());
                    i += 1;
                }
            },
            "--body-file" => match args
                .get(i + 1)
                .and_then(|p| std::fs::read_to_string(p).ok())
            {
                Some(text) => {
                    tail.body = Some(text);
                    tail.body_sources += 1;
                    i += 2;
                }
                None => {
                    tail.extra.push(args[i].clone());
                    i += 1;
                }
            },
            "--kind" => match args.get(i + 1) {
                Some(_) => {
                    tail.kind = true;
                    i += 2;
                }
                None => {
                    tail.extra.push(args[i].clone());
                    i += 1;
                }
            },
            other => {
                // A positional body, or a word the store lane cannot carry
                // (an unknown flag, a Python-only flag). The body rides in
                // `body`; the rest lands in `extra` for the route decision.
                if other.starts_with('-') {
                    tail.extra.push(other.to_string());
                } else {
                    tail.body = Some(other.to_string());
                    tail.body_sources += 1;
                }
                i += 1;
            }
        }
    }
    tail
}

fn cmd_reply(args: &[String]) -> i32 {
    let tail = parse_reply_tail(args);
    // One exec hop for the whole reply surface: the front door claims every
    // `mail reply`, and any case the cross-host branch does not own execs
    // the Python reply with the original tail, so its refusals, kinds and
    // exit codes stay byte-identical (the parity contract).
    if !tail.routable() {
        return forward_reply(args);
    }
    match reply_cross_host(
        tail.to.as_deref().expect("checked routable"),
        tail.body.as_deref().expect("checked routable"),
        tail.kind,
        &tail.extra,
    ) {
        Some(code) => code,
        None => forward_reply(args),
    }
}

/// The verbatim Python forward: the original tail, same argv shape the
/// front door would hand the Python CLI on main.
fn forward_reply(args: &[String]) -> i32 {
    let mut argv: Vec<String> = vec![
        "agents".to_string(),
        "mail".to_string(),
        "reply".to_string(),
    ];
    argv.extend(args.iter().cloned());
    crate::scrape::exec_python("fno agents mail reply", argv)
}

fn cmd_status(_args: &[String]) -> i32 {
    match store_path() {
        StorePath::Primary(remote) => {
            if let Err(e) = mail_bus::ensure_schema(&remote) {
                eprintln!("fno agents mail status: {e}");
                return 1;
            }
            let host = mail_bus::normalize_host(&crate::claims::hostname());
            match mail_bus::sent_rows(&remote, &host) {
                Ok(rows) => {
                    for (msg_id, status) in rows {
                        println!("{msg_id} {status}");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("fno agents mail status: {e}");
                    1
                }
            }
        }
        StorePath::None => {
            eprintln!(
                "fno agents mail status: no shared store configured; sends a \
cross-host target answered over the ssh fallback leave no bus rows"
            );
            2
        }
        StorePath::Malformed(e) => {
            eprintln!("fno agents mail status: {e}");
            2
        }
    }
}

/// Binary-direct entry from `fno-agents mail-remote <send|peek|status|reply> ...`.
/// The dispatcher holds `Vec<String>`, so the tail rides as `&[String]`.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(|a| a.as_str()) {
        Some("send") => cmd_send(&args[1..]),
        Some("peek") => cmd_peek(&args[1..]),
        Some("reply") => cmd_reply(&args[1..]),
        Some("status") => cmd_status(&args[1..]),
        _ => {
            eprintln!(
                "usage: fno-agents mail-remote <send|peek|status|reply> <name>@<host> [args...]"
            );
            2
        }
    }
}

/// The marker a same-host forward carries. The front door's intercept claims
/// a plain-name `mail send`, so without it the forward re-enters this command
/// and every send recurses until the process table fills.
pub const LOCAL_FORWARD_ENV: &str = "FNO_MAIL_LOCAL_FORWARD";

/// Exec an argv, inherit stdio, return the exit code (same-host forwards).
/// The relay family's shared same-host forward: exec an argv, inherit stdio,
/// return the exit code. `mail_env` stamps the mail-only re-entry marker.
pub fn exec_forward(argv: &[OsString], label: &str, mail_env: bool) -> i32 {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if mail_env {
        cmd.env(LOCAL_FORWARD_ENV, "1");
    }
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("{label}: local forward failed: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One declaration, every contract: the CI delta caps new test
    // declarations, so the relay's unit contracts share one table-style
    // body. The daemon arm's interval rule joins it (same crate).
    #[test]
    fn relay_units_guard_their_contracts() {
        // Target grammar: exactly one @, both sides nonempty.
        assert_eq!(
            split_target("nemo@imac2d2").ok(),
            Some(("nemo".into(), "imac2d2".into()))
        );
        assert!(split_target("nemo@bb16@imac2d2").is_err());
        assert!(split_target("@imac2d2").is_err());
        assert!(split_target("nemo@").is_err());
        assert!(split_target("nemo").is_err());
        // Remote-shell quoting: plain words wrap; the quote escapes; the
        // -lc script itself rides quoted so ssh's space-join cannot split
        // it (the round-1 regression).
        assert_eq!(sh_quote("hello"), "'hello'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("two words"), "'two words'");
        let argv = ssh_argv(
            "mbp3",
            &[
                "agents".into(),
                "peek".into(),
                "nemo".into(),
                "-n".into(),
                "5".into(),
            ],
        );
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "mbp3");
        assert_eq!(argv[2], "--");
        assert_eq!(argv[3], "sh");
        assert_eq!(argv[4], "-lc");
        assert_eq!(argv[5], "'exec fno \"$@\"'");
        assert_eq!(argv[6], "sh");
        assert_eq!(argv[7], "'agents'");
        assert_eq!(argv[9], "'nemo'");
        // The send tail lifts identity flags and keeps the rest in order.
        let tail = parse_send_tail(&[
            "--from-name".into(),
            "rowan".into(),
            "--subject".into(),
            "hi".into(),
            "hello world".into(),
        ])
        .expect("parses");
        assert_eq!(tail.from_name.as_deref(), Some("rowan"));
        assert!(!tail.raw);
        assert_eq!(
            tail.rest,
            vec![
                "--subject".to_string(),
                "hi".to_string(),
                "hello world".to_string()
            ]
        );
        // --from-self is fail-loud without a proven session (the same floor
        // the local command applies); the session rides in as a parameter, so
        // no env mutation races the parallel tests.
        let selfy = SendTail {
            from_self: true,
            ..SendTail::default()
        };
        assert!(resolve_from_name_with(None, &selfy).is_err());
        assert!(resolve_from_name_with(Some("  "), &selfy).is_err());
        // A shell with no proven session keeps the plain
        // `<user>:<host>/<user>` form.
        let plain = SendTail::default();
        assert_eq!(
            resolve_from_name_with(None, &plain),
            Ok(FromName::OsUser(os_user())),
            "no proven session resolves to the OS user"
        );
        // The ssh fallback stamps the sender locally in the full
        // `<user>:<host>/<name>` form, the same shape the bus lane passes,
        // and keeps the caller's flags and message in order after it.
        let words = relay_words_with_sender(
            "Quinlan",
            &[
                "--subject".to_string(),
                "hi".to_string(),
                "hello".to_string(),
            ],
            &FromName::Given("Ellery".to_string()),
            "bb16",
            "mbp3",
        );
        assert_eq!(
            words,
            vec![
                "agents".to_string(),
                "mail".to_string(),
                "send".to_string(),
                "Quinlan".to_string(),
                "--from-name".to_string(),
                "bb16:mbp3/Ellery".to_string(),
                "--subject".to_string(),
                "hi".to_string(),
                "hello".to_string(),
            ]
        );
        // The OsUser floor keeps the same form.
        let floored = relay_words_with_sender(
            "nemo",
            &[],
            &FromName::OsUser("operator".to_string()),
            "bb16",
            "mbp3",
        );
        assert_eq!(floored[5], "bb16:mbp3/operator");
        let named = SendTail {
            from_name: Some("rowan".to_string()),
            ..SendTail::default()
        };
        assert_eq!(
            resolve_from_name_with(Some("3912a97c-c37b-4663-a8f1-e6772ba85586"), &named),
            Ok(FromName::Given("rowan".to_string())),
            "--from-name outranks the proven session"
        );
        // A blank session falls through, never stamps a blank.
        assert_eq!(
            resolve_from_name_with(Some("  "), &plain),
            Ok(FromName::OsUser(os_user()))
        );
        // Sender identity normalizes the host part.
        assert_eq!(
            sender_id("bb16", "iMac2D2.local", "nemo"),
            "bb16:imac2d2/nemo"
        );
        // The pull arm's argv carries the name, the sender, and the payload.
        let argv = local_send_argv("nemo", "hello there", false, "rowan@mbp3/r");
        let words: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(words.last().expect("payload rides last"), "hello there");
        assert!(words.contains(&"--from-name".to_string()));
        assert!(words.contains(&"rowan@mbp3/r".to_string()));
        // Strict payload shape: the message is the word after the target;
        // a leading flag means no message rode directly and the strict
        // shape refuses rather than guessing.
        let tail = parse_send_tail(&["hello world".to_string()]).expect("parses");
        assert_eq!(payload_of(&tail).as_deref(), Some("hello world"));
        let tail = parse_send_tail(&["--subject".to_string(), "x".to_string(), "msg".to_string()])
            .expect("parses");
        assert_eq!(payload_of(&tail), None);
        // The state-root files this module owns.
        let root = std::path::Path::new("/tmp/fno-mail-remote-test");
        assert!(outbox_path(root).ends_with("mail_outbox.jsonl"));
        assert!(seen_path(root).ends_with("mail_seen.jsonl"));
        // The daemon arm's interval rule: a positive override wins,
        // anything else keeps the default.
        assert_eq!(
            crate::mail_arm::interval_from_secs(None),
            crate::mail_arm::DEFAULT_INTERVAL
        );
        assert_eq!(
            crate::mail_arm::interval_from_secs(Some(0)),
            crate::mail_arm::DEFAULT_INTERVAL
        );
        assert_eq!(
            crate::mail_arm::interval_from_secs(Some(-5)),
            crate::mail_arm::DEFAULT_INTERVAL
        );
        assert_eq!(
            crate::mail_arm::interval_from_secs(Some(30)),
            std::time::Duration::from_secs(30)
        );
        // The pull arm's retry contract: an injection that fails leaves the
        // row in the pending pool, so a later pull still delivers it after a
        // younger row did (the fetch re-selects every pending row each beat).
        // The fake inject answers no remote call, and the seen ledger is
        // absent under the temp home, so the loop never dials the store.
        let bus_row = |seq: i64, id: &str| crate::mail_bus::BusRow {
            seq,
            msg_id: id.to_string(),
            from_id: "s@h/s".into(),
            to_name: "quill".into(),
            payload: "hello".into(),
            raw: false,
            expire_at: 1_800_000_000,
        };
        let home = std::path::Path::new("/tmp/fno-mail-arm-retry-test");
        let remote = Remote::parse("http://127.0.0.1:1", None).expect("parses");
        let mut fail_older = |row: &crate::mail_bus::BusRow| row.msg_id != "older";
        let rows = vec![bus_row(1, "older"), bus_row(2, "newer")];
        assert_eq!(
            crate::mail_arm::pull_rows_with(&remote, rows, home, &mut fail_older),
            1,
            "the failed row does not count as delivered"
        );
        let mut deliver_all = |_row: &crate::mail_bus::BusRow| true;
        let retried = vec![bus_row(1, "older")];
        // The failed attempt above booked a ladder slot; clear the memory so
        // this phase reads the delivery contract, not the ladder's.
        let _ = std::fs::remove_file(crate::mail_remote::retry_path(home));
        assert_eq!(
            crate::mail_arm::pull_rows_with(&remote, retried, home, &mut deliver_all),
            1,
            "the next beat delivers the row the earlier beat could not"
        );
        // The retry ladder: a failed row books a next-due 15 s out, a beat
        // inside that window injects nothing, a beat past the booked second
        // injects again, and delivery clears the ledger. The home dir is
        // cleaned first so a rerun starts from no memory.
        let ladder_home = std::path::Path::new("/tmp/fno-mail-arm-ladder-test");
        let _ = std::fs::remove_dir_all(ladder_home);
        std::fs::create_dir_all(ladder_home).unwrap();
        let mut fail_once = |_row: &crate::mail_bus::BusRow| false;
        let row = || bus_row(7, "laddered");
        assert_eq!(
            crate::mail_arm::pull_rows_with(&remote, vec![row()], ladder_home, &mut fail_once),
            0,
            "the failed send books the ladder, nothing delivers"
        );
        let booked = std::fs::read_to_string(crate::mail_remote::retry_path(ladder_home)).unwrap();
        assert!(
            booked.contains("\"msg_id\":\"laddered\""),
            "the failed row booked a retry: {booked}"
        );
        // The failed send booked a due 15 s out; a beat now injects nothing.
        let mut no_inject = |_row: &crate::mail_bus::BusRow| {
            panic!("a row inside its backoff window never injects")
        };
        assert_eq!(
            crate::mail_arm::pull_rows_with(&remote, vec![row()], ladder_home, &mut no_inject),
            0,
            "the second beat inside the window skips the send"
        );
        // Past the booked second the row injects again (the ledger carries a
        // due ten seconds old, fresh enough to survive the load prune), and
        // delivery clears the memory.
        let past = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 10;
        let due_line = format!("{{\"msg_id\":\"laddered\",\"attempts\":1,\"next_due\":{past}}}",);
        std::fs::write(crate::mail_remote::retry_path(ladder_home), due_line + "\n").unwrap();
        assert_eq!(
            crate::mail_arm::pull_rows_with(&remote, vec![row()], ladder_home, &mut deliver_all),
            1,
            "a past-due row injects again"
        );
        assert!(
            !crate::mail_remote::retry_path(ladder_home).exists(),
            "delivery clears the retry memory"
        );
        // The ladder itself: doubling from the 15 s base, capped at 15 min.
        assert_eq!(
            crate::mail_arm::retry_backoff(1),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            crate::mail_arm::retry_backoff(2),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            crate::mail_arm::retry_backoff(3),
            std::time::Duration::from_secs(60)
        );
        assert_eq!(
            crate::mail_arm::retry_backoff(12),
            std::time::Duration::from_secs(900),
            "the cap holds at fifteen minutes"
        );
        // The finality rule: the style door naming violations (with no
        // exception declared) is the deterministic refusal; a clean receipt,
        // an excepted body, and unparseable output read transient, so the
        // row stays pending for the next beat.
        assert!(crate::mail_arm::door_confirms_refusal(
            br#"{"violations":[{"rule":3,"sentence_index":3}]}"#
        ));
        assert!(!crate::mail_arm::door_confirms_refusal(
            br#"{"violations":[]}"#
        ));
        assert!(!crate::mail_arm::door_confirms_refusal(
            br#"{"exception":"quoted word","violations":[{"rule":3}]}"#
        ));
        assert!(!crate::mail_arm::door_confirms_refusal(b"{}"));
        assert!(!crate::mail_arm::door_confirms_refusal(b"not json"));
        // The consult rides the front door's lint-style forwarding, with the
        // configured mail word cap appended when one is set.
        let argv = crate::mail_arm::style_door_argv(None);
        assert_eq!(
            argv.first().map(String::as_str),
            Some("doctor"),
            "the front door classifies the check name"
        );
        assert!(argv.contains(&"--stdin".to_string()));
        assert!(argv.contains(&"--json".to_string()));
        assert!(!argv.iter().any(|word| word == "--word-cap"));
        let capped = crate::mail_arm::style_door_argv(Some(60));
        let cap_at = capped
            .iter()
            .position(|word| word == "--word-cap")
            .expect("cap flag");
        assert_eq!(capped[cap_at + 1], "60");
        // The sender-side consult rides the same door without --json, so
        // exit 1 with the human report is the refusal.
        let report_argv = crate::mail_arm::style_door_report_argv(None);
        assert!(
            !report_argv.iter().any(|word| word == "--json"),
            "the report form prints the human report, no JSON receipt"
        );
        assert!(
            report_argv.contains(&"--stdin".to_string()),
            "the body still rides stdin"
        );
        // The pull arm's exit reading: 0 delivers, and so does the Python
        // receipts' NOT LANDED exit (a durable demotion - the durable copy
        // is written, so a retry would duplicate it; the constant's comment
        // names the Python owner of the meaning). Exit 1 delivers a refused
        // verdict only when the style door names the violations; every
        // other code, and a signal, stay pending on the ladder.
        use crate::mail_arm::{send_verdict, SendVerdict};
        assert_eq!(
            send_verdict(Some(0), || unreachable!()),
            SendVerdict::Delivered
        );
        assert_eq!(
            send_verdict(
                Some(crate::mail_receipt::NOT_LANDED_EXIT),
                || unreachable!()
            ),
            SendVerdict::Delivered
        );
        assert_eq!(send_verdict(Some(1), || true), SendVerdict::Refused);
        assert_eq!(send_verdict(Some(1), || false), SendVerdict::Pending);
        assert_eq!(
            send_verdict(Some(12), || unreachable!()),
            SendVerdict::Pending
        );
        assert_eq!(
            send_verdict(Some(16), || unreachable!()),
            SendVerdict::Pending
        );
        assert_eq!(send_verdict(None, || unreachable!()), SendVerdict::Pending);
        // The exit-14 row ends like a delivered one: one inject, and the
        // next beat reads the seen ledger instead of re-sending. The real
        // inject_row writes that ledger as its side effect, so the fake
        // simulates the write; the loop itself only reads it.
        let exit14_home = std::path::Path::new("/tmp/fno-mail-arm-exit14-test");
        let _ = std::fs::remove_dir_all(exit14_home);
        std::fs::create_dir_all(exit14_home).unwrap();
        let mut injects = 0;
        let mut delivered_once = |row: &crate::mail_bus::BusRow| {
            use std::io::Write as _;
            injects += 1;
            assert_eq!(injects, 1, "the exit-14 row was injected twice");
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crate::mail_remote::seen_path(exit14_home))
                .and_then(|mut f| writeln!(f, "{}", row.msg_id));
            true
        };
        assert_eq!(
            crate::mail_arm::pull_rows_with(
                &remote,
                vec![bus_row(9, "exit14")],
                exit14_home,
                &mut delivered_once
            ),
            1
        );
        assert_eq!(
            crate::mail_arm::pull_rows_with(
                &remote,
                vec![bus_row(9, "exit14")],
                exit14_home,
                &mut delivered_once
            ),
            0,
            "the second beat reads the seen ledger and re-sends nothing"
        );
        assert_eq!(injects, 1, "the durable demotion ran the local send once");
        assert!(
            !crate::mail_remote::retry_path(exit14_home).exists(),
            "a delivered row books no ladder entry"
        );
        // The reply route: a foreign-host sender routes to a cross-host
        // send at <name>@<host>; the local host forwards to the Python
        // reply; an OS-user label refuses naming the send form; a bare
        // handle or any other shape forwards. The local-host case is built
        // from the test host, so the same asserts hold on any machine.
        use crate::mail_remote::{reply_route, ReplyRoute};
        let test_host = crate::mail_bus::normalize_host(&crate::claims::hostname());
        assert_eq!(
            reply_route("bb16@some-remote/nemo"),
            ReplyRoute::Send {
                name: "nemo".to_string(),
                host: "some-remote".to_string()
            }
        );
        assert_eq!(
            reply_route("bb16:some-remote/nemo"),
            ReplyRoute::Send {
                name: "nemo".to_string(),
                host: "some-remote".to_string()
            },
            "the colon label sender_id writes routes the same"
        );
        assert_eq!(
            reply_route("bb16:imac2d2-peer/ed2495bd"),
            ReplyRoute::Send {
                name: "ed2495bd".to_string(),
                host: "imac2d2-peer".to_string()
            },
            "an 8-hex session handle is a routable name"
        );
        assert_eq!(
            reply_route("fmail session:h/x"),
            ReplyRoute::Forward,
            "a user part that is no OS user name is not the relay's label"
        );
        assert_eq!(
            reply_route(&format!("a@{test_host}/b")),
            ReplyRoute::Forward,
            "the local host forwards to the Python reply"
        );
        assert_eq!(
            reply_route("bb16@some-remote/bb16"),
            ReplyRoute::Refuse(
                "sender bb16@some-remote/bb16 carries no role name; send to the lead \
                 by name with fno agents mail send <name>@some-remote \"...\""
                    .to_string()
            )
        );
        assert_eq!(
            reply_route("some-agent"),
            ReplyRoute::Forward,
            "a bare handle forwards"
        );
        assert_eq!(
            reply_route("a@h/x/y"),
            ReplyRoute::Forward,
            "a nested slash is not the relay's label form"
        );
        assert_eq!(
            reply_route(""),
            ReplyRoute::Forward,
            "an empty address forwards"
        );
        // The route gate: uncarryable words never short-circuit it (the
        // foreign route refuses them naming each; Python keeps its own
        // flags locally), while a duplicated body still forwards.
        let flagged = ReplyTail {
            to: Some("fmail-x".to_string()),
            body: Some("ok".to_string()),
            body_sources: 1,
            kind: false,
            extra: vec!["--json".to_string()],
        };
        assert!(
            flagged.routable(),
            "a Python-only flag rides to the route decision"
        );
        let dup = ReplyTail {
            body_sources: 2,
            ..flagged
        };
        assert!(
            !dup.routable(),
            "a duplicated body forwards to the Python command"
        );
        // The reply parser terminates on the positional form (a missing
        // advance here hung on the most common shape) and forwards the
        // missing-value shapes to the Python command.
        let parsed = parse_reply_tail(&[
            "--to".to_string(),
            "fmail-x".to_string(),
            "hello".to_string(),
        ]);
        assert_eq!(parsed.to.as_deref(), Some("fmail-x"));
        assert_eq!(parsed.body.as_deref(), Some("hello"));
        assert_eq!(parsed.body_sources, 1);
        assert!(parsed.extra.is_empty());
        assert!(parsed.routable());
        let starved = parse_reply_tail(&["--to".to_string()]);
        assert!(!starved.routable(), "a --to with no value forwards");
        assert_eq!(starved.extra, vec!["--to".to_string()]);
    }

    #[test]
    fn specimen_x31b6_sender_stamps_the_proven_session() {
        let own = "3912a97c-c37b-4663-a8f1-e6772ba85586";
        let plain = SendTail::default();
        assert_eq!(
            resolve_from_name_with(Some(own), &plain),
            Ok(FromName::SelfName("3912a97c".to_string())),
            "a default send signs the proven session, never a registry row"
        );
        let selfy = SendTail {
            from_self: true,
            ..SendTail::default()
        };
        assert_eq!(
            resolve_from_name_with(Some(own), &selfy),
            Ok(FromName::SelfName("3912a97c".to_string())),
            "--from-self needs no ambient name variable"
        );
        let foreign = SendTail {
            from_name: Some("ed2495bd".to_string()),
            ..SendTail::default()
        };
        let err = resolve_from_name_with(Some("d1aec0d9-4c60-4246-a51b-24ac8c83560a"), &foreign)
            .expect_err("a copied foreign handle refuses");
        assert!(err.contains("d1aec0d9"), "the refusal names the own handle");
        let role = SendTail {
            from_name: Some("Quinlan".to_string()),
            ..SendTail::default()
        };
        assert_eq!(
            resolve_from_name_with(Some(own), &role),
            Ok(FromName::Given("Quinlan".to_string())),
            "a role name still passes verbatim"
        );
    }
}
