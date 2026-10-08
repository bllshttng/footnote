//! Move a session bundle to another computer: a pairing code, or a file.
//!
//! `transcript send <session-id>` bundles the transcript with its origin
//! record and offers it over a magic-wormhole pairing code; `--file <path>`
//! writes the same bundle to a path instead (the fallback when no relay is
//! wanted). `transcript receive <code|path>` unpacks into the local
//! transcript store at the source-relative path, writes the origin record
//! beside it, and prints the resume hints. fno runs no relay and no daemon
//! for this: the transfer dials the public magic-wormhole transit directly.

use crate::claude_drive;
use crate::claude_transcript_paths;
use crate::codex_store;
use crate::session_origin::{read_beside, write_record_beside, SessionOrigin};
use magic_wormhole::{transfer, transit, Code, MailboxConnection, Wormhole};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 6] = b"FNOS1\n";
const RELAY_HOST: &str = "transit.magic-wormhole.io";
const RELAY_PORT: u16 = 4001;

/// What the receiving machine needs to rebuild the transcript's location.
/// Everything but the envelope shape is untrusted input on receive.
#[derive(Debug, Serialize, Deserialize)]
struct BundleMeta {
    version: u32,
    harness: String,
    session_id: String,
    branch: String,
    /// Path of the transcript relative to its store root on the sender.
    rel: String,
}

fn push_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn read_u64(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    if at + 8 > bytes.len() {
        return None;
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[at..at + 8]);
    Some((u64::from_le_bytes(raw), at + 8))
}

fn build_envelope(
    meta: &BundleMeta,
    transcript: &[u8],
    origin: Option<&SessionOrigin>,
) -> std::io::Result<Vec<u8>> {
    let meta_bytes = serde_json::to_vec(meta).map_err(|e| std::io::Error::other(e.to_string()))?;
    let origin_bytes = origin
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .unwrap_or_default();
    let mut out = Vec::with_capacity(MAGIC.len() + 24 + meta_bytes.len() + transcript.len());
    out.extend_from_slice(MAGIC);
    push_u64(&mut out, meta_bytes.len() as u64);
    out.extend_from_slice(&meta_bytes);
    push_u64(&mut out, transcript.len() as u64);
    out.extend_from_slice(transcript);
    push_u64(&mut out, origin_bytes.len() as u64);
    out.extend_from_slice(&origin_bytes);
    Ok(out)
}

fn parse_envelope(bytes: &[u8]) -> Result<(BundleMeta, &[u8], Option<SessionOrigin>), String> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err("not an fno session bundle (bad header)".into());
    }
    let mut at = MAGIC.len();
    let (meta_len, next) = read_u64(bytes, at).ok_or("truncated bundle")?;
    at = next;
    let end = at
        .checked_add(meta_len as usize)
        .filter(|&end| end <= bytes.len())
        .ok_or("truncated bundle")?;
    let meta: BundleMeta = serde_json::from_slice(&bytes[at..end])
        .map_err(|e| format!("bundle meta unreadable: {e}"))?;
    at = end;
    let (transcript_len, next) = read_u64(bytes, at).ok_or("truncated bundle")?;
    at = next;
    let end = at
        .checked_add(transcript_len as usize)
        .filter(|&end| end <= bytes.len())
        .ok_or("truncated bundle")?;
    let transcript = &bytes[at..end];
    let (origin_len, next) = read_u64(bytes, next).ok_or("truncated bundle")?;
    let end = next
        .checked_add(origin_len as usize)
        .filter(|&end| end <= bytes.len())
        .ok_or("truncated bundle")?;
    if meta.version != 1 {
        return Err(format!(
            "unsupported bundle format version {}",
            meta.version
        ));
    }
    let origin: Option<SessionOrigin> = if origin_len == 0 {
        None
    } else {
        Some(
            serde_json::from_slice(&bytes[next..end])
                .map_err(|e| format!("origin record unreadable: {e}"))?,
        )
    };
    Ok((meta, transcript, origin))
}

/// A relative path is safe to join under the store root only when it never
/// escapes it. Bundle meta is untrusted: an absolute path, a `..` part, or
/// an empty remnant refuses the whole placement.
fn safe_rel(rel: &str) -> Option<PathBuf> {
    let rel = rel.replace('\\', "/");
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn current_branch() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

fn relay_hints() -> Vec<transit::RelayHint> {
    vec![transit::RelayHint::new(RELAY_HOST, RELAY_PORT)]
}

fn print_code(code: &Code) {
    println!("Wormhole code: {code}");
    println!("On the other computer, run: fno agents transcript receive '{code}'");
    println!("Waiting for the receiver...");
}

/// Locate one session's transcript and name the store it lives in. Claude
/// resolves through the cap-and-intel resolver; codex through the rollout
/// locator. The relative path is the answer's suffix under its store root,
/// so the receiving machine rebuilds the same location.
fn locate(sid: &str) -> Result<(PathBuf, &'static str, PathBuf), String> {
    let claude_root = claude_drive::claude_projects_dir();
    if let Some(path) = claude_transcript_paths::resolve_transcript(&claude_root, sid) {
        let rel = path
            .strip_prefix(&claude_root)
            .map_err(|_| format!("{} sits outside the claude store", path.display()))?;
        return Ok((path, "claude", rel.to_path_buf()));
    }
    if let Some(path) = codex_store::codex_rollout_path(None, sid) {
        let root = codex_store::codex_home()
            .map(|home| home.join("sessions"))
            .ok_or("the codex home is unreadable")?;
        let rel = path
            .strip_prefix(&root)
            .map_err(|_| format!("{} sits outside the codex store", path.display()))?;
        return Ok((path, "codex", rel.to_path_buf()));
    }
    Err(format!(
        "no transcript for {sid} (claude projects and codex sessions searched)"
    ))
}

/// The local root of a store named the way `locate` names it.
fn store_root(store: &str) -> Option<PathBuf> {
    match store {
        "claude" => Some(claude_drive::claude_projects_dir()),
        "codex" => codex_home_sessions(),
        _ => None,
    }
}

fn codex_home_sessions() -> Option<PathBuf> {
    codex_store::codex_home().map(|home| home.join("sessions"))
}

async fn run_send(rest: &[String]) -> i32 {
    let mut it = crate::client_verbs::expand_eq(rest).into_iter();
    let mut sid = String::new();
    let mut file_dest: Option<PathBuf> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--file" => {
                file_dest = Some(PathBuf::from(it.next().unwrap_or_else(|| {
                    eprintln!("transcript send: --file needs a path");
                    std::process::exit(2);
                })));
            }
            other if sid.is_empty() => sid = other.to_string(),
            other => {
                eprintln!("transcript send: unexpected argument {other}");
                return 2;
            }
        }
    }
    if sid.is_empty() {
        eprintln!("transcript send: a session id is required");
        return 2;
    }
    let (transcript, store, rel) = match locate(&sid) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("transcript send: {e}");
            return 1;
        }
    };
    let bytes = match std::fs::read(&transcript) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("transcript send: {}: {e}", transcript.display());
            return 1;
        }
    };
    let origin = read_beside(&transcript, &sid);
    let meta = BundleMeta {
        version: 1,
        harness: store.to_string(),
        session_id: sid.clone(),
        branch: current_branch(),
        rel: rel.to_string_lossy().into_owned(),
    };
    let envelope = match build_envelope(&meta, &bytes, origin.as_ref()) {
        Ok(envelope) => envelope,
        Err(e) => {
            eprintln!("transcript send: bundle failed: {e}");
            return 1;
        }
    };
    if let Some(dest) = file_dest {
        if dest.exists() {
            eprintln!("transcript send: {} exists", dest.display());
            return 1;
        }
        return match std::fs::write(&dest, &envelope) {
            Ok(()) => {
                println!("bundle: {}", dest.display());
                println!("Move it any way you like, then: fno agents transcript receive <path>");
                0
            }
            Err(e) => {
                eprintln!("transcript send: {}: {e}", dest.display());
                1
            }
        };
    }
    let send_name = format!("{sid}.fno-session");
    let size = envelope.len() as u64;
    let sent = async move {
        let mailbox = MailboxConnection::create(transfer::APP_CONFIG, 2)
            .await
            .map_err(|e| e.to_string())?;
        let code = mailbox.code.clone();
        print_code(&code);
        let wh = Wormhole::connect(mailbox)
            .await
            .map_err(|e| e.to_string())?;
        let mut cursor = std::io::Cursor::new(envelope);
        transfer::send_file(
            wh,
            relay_hints(),
            &mut cursor,
            send_name,
            size,
            transit::Abilities::ALL,
            |_info| {},
            |_sent, _total| {},
            std::future::pending(),
        )
        .await
        .map_err(|e| e.to_string())
    };
    match sent.await {
        Ok(()) => {
            println!("Sent.");
            0
        }
        Err(e) => {
            eprintln!("transcript send: {e}");
            1
        }
    }
}

async fn run_receive(rest: &[String]) -> i32 {
    let arg = crate::client_verbs::expand_eq(rest).join(" ");
    if arg.is_empty() {
        eprintln!("transcript receive: a wormhole code or a bundle path is required");
        return 2;
    }
    let envelope: Vec<u8> = if Path::new(&arg).is_file() {
        match std::fs::read(&arg) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("transcript receive: {}: {e}", arg);
                return 1;
            }
        }
    } else {
        let code: Code = match arg.parse() {
            Ok(code) => code,
            Err(_) => {
                eprintln!(
                    "transcript receive: {} is neither a bundle file nor a wormhole code",
                    arg
                );
                return 2;
            }
        };
        match receive_over_wormhole(code).await {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("transcript receive: {e}");
                return 1;
            }
        }
    };
    let (meta, transcript, origin) = match parse_envelope(&envelope) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("transcript receive: {e}");
            return 1;
        }
    };
    place_bundle(&meta, transcript, origin)
}

async fn receive_over_wormhole(code: Code) -> Result<Vec<u8>, String> {
    let mailbox = MailboxConnection::connect(transfer::APP_CONFIG, code, true)
        .await
        .map_err(|e| e.to_string())?;
    let wh = Wormhole::connect(mailbox)
        .await
        .map_err(|e| e.to_string())?;
    let request = transfer::request_file(
        wh,
        relay_hints(),
        transit::Abilities::ALL,
        std::future::pending(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let Some(request) = request else {
        return Err("the offer was cancelled".into());
    };
    // The bundle rides in memory: a session transcript is a few MB at most,
    // and the envelope parse wants the whole thing anyway.
    let mut bytes = Vec::new();
    request
        .accept(
            |_info| {},
            |_sent, _total| {},
            &mut bytes,
            std::future::pending(),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn place_bundle(meta: &BundleMeta, transcript: &[u8], origin: Option<SessionOrigin>) -> i32 {
    let Some(rel) = safe_rel(&meta.rel) else {
        eprintln!(
            "transcript receive: bundle names an unsafe transcript path {:?}",
            meta.rel
        );
        return 1;
    };
    let Some(root) = store_root(&meta.harness) else {
        eprintln!(
            "transcript receive: bundle names an unknown store {:?}",
            meta.harness
        );
        return 1;
    };
    let dest = root.join(&rel);
    if dest.exists() {
        eprintln!(
            "transcript receive: {} already exists; the session is already here",
            dest.display()
        );
        return 1;
    }
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("transcript receive: {}: {e}", parent.display());
            return 1;
        }
    }
    // The lexical check above cannot see a symlinked directory component
    // that already exists under the root, so canonicalize both sides and
    // compare; and create_new refuses a symlink at the final component,
    // which a plain write would follow.
    let canon_root = match root.canonicalize() {
        Ok(canon) => canon,
        Err(e) => {
            eprintln!("transcript receive: {}: {e}", root.display());
            return 1;
        }
    };
    let Some(parent) = dest.parent() else {
        eprintln!("transcript receive: {} has no parent", dest.display());
        return 1;
    };
    let canon_parent = match parent.canonicalize() {
        Ok(canon) => canon,
        Err(e) => {
            eprintln!("transcript receive: {}: {e}", parent.display());
            return 1;
        }
    };
    if !canon_parent.starts_with(&canon_root) {
        eprintln!(
            "transcript receive: {} escapes the {} store through a symlink",
            dest.display(),
            meta.harness
        );
        return 1;
    }
    if let Err(e) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
    {
        eprintln!("transcript receive: {}: {e}", dest.display());
        return 1;
    }
    if let Err(e) = std::fs::write(&dest, transcript) {
        eprintln!("transcript receive: {}: {e}", dest.display());
        return 1;
    }
    println!("unpacked: {}", dest.display());
    if let Some(origin) = origin.as_ref() {
        match write_record_beside(&dest, origin) {
            Ok(true) => println!(
                "origin: {}, {} session {}",
                origin.host, origin.harness, origin.session_id
            ),
            Ok(false) => {}
            Err(e) => eprintln!("transcript receive: origin record: {e}"),
        }
    }
    if !meta.branch.is_empty() {
        println!("branch on the sending machine: {}", meta.branch);
    }
    if meta.harness == "codex" {
        println!(
            "next: check out that branch here, then fno agents adopt {} and fno agents resume; a different harness goes through a handoff doc",
            meta.session_id
        );
    } else {
        println!(
            "next: check out that branch here, then claude --resume {} (same harness) or a handoff for a different one",
            meta.session_id
        );
    }
    0
}

/// `fno agents transcript send|receive` - move a session bundle over a
/// pairing code, or through a plain file. No daemon, no registry write.
pub async fn run_transcript(rest: &[String]) -> i32 {
    match rest.first().map(String::as_str) {
        Some("send") => run_send(&rest[1..]).await,
        Some("receive") => run_receive(&rest[1..]).await,
        _ => {
            eprintln!("usage: fno agents transcript send <session-id> [--file <path>]");
            eprintln!("       fno agents transcript receive <code|path>");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta(rel: &str) -> BundleMeta {
        BundleMeta {
            version: 1,
            harness: "claude".into(),
            session_id: "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d".into(),
            branch: "feature/x".into(),
            rel: rel.into(),
        }
    }

    #[test]
    fn envelope_round_trips_meta_transcript_and_origin() {
        let origin = SessionOrigin::for_this_machine(
            "claude",
            "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d",
            Path::new("/t/w.jsonl"),
        );
        let envelope =
            build_envelope(&sample_meta("-repo/w.jsonl"), b"line\n", Some(&origin)).unwrap();
        let (meta, transcript, origin) = parse_envelope(&envelope).unwrap();
        assert_eq!(meta.session_id, "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d");
        assert_eq!(meta.harness, "claude");
        assert_eq!(meta.rel, "-repo/w.jsonl");
        assert_eq!(meta.branch, "feature/x");
        assert_eq!(transcript, b"line\n");
        assert_eq!(
            origin.expect("origin rides the envelope").machine,
            crate::session_origin::this_machine()
        );
        // A foreign header and a truncated tail refuse; the originless
        // envelope still parses.
        assert!(parse_envelope(b"NOTFNOS").is_err());
        assert!(parse_envelope(&envelope[..envelope.len() - 3]).is_err());
        let (meta, transcript, origin) =
            parse_envelope(&build_envelope(&sample_meta("w.jsonl"), b"t", None).unwrap()).unwrap();
        assert_eq!(transcript, b"t");
        assert!(origin.is_none());
    }

    #[test]
    fn place_bundle_refuses_an_existing_transcript() {
        // The path rule refuses escape and absolute shapes outright.
        assert_eq!(safe_rel("../escape.jsonl"), None);
        assert_eq!(safe_rel("/abs/w.jsonl"), None);
        assert_eq!(safe_rel("a/../../b.jsonl"), None);
        assert_eq!(safe_rel(""), None);
        assert_eq!(
            safe_rel("-repo/w.jsonl"),
            Some(PathBuf::from("-repo/w.jsonl"))
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        std::fs::create_dir_all(root.join("-repo")).unwrap();
        std::fs::write(root.join("-repo/w.jsonl"), "here").unwrap();
        let saved = std::env::var_os(claude_drive::PROJECTS_DIR_ENV);
        std::env::set_var(claude_drive::PROJECTS_DIR_ENV, &root);
        let meta = sample_meta("-repo/w.jsonl");
        assert_eq!(place_bundle(&meta, b"new", None), 1);
        // The refusal wrote nothing: the local transcript is untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("-repo/w.jsonl")).unwrap(),
            "here"
        );
        match saved {
            Some(v) => std::env::set_var(claude_drive::PROJECTS_DIR_ENV, v),
            None => std::env::remove_var(claude_drive::PROJECTS_DIR_ENV),
        }
    }

    #[test]
    fn place_bundle_writes_transcript_and_origin_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let saved = std::env::var_os(claude_drive::PROJECTS_DIR_ENV);
        std::env::set_var(claude_drive::PROJECTS_DIR_ENV, &root);
        let meta = sample_meta("-repo/w.jsonl");
        let origin = SessionOrigin {
            machine: "aaaaaaaaaaaaaaaa".into(),
            host: "mac-a".into(),
            harness: "claude".into(),
            session_id: meta.session_id.clone(),
            transcript_path: "/gone/w.jsonl".into(),
            recorded_at: "2026-10-07T00:00:00+00:00".into(),
        };
        assert_eq!(place_bundle(&meta, b"fresh", Some(origin)), 0);
        let dest = root.join("-repo/w.jsonl");
        assert_eq!(std::fs::read(&dest).unwrap(), b"fresh");
        let record = root.join("-repo/3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d.fno.json");
        let placed: SessionOrigin =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(placed.host, "mac-a");
        match saved {
            Some(v) => std::env::set_var(claude_drive::PROJECTS_DIR_ENV, v),
            None => std::env::remove_var(claude_drive::PROJECTS_DIR_ENV),
        }
    }
}
