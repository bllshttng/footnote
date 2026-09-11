//! Units for the timed-out event wait's verdict builder: three
//! daemon.stderr inputs, three different verdicts, so the helper discriminates
//! rather than merely emitting words.

mod common;

use std::path::PathBuf;
use std::time::Duration;

fn workdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fnoav{}_{}", std::process::id(), label));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn journal_with_lines(dir: &PathBuf, lines: usize) -> PathBuf {
    let path = dir.join("events.jsonl");
    let body = (0..lines)
        .map(|i| format!(r#"{{"type":"daemon_started","i":{i}}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    let body = if lines == 0 { body } else { body + "\n" };
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn blocked_writer_names_the_lock_and_the_count() {
    let dir = workdir("blocked");
    let stderr = dir.join("daemon.stderr");
    std::fs::write(
        &stderr,
        "claims: failed to emit \"claim_acquired\": events.jsonl lock timeout: /x/y.lock.d\n\
         claims: failed to emit \"single_flight_gate\": events.jsonl lock timeout: /x/y.lock.d\n\
         claims: failed to emit \"claim_released\": events.jsonl lock timeout: /x/y.lock.d\n",
    )
    .unwrap();
    let journal = journal_with_lines(&dir, 2);
    let v = common::absence_verdict(
        &journal,
        &stderr,
        "startup_reconcile_done",
        Duration::from_secs(30),
    );
    assert!(v.contains("the writer could not write"), "{v}");
    assert!(v.contains("3 emit failure(s)"), "{v}");
    assert!(v.contains("/x/y.lock.d"), "{v}");
    assert!(v.contains("lock path: /x/y.lock.d"), "{v}");
    assert!(v.contains(&journal.display().to_string()), "{v}");
    assert!(v.contains("(2 lines)"), "{v}");
    assert!(v.contains("never appeared within 30s"), "{v}");
}

#[test]
fn empty_stderr_means_the_event_did_not_occur() {
    let dir = workdir("empty");
    let stderr = dir.join("daemon.stderr");
    std::fs::write(&stderr, "").unwrap();
    let journal = journal_with_lines(&dir, 0);
    let v = common::absence_verdict(
        &journal,
        &stderr,
        "startup_reconcile_done",
        Duration::from_secs(30),
    );
    assert!(
        v.contains("no emit failure was recorded; the event did not occur"),
        "{v}"
    );
    assert!(!v.contains("the writer could not write"), "{v}");
    assert!(v.contains("(0 lines)"), "{v}");
}

#[test]
fn missing_stderr_capture_refuses_to_verdict() {
    let dir = workdir("missing");
    let stderr = dir.join("daemon.stderr");
    let journal = journal_with_lines(&dir, 1);
    let v = common::absence_verdict(
        &journal,
        &stderr,
        "startup_reconcile_done",
        Duration::from_secs(30),
    );
    assert!(v.contains("capture file was missing"), "{v}");
    assert!(!v.contains("the event did not occur"), "{v}");
    assert!(!v.contains("the writer could not write"), "{v}");
}
