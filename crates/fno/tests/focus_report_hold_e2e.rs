//! The view-switch DND repro (x-5198) on the real client path. A DEC 1004
//! focus-in report arriving on a real client's TTY routes to the focused
//! pane as `CoreMsg::Input`; before the reply classifier it armed the pane
//! session's attended mail hold, so merely viewing a row marked it DND and
//! held its mail. The contract here: the report arms nothing, and a typed
//! keystroke on the same client still arms.

mod common;

use common::{screen_has_line, worker_bin, ClientHarness, Scratch};

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn agents_home(scratch: &Scratch) -> PathBuf {
    scratch.0.join("iso-agents")
}

fn hold_dir(scratch: &Scratch) -> PathBuf {
    agents_home(scratch).join("mail-hold")
}

const HOLD_SID: &str = "eeeeeeee-1111-2222-3333-444455556666";

fn hold_clocks(scratch: &Scratch) -> Vec<PathBuf> {
    let mut clocks: Vec<PathBuf> = std::fs::read_dir(hold_dir(scratch))
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    clocks.sort();
    clocks
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
fn a_focus_report_on_the_real_client_arms_nothing_and_a_conversation_arms() {
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
    // The probe is itself a Message line, so a slash line resets after it.
    h.type_bytes(b"echo dnd-probe-ok\r");
    h.wait_screen(15, |s| screen_has_line(s, "dnd-probe-ok"));
    h.type_bytes(b"/x\r");
    std::thread::sleep(Duration::from_millis(500));

    // The view-switch byte: the DEC 1004 focus-in report a real terminal
    // emits when a row gains the view, which the client forwards untouched.
    h.type_bytes(b"\x1b[I");

    // No clock may appear. Poll the window so a slow mail-hold spawn cannot
    // sneak past a fixed sleep and read as a pass.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let clocks = hold_clocks(&scratch);
        assert!(
            clocks.is_empty(),
            "a focus-in report armed the attended hold: {clocks:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // One real message still arms nothing (the R2 two-message rule).
    h.type_bytes(b"hello\r");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let clocks = hold_clocks(&scratch);
        assert!(
            clocks.is_empty(),
            "one real message armed the hold before a conversation: {clocks:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // The control: a second consecutive real message on the same client
    // DOES arm the same pane's session - the path works end to end.
    h.type_bytes(b"world\r");
    let deadline = Instant::now() + Duration::from_secs(20);
    while hold_clocks(&scratch).is_empty() {
        assert!(
            Instant::now() < deadline,
            "two real messages never armed the hold"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    assert_eq!(
        hold_clocks(&scratch),
        vec![hold_dir(&scratch).join(format!("{HOLD_SID}.json"))],
        "the conversation armed exactly its own session's clock"
    );

    // SHELL=/bin/sh panes are keeper-ineligible, so the plain kill-server in
    // Scratch::drop refuses; end the unkept shells before the drop.
    drop(h);
    let _ = scratch
        .command()
        .args(["mux", "kill-server", "main", "--end-unkept"])
        .output();
}
