//! The opencode archive op and its retirement-lane arm.
//!
//! The fake serve here MODELS the archive state: a PATCH that stores the
//! stamp and a GET that reports it are two different behaviors, and the tests
//! that matter (`Archived` vs `Survived`) differ only in whether the store
//! kept the write. A fake that always echoed success would pass both and
//! prove nothing.

use super::*;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

/// How the fake serve behaves for one test.
#[derive(Clone)]
struct ServeShape {
    /// The version reported by `/global/health`.
    version: String,
    /// False: `/session/{id}` answers 404.
    exists: bool,
    /// False: a PATCH is accepted and the stamp is never stored (the
    /// survives-a-successful-write shape).
    persist: bool,
    /// The stamp the session already carries when the test starts.
    archived_at: Option<u64>,
}

impl Default for ServeShape {
    fn default() -> Self {
        ServeShape {
            version: "1.14.50".to_string(),
            exists: true,
            persist: true,
            archived_at: None,
        }
    }
}

struct ArchiveServe {
    addr: std::net::SocketAddr,
    /// Every request line the serve saw, in order.
    requests: Arc<Mutex<Vec<String>>>,
}

impl ArchiveServe {
    fn start(shape: ServeShape) -> ArchiveServe {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let stored = Arc::new(Mutex::new(shape.archived_at));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
                let mut req = String::new();
                let mut buf = [0u8; 8192];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => req.push_str(&String::from_utf8_lossy(&buf[..n])),
                        Err(_) => break,
                    }
                }
                let line = req.lines().next().unwrap_or("").to_string();
                seen.lock().unwrap().push(line.clone());
                let session_body = || {
                    let time = match *stored.lock().unwrap() {
                        Some(at) => format!(r#"{{"created":1,"updated":2,"archived":{at}}}"#),
                        None => r#"{"created":1,"updated":2}"#.to_string(),
                    };
                    format!(r#"{{"id":"ses_probe","time":{time}}}"#)
                };
                let (status, body) = if line.starts_with("GET /global/health") {
                    (
                        "200 OK",
                        format!(
                            r#"{{"healthy":true,"version":"{}"}}"#,
                            shape.version.clone()
                        ),
                    )
                } else if !shape.exists {
                    ("404 Not Found", "{}".to_string())
                } else if line.starts_with("PATCH /session/") {
                    if shape.persist {
                        *stored.lock().unwrap() = Some(1_788_909_086_102);
                    }
                    ("200 OK", session_body())
                } else if line.starts_with("GET /session/") {
                    ("200 OK", session_body())
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
        });
        ArchiveServe { addr, requests }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn patches(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("PATCH "))
            .count()
    }
}

// --- the version bound ------------------------------------------------------

#[test]
fn version_at_least_answers_the_measured_bound_and_refuses_what_it_cannot_read() {
    let min = ARCHIVE_MIN_VERSION;
    assert!(version_at_least("1.14.50", min));
    assert!(version_at_least("v1.14.50", min));
    assert!(version_at_least("1.15.0", min));
    assert!(version_at_least("2.0.0", min));
    assert!(!version_at_least("1.14.49", min));
    assert!(!version_at_least("1.13.99", min));
    assert!(!version_at_least("0.99.99", min));
    // An unreadable version is not evidence of a supported one.
    assert!(!version_at_least("", min));
    assert!(!version_at_least("1.14", min));
    assert!(!version_at_least("nightly", min));
    assert!(!version_at_least("1.x.50", min));
}

// --- the archive op ---------------------------------------------------------

#[test]
fn archive_stamps_the_session_and_reads_the_stored_record_back() {
    let serve = ArchiveServe::start(ServeShape::default());
    let outcome = archive_session(&serve.base_url(), "tok", "ses_probe").unwrap();
    assert_eq!(outcome, ArchiveOutcome::Archived);
    assert_eq!(serve.patches(), 1);
    // GET, PATCH, GET: the readback is a separate read of the stored record,
    // never the PATCH's own echo.
    let lines = serve.requests.lock().unwrap().clone();
    assert_eq!(lines.len(), 3, "saw {lines:?}");
    assert!(lines[0].starts_with("GET /session/ses_probe"));
    assert!(lines[1].starts_with("PATCH /session/ses_probe"));
    assert!(lines[2].starts_with("GET /session/ses_probe"));
}

#[test]
fn an_already_archived_session_is_not_written_to() {
    let serve = ArchiveServe::start(ServeShape {
        archived_at: Some(1_788_000_000_000),
        ..Default::default()
    });
    let outcome = archive_session(&serve.base_url(), "tok", "ses_probe").unwrap();
    assert_eq!(outcome, ArchiveOutcome::AlreadyArchived);
    assert_eq!(serve.patches(), 0, "an archived session needs no write");
}

#[test]
fn a_session_absent_from_the_store_is_gone() {
    let serve = ArchiveServe::start(ServeShape {
        exists: false,
        ..Default::default()
    });
    let outcome = archive_session(&serve.base_url(), "tok", "ses_probe").unwrap();
    assert_eq!(outcome, ArchiveOutcome::Gone);
    assert_eq!(serve.patches(), 0);
}

#[test]
fn an_accepted_write_the_store_did_not_keep_answers_survived() {
    let serve = ArchiveServe::start(ServeShape {
        persist: false,
        ..Default::default()
    });
    let outcome = archive_session(&serve.base_url(), "tok", "ses_probe").unwrap();
    assert_eq!(
        outcome,
        ArchiveOutcome::Survived,
        "a 200 on the PATCH is the server's own projection, not the stored record"
    );
    assert_eq!(serve.patches(), 1);
}

#[test]
fn an_unreachable_serve_is_an_error_not_an_outcome() {
    // Bind and drop, so the port is closed and nothing answers.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    let err = archive_session(&format!("http://{addr}"), "tok", "ses_probe").unwrap_err();
    assert!(!err.is_empty());
}

// --- the capability gate ----------------------------------------------------

fn write_serve_state(home: &AgentsHome, base_url: &str) {
    std::fs::create_dir_all(home.root()).unwrap();
    std::fs::write(
        serve_state_path(home),
        format!(r#"{{"base_url":"{base_url}","token":"tok","pid":4242,"pid_start":7}}"#),
    )
    .unwrap();
}

#[test]
fn a_serve_at_the_bound_is_archive_capable() {
    let serve = ArchiveServe::start(ServeShape::default());
    let dir = tempfile::tempdir().unwrap();
    let home = AgentsHome::at(dir.path());
    write_serve_state(&home, &serve.base_url());
    let handle = archive_capable_serve(&home).expect("1.14.50 carries the archive op");
    assert_eq!(handle.token, "tok");
    assert_eq!(handle.pid, 4242);
}

#[test]
fn a_serve_below_the_bound_is_not_archive_capable() {
    let serve = ArchiveServe::start(ServeShape {
        version: "1.14.49".to_string(),
        ..Default::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let home = AgentsHome::at(dir.path());
    write_serve_state(&home, &serve.base_url());
    assert!(archive_capable_serve(&home).is_none());
}

#[test]
fn a_missing_serve_state_is_not_archive_capable() {
    let dir = tempfile::tempdir().unwrap();
    let home = AgentsHome::at(dir.path());
    assert!(archive_capable_serve(&home).is_none());
}

// --- the retirement-lane arm ------------------------------------------------

fn never_called(_: &str, _: &str, _: &str) -> Result<ArchiveOutcome, String> {
    panic!("the archive op must not be reached on a skip");
}

#[test]
fn a_row_without_a_session_id_skips_without_touching_the_serve() {
    use crate::gc_native::opencode_archive_outcome;
    let serve = Some(("http://127.0.0.1:1".to_string(), "tok".to_string()));
    assert_eq!(
        opencode_archive_outcome(None, serve.clone(), &never_called),
        crate::daemon::CascadeOutcome::NotApplicable
    );
    assert_eq!(
        opencode_archive_outcome(Some(""), serve, &never_called),
        crate::daemon::CascadeOutcome::NotApplicable
    );
}

#[test]
fn no_archive_capable_serve_skips_and_still_satisfies_the_applied_gate() {
    use crate::gc_native::opencode_archive_outcome;
    let outcome = opencode_archive_outcome(Some("ses_probe"), None, &never_called);
    assert_eq!(outcome, crate::daemon::CascadeOutcome::NotApplicable);
    assert!(
        outcome.satisfies_applied(),
        "a machine with no serve must not hold every opencode row forever"
    );
}

#[test]
fn each_archive_outcome_maps_to_its_effect_word() {
    use crate::daemon::CascadeOutcome;
    use crate::gc_native::opencode_archive_outcome;
    let serve = || Some(("http://127.0.0.1:1".to_string(), "tok".to_string()));
    let run = |result: Result<ArchiveOutcome, String>| {
        opencode_archive_outcome(Some("ses_probe"), serve(), &move |_, _, _| result.clone())
    };

    let archived = run(Ok(ArchiveOutcome::Archived));
    assert_eq!(archived, CascadeOutcome::Removed);
    assert!(archived.satisfies_applied());

    let already = run(Ok(ArchiveOutcome::AlreadyArchived));
    assert_eq!(already.as_str(), "confirmed-already-absent");
    assert!(already.satisfies_applied());

    let gone = run(Ok(ArchiveOutcome::Gone));
    assert_eq!(gone.as_str(), "confirmed-already-absent");

    let survived = run(Ok(ArchiveOutcome::Survived));
    assert_eq!(survived.as_str(), "failed");
    assert!(
        !survived.satisfies_applied(),
        "an unstored write must hold the row"
    );

    let unverified = run(Err("connect refused".to_string()));
    assert_eq!(unverified.as_str(), "kept");
    assert!(!unverified.satisfies_applied());
    assert_eq!(unverified.detail().as_deref(), Some("connect refused"));
}

// --- the live re-verification probe -----------------------------------------

/// Re-measure [`ARCHIVE_MIN_VERSION`] against a real opencode serve. The
/// version bound is a claim about a vendor binary, and only a live run can
/// renew it. Ignored by default because it needs a serve:
///
/// ```text
/// OPENCODE_SERVER_PASSWORD=tok opencode serve --port 45999 --hostname 127.0.0.1 &
/// FNO_OPENCODE_LIVE_URL=http://127.0.0.1:45999 FNO_OPENCODE_LIVE_TOKEN=tok \
///   cargo test -p fno-agents --lib live_opencode -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn live_opencode_serve_carries_the_archive_op() {
    let base_url = std::env::var("FNO_OPENCODE_LIVE_URL")
        .expect("set FNO_OPENCODE_LIVE_URL and FNO_OPENCODE_LIVE_TOKEN");
    let token = std::env::var("FNO_OPENCODE_LIVE_TOKEN").unwrap_or_default();
    let (status, body) = http_json(
        &base_url,
        "GET",
        "/global/health",
        None,
        Some(&token),
        Duration::from_secs(10),
    )
    .expect("health read");
    assert_eq!(status, 200, "{body}");
    let health: serde_json::Value = serde_json::from_str(&body).unwrap();
    let version = health["version"]
        .as_str()
        .expect("health carries a version");
    println!("live opencode version: {version}");
    assert!(
        version_at_least(version, ARCHIVE_MIN_VERSION),
        "live serve reports {version}, below the measured bound {ARCHIVE_MIN_VERSION:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = encode_query_path(dir.path().to_str().unwrap());
    let (status, body) = http_json(
        &base_url,
        "POST",
        &format!("/session?directory={path}"),
        Some(&serde_json::json!({"title": "fno archive probe"})),
        Some(&token),
        Duration::from_secs(60),
    )
    .expect("session create");
    assert_eq!(status, 200, "{body}");
    let session: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sid = session["id"].as_str().expect("created session id");

    assert_eq!(
        archive_session(&base_url, &token, sid).expect("archive"),
        ArchiveOutcome::Archived
    );
    assert_eq!(
        archive_session(&base_url, &token, sid).expect("second archive"),
        ArchiveOutcome::AlreadyArchived,
        "the archive op is idempotent"
    );
    // History preserved: the record is still readable after the archive.
    let (status, body) = http_json(
        &base_url,
        "GET",
        &format!("/session/{sid}"),
        None,
        Some(&token),
        Duration::from_secs(10),
    )
    .expect("post-archive read");
    assert_eq!(status, 200, "an archived session is still readable: {body}");
}
