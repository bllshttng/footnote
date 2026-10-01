//! The conversation-hold witness repro on the real client path. A DEC 1004
//! focus-in report arriving on a real client's TTY routes to the focused
//! pane as `CoreMsg::Input`; the reply classifier must read it as
//! terminal-generated loopback, so merely viewing a row writes no
//! `operator_submit` witness. The contract here: the report witnesses
//! nothing, and a typed line with Enter writes exactly one witness row
//! bound to the focused pane's registry session - the row the prompt hook
//! joins to arm the conversation hold.

mod common;

use common::{screen_has_line, worker_bin, ClientHarness, Scratch};

use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, Instant};

fn agents_home(scratch: &Scratch) -> PathBuf {
    scratch.0.join("iso-agents")
}

const HOLD_SID: &str = "eeeeeeee-1111-2222-3333-444455556666";

/// The `operator_submit` rows the mux committed to the agents journal's store.
fn submits(scratch: &Scratch) -> Vec<serde_json::Value> {
    fno::event_store::query_events(
        &agents_home(scratch).join("events.jsonl"),
        &fno::event_store::EventQuery::of_types(&["operator_submit"]),
    )
    .unwrap_or_default()
    .iter()
    .filter_map(|row| serde_json::from_str(&row.line).ok())
    .collect()
}

fn pane(scratch: &Scratch, args: &[&str]) -> Output {
    scratch
        .command()
        .args(["mux", "pane"])
        .args(args)
        .env("FNO_AGENTS_WORKER_BIN", worker_bin())
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Write the registry file the server's reader parses, bound to the session
/// the harness client attaches to (`main`) and the pane it focuses. Minimal
/// row: the reader needs name/cwd/status/mux, not the whole schema.
fn write_registry(scratch: &Scratch, pane_id: u64) {
    let home = agents_home(scratch);
    std::fs::create_dir_all(&home).unwrap();
    let row = format!(
        r#"{{"name":"dnd-row","harness":"claude","cwd":"{}","status":"live",
             "created_at":"2026-09-26T00:00:00Z",
             "harness_session_id":"{HOLD_SID}",
             "mux":{{"session":"main","pane_id":{pane_id}}}}}"#,
        scratch.0.join("home").to_string_lossy(),
    );
    let tmp = home.join("registry.json.tmp");
    std::fs::write(
        &tmp,
        format!(r#"{{"schema_version": 36, "agents": [{row}]}}"#),
    )
    .unwrap();
    std::fs::rename(tmp, home.join("registry.json")).unwrap();
}

#[test]
fn a_focus_report_writes_no_witness_and_a_typed_enter_writes_one() {
    let scratch = Scratch::new("dnd-view");
    let dir = scratch.0.to_str().unwrap().to_string();

    // The pane the client will focus: an interactive shell, so an echo
    // probe can prove typed input lands on the focused pane.
    let run = pane(&scratch, &["run", "--cwd", &dir, "--", "/bin/sh"]);
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let pane_id: u64 = stdout(&run).parse().expect("machine-readable pane id");

    write_registry(&scratch, pane_id);

    let mut h = ClientHarness::spawn_session(&scratch, "main");
    h.wait_screen(15, |s| !s.trim().is_empty());

    // Focus it now that a client is attached: the focus verb moves the
    // attached client's view, and its keystrokes route to this pane after.
    // The verb refuses before the attach registers, so retry the refusal.
    let mut focused = String::new();
    for _ in 0..20 {
        let focus = pane(&scratch, &["focus", &pane_id.to_string()]);
        if focus.status.success() {
            focused = String::from_utf8_lossy(&focus.stdout).to_string();
            break;
        }
        focused = String::from_utf8_lossy(&focus.stderr).to_string();
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        !focused.is_empty() && !focused.contains("no attached client"),
        "the focus verb never moved the attached client: {focused}"
    );
    std::thread::sleep(Duration::from_millis(1500));

    // Probe: typed input must land on the focused pane (echo renders there).
    // The probe is itself a typed Enter, so it writes the baseline witness
    // row the assertions below count from.
    h.type_bytes(b"echo dnd-probe-ok\r");
    h.wait_screen(15, |s| screen_has_line(s, "dnd-probe-ok"));
    let deadline = Instant::now() + Duration::from_secs(15);
    let baseline = loop {
        let rows = submits(&scratch);
        if !rows.is_empty() {
            break rows.len();
        }
        assert!(
            Instant::now() < deadline,
            "the typed probe never wrote a witness row"
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    // The view-switch byte: the DEC 1004 focus-in report a real terminal
    // emits when a row gains the view, which the client forwards untouched.
    h.type_bytes(b"\x1b[I");

    // No witness row may appear. Poll the window so a slow append cannot
    // sneak past a fixed sleep and read as a pass.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        assert_eq!(
            submits(&scratch).len(),
            baseline,
            "a focus-in report wrote an operator_submit row: {:?}",
            submits(&scratch)
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // One real message: exactly one NEW witness row, bound to the focused
    // pane's registry session (via pane, resolution ok, its harness_session).
    h.type_bytes(b"hello\r");
    let deadline = Instant::now() + Duration::from_secs(20);
    let rows = loop {
        let rows = submits(&scratch);
        if rows.len() > baseline {
            break rows;
        }
        assert!(
            Instant::now() < deadline,
            "a typed Enter never wrote the witness row"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(
        rows.len(),
        baseline + 1,
        "one Enter writes exactly one witness row: {rows:?}"
    );
    let data = &rows.last().unwrap()["data"];
    assert_eq!(data["via"], "pane");
    assert_eq!(data["resolution"], "ok");
    assert_eq!(
        data["harness_session"], HOLD_SID,
        "the row binds the pane to the registry row's session"
    );

    // SHELL=/bin/sh panes are keeper-ineligible, so the plain kill-server in
    // Scratch::drop refuses; end the unkept shells before the drop.
    drop(h);
    let _ = scratch
        .command()
        .args(["mux", "kill-server", "main", "--end-unkept"])
        .output();
}
