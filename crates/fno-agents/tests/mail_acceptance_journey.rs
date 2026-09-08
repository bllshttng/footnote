//! The keeper mail boundary, driven end to end (x-175a AC3-HP): a scripted
//! fake keeper speaks the real frame codec on a real unix socket, a fake pi
//! store materializes accepted-turn records, and the production
//! `deliver_via_keeper_socket_in` decides. The positive control is the exact
//! accepted turn - never a transport success, and never paint: draft echo,
//! fragmented frames and a same-prefix sibling must all stay unconfirmed.

use fno_agents::mail_inject::deliver_via_keeper_socket_in;
use fno_agents::pane_keeper::{decode, Decode, Frame};
use fno_agents::paths::AgentsHome;
use fno_agents::pi::encode_cwd;
use fno_agents::state::{update_registry, RegistryEntry};
use fno_agents::AgentStatus;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;

/// One scenario's script for the fake keeper: what to paint once the paste
/// lands, and whether the hosted harness accepts the turn after the wire CR
/// (into which session's transcript, after how long).
#[derive(Default)]
struct Script {
    /// `Output` payloads emitted as soon as the paste frame arrives: the TUI
    /// echoing the draft into the composer.
    paint: Vec<Vec<u8>>,
    /// After the CR: append the accepted turn to this session id's pi
    /// transcript (None = the harness never accepts within the budget).
    accept_session: Option<String>,
    /// How long after the CR the acceptance lands.
    accept_delay_ms: u64,
}

struct Rig {
    dir: PathBuf,
    home: AgentsHome,
    pi_root: PathBuf,
    cwd: PathBuf,
    sock: PathBuf,
}

fn rig(tag: &str) -> Rig {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("mail-journey-{}-{}-{n}", std::process::id(), tag));
    let home = AgentsHome::at(dir.join("agents"));
    home.ensure_root().unwrap();
    std::fs::create_dir_all(dir.join("mux").join("threads")).unwrap();
    let cwd = dir.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let pi_root = dir.join("pistore");
    std::fs::create_dir_all(pi_root.join(encode_cwd(&cwd))).unwrap();
    Rig {
        home,
        pi_root,
        cwd,
        sock: dir.join("mux/threads/wk.sock"),
        dir,
    }
}

fn row_for(rig: &Rig, harness: &str, session: &str) {
    update_registry(&rig.home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            name: "wk".into(),
            cwd: rig.cwd.to_string_lossy().into_owned(),
            harness: Some(harness.into()),
            harness_session_id: Some(session.into()),
            host_mode: Some("interactive".into()),
            messaging_socket_path: Some(rig.sock.to_string_lossy().into_owned()),
            status: AgentStatus::Live,
            pid: Some(4242),
            created_at: "2026-09-01T00:00:00Z".into(),
            ..Default::default()
        });
    })
    .unwrap();
}

/// Append one accepted-turn record carrying `text` to `session`'s pi
/// transcript, as the hosted harness would record a submitted turn.
fn record_turn(rig: &Rig, session: &str, text: &str) {
    let dir = rig.pi_root.join(encode_cwd(&rig.cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("20260901T000000Z_{session}.jsonl"));
    let line = serde_json::json!({ "text": text }).to_string();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    use std::io::Write;
    writeln!(f, "{line}").unwrap();
}

/// The fake keeper: binds `sock`, accepts the inject's connection, records
/// every decoded frame, and plays `script` (paint on paste, acceptance on
/// CR). Joining returns the frames in arrival order; an `Input` frame per
/// submit means the vector length IS the observed submit count.
fn spawn_keeper(rig: &Rig, text: &str, script: Script) -> std::thread::JoinHandle<Vec<Frame>> {
    let listener = UnixListener::bind(&rig.sock).unwrap();
    let text = text.to_string();
    let pi_root = rig.pi_root.clone();
    let cwd = rig.cwd.clone();
    std::thread::Builder::new()
        .name("fake-keeper".into())
        .spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return Vec::new();
            };
            let mut frames: Vec<Frame> = Vec::new();
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            'outer: loop {
                loop {
                    match decode(&buf) {
                        Decode::NeedMore => break,
                        Decode::Violation(_) => break 'outer,
                        Decode::Frame(frame, used) => {
                            buf.drain(..used);
                            let is_cr = matches!(&frame, Frame::Input(b) if b.as_slice() == b"\r");
                            frames.push(frame);
                            if matches!(&frames.last(), Some(Frame::Input(_))) && !is_cr {
                                // The paste: the TUI paints the draft.
                                use std::io::Write;
                                for payload in &script.paint {
                                    let _ = stream.write_all(&fno_agents::pane_keeper::encode(
                                        &Frame::Output(payload.clone()),
                                    ));
                                    let _ = stream.flush();
                                }
                            }
                            if is_cr {
                                // The submit: acceptance lands after its delay.
                                if let Some(sess) = &script.accept_session {
                                    std::thread::sleep(Duration::from_millis(
                                        script.accept_delay_ms,
                                    ));
                                    let dir = pi_root.join(encode_cwd(&cwd));
                                    std::fs::create_dir_all(&dir).unwrap();
                                    let line = serde_json::json!({ "text": text }).to_string();
                                    let file = dir.join(format!("20260901T000000Z_{sess}.jsonl"));
                                    let mut f = std::fs::OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open(file)
                                        .unwrap();
                                    use std::io::Write;
                                    writeln!(f, "{line}").unwrap();
                                }
                            }
                        }
                    }
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break 'outer,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            frames
        })
        .unwrap()
}

const ENVELOPE: &str =
    "<fno_mail from=\"a1b2c3d4\" id=\"msg-c3d4e5f6a7b8c91\">\nthe body\n</fno_mail>";

/// A sibling sharing ENVELOPE's first 48 characters but differing after.
fn same_prefix_envelope() -> String {
    let marker = ENVELOPE.lines().next().unwrap();
    let body = ENVELOPE.split_once('\n').unwrap().1;
    // Swap the id's final digit: the shared 48 chars survive, the line does not.
    format!("{}2>\n{body}", &marker[..marker.len() - 2])
}

fn submit_count(frames: &[Frame]) -> usize {
    frames
        .iter()
        .filter(|f| matches!(f, Frame::Input(_)))
        .count()
}

#[test]
fn accepted_turn_after_painted_draft_confirms_exactly_once() {
    // AC3-HP: the full envelope is painted as a draft BEFORE the submit and
    // the accepted record lands only AFTER the wire CR. The paint must not
    // confirm, the acceptance must, and the confirm must stop the retry
    // cadence at exactly paste + CR.
    let rig = rig("accepted");
    row_for(&rig, "pi", "sess-acc");
    let script = Script {
        paint: vec![ENVELOPE.as_bytes().to_vec()],
        accept_session: Some("sess-acc".into()),
        accept_delay_ms: 30,
    };
    let handle = spawn_keeper(&rig, ENVELOPE, script);
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-acc", ENVELOPE, 12, 25, 0);
    assert_eq!(outcome, Ok(()), "the accepted turn is the delivery proof");
    let frames = handle.join().unwrap();
    assert_eq!(submit_count(&frames), 2, "paste + CR, no extra submits");
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn painted_draft_without_acceptance_stays_unconfirmed() {
    // AC1-HP, draft half: the composer echoes the full message before any
    // submit. That paint never proves delivery, so the budget exhausts to
    // the honest unconfirmed.
    let rig = rig("draft");
    row_for(&rig, "cursor-agent", "sess-draft");
    let script = Script {
        paint: vec![ENVELOPE.as_bytes().to_vec()],
        ..Default::default()
    };
    let handle = spawn_keeper(&rig, ENVELOPE, script);
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-draft", ENVELOPE, 4, 20, 0);
    assert_eq!(outcome, Err("not-confirmed"));
    assert_eq!(submit_count(&handle.join().unwrap()), 2);
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn fragmented_paint_stays_unconfirmed() {
    // AC1-HP, framing half: the same draft split across `Output` frames must
    // not assemble into a confirmation either.
    let rig = rig("frag");
    row_for(&rig, "cursor-agent", "sess-frag");
    let bytes = ENVELOPE.as_bytes();
    let (a, rest) = bytes.split_at(bytes.len() / 3);
    let (b, c) = rest.split_at(rest.len() / 2);
    let script = Script {
        paint: vec![a.to_vec(), b.to_vec(), c.to_vec()],
        ..Default::default()
    };
    let handle = spawn_keeper(&rig, ENVELOPE, script);
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-frag", ENVELOPE, 4, 20, 0);
    assert_eq!(outcome, Err("not-confirmed"));
    assert_eq!(submit_count(&handle.join().unwrap()), 2);
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn same_prefix_paint_stays_unconfirmed() {
    // AC1-HP, sibling half: a WRONG message sharing the first 48 characters
    // (the retired matcher's whole needle) never proves THIS message landed.
    let rig = rig("prefix");
    row_for(&rig, "cursor-agent", "sess-prefix");
    let other = same_prefix_envelope();
    assert_eq!(
        ENVELOPE.chars().take(48).collect::<String>(),
        other.chars().take(48).collect::<String>(),
        "the scenario requires a shared 48-character prefix"
    );
    let script = Script {
        paint: vec![other.into_bytes()],
        ..Default::default()
    };
    let handle = spawn_keeper(&rig, ENVELOPE, script);
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-prefix", ENVELOPE, 4, 20, 0);
    assert_eq!(outcome, Err("not-confirmed"));
    assert_eq!(submit_count(&handle.join().unwrap()), 2);
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn stale_accepted_record_before_the_send_stays_unconfirmed() {
    // AC2-HP, stale half: a PREVIOUS attempt's accepted record sits in the
    // transcript before this attempt's baseline. The captured boundary
    // keeps the retry from reading the old acceptance as its own delivery.
    let rig = rig("stale");
    row_for(&rig, "pi", "sess-stale");
    record_turn(&rig, "sess-stale", ENVELOPE);
    let handle = spawn_keeper(&rig, ENVELOPE, Script::default());
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-stale", ENVELOPE, 4, 20, 0);
    assert_eq!(outcome, Err("not-confirmed"));
    assert_eq!(submit_count(&handle.join().unwrap()), 2);
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn newly_accepted_record_confirms_past_an_existing_baseline() {
    // AC1-EDGE: with an existing store (a real baseline, not zero), the
    // accepted record that lands AFTER the send confirms exactly this
    // message. This is the suite's positive control: the confirm
    // instrument can actually fire.
    let rig = rig("fresh");
    row_for(&rig, "pi", "sess-fresh");
    record_turn(&rig, "sess-fresh", "an unrelated earlier turn");
    let script = Script {
        accept_session: Some("sess-fresh".into()),
        accept_delay_ms: 20,
        ..Default::default()
    };
    let handle = spawn_keeper(&rig, ENVELOPE, script);
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-fresh", ENVELOPE, 12, 25, 0);
    assert_eq!(outcome, Ok(()));
    assert_eq!(submit_count(&handle.join().unwrap()), 2);
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn wrong_session_record_stays_unconfirmed() {
    // The intended-worker-session half of AC1-EDGE: the acceptance lands in
    // a DIFFERENT session's transcript, so this session's confirm reads
    // nothing and stays unconfirmed.
    let rig = rig("wrongsess");
    row_for(&rig, "pi", "sess-mine");
    let script = Script {
        accept_session: Some("sess-other".into()),
        accept_delay_ms: 20,
        ..Default::default()
    };
    let handle = spawn_keeper(&rig, ENVELOPE, script);
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-mine", ENVELOPE, 4, 20, 0);
    assert_eq!(outcome, Err("not-confirmed"));
    assert_eq!(submit_count(&handle.join().unwrap()), 2);
    std::fs::remove_dir_all(&rig.dir).ok();
}

#[test]
fn unavailable_reader_refuses_before_typing() {
    // AC2-HP, unavailable half: a duplicated session store refuses the
    // confirm before a single frame is typed - no submits, honest reason.
    let rig = rig("dup");
    row_for(&rig, "pi", "sess-dup");
    record_turn(&rig, "sess-dup", "first store copy");
    // A second store file for the SAME id: pi's duplicate refusal.
    let dir = rig.pi_root.join(encode_cwd(&rig.cwd));
    std::fs::write(dir.join("20260902T000000Z_sess-dup.jsonl"), "").unwrap();
    let handle = spawn_keeper(&rig, ENVELOPE, Script::default());
    let outcome =
        deliver_via_keeper_socket_in(&rig.home, &rig.pi_root, "sess-dup", ENVELOPE, 4, 20, 0);
    assert_eq!(outcome, Err("duplicate-session-store"));
    assert_eq!(submit_count(&handle.join().unwrap()), 0);
    std::fs::remove_dir_all(&rig.dir).ok();
}
