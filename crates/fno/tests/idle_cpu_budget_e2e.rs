//! x-926c idle CPU budget: a server with one attached, silent client must not
//! burn CPU re-deriving unchanged inputs. Seeds a registry (20 rows), a
//! two-segment journal (~16 MB) and a graph (~15 MB), attaches, waits for the
//! positive markers that prove both sideline readers ran, then measures the
//! server's CPU seconds across a 20 s idle window. Budget: 1.0 s (5 percent
//! of one core).
//!
//! Positive markers come FIRST (AC1-EDGE): a server whose readers never ran
//! would trivially pass the budget, so a marker that never arrives panics
//! before any measurement. On the unmodified base this test FAILS with a
//! delta above 2.0 s (AC1-ERR) - that red run is the repro.

mod common;

use common::{process_cpu_secs, spawn_server, FakeClient, Scratch};

use std::time::Duration;

const REGISTRY_ROWS: usize = 20;
const JOURNAL_SEGMENT_LINES: usize = 10_000;
const GRAPH_NODES: usize = 6_000;
const DETAILS_BYTES: usize = 2_000;
const BUDGET_SECS: f64 = 1.0;

/// `YYYY-MM-DDTHH:MM:SSZ` at `secs`, the one shape `rfc3339_like_to_secs` reads.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm), the inverse of the
    // reader's days-from-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn now_rfc3339() -> String {
    rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
}

/// The registry the row reader parses, written atomically (tmp + rename) so
/// the reader never sees a torn seed.
fn seed_registry(dir: &std::path::Path) {
    let now = now_rfc3339();
    let mut rows = String::new();
    for i in 0..REGISTRY_ROWS {
        if i > 0 {
            rows.push(',');
        }
        rows.push_str(&format!(
            r#"{{"name":"w-{i:02}","cwd":"/tmp","status":"running","liveness":"alive","liveness_measured_at":"{now}","harness":"claude","short_id":"s-{i:02}"}}"#
        ));
    }
    std::fs::write(
        dir.join("registry.json.tmp"),
        format!(r#"{{"schema_version": 6, "agents": [{rows}]}}"#),
    )
    .unwrap();
    std::fs::rename(dir.join("registry.json.tmp"), dir.join("registry.json")).unwrap();
}

/// One journal line the parser must skip (none of the 4 handled types).
fn noise_line(kind: &str, i: usize) -> String {
    let filler = "x".repeat(600);
    format!(
        r#"{{"type":"{kind}","ts":"2026-09-11T00:00:00Z","data":{{"seq":{i},"payload":"{filler}"}}}}"#
    )
}

/// `agent_spawned` / `agent_removed` pairs naming registry row `i`.
fn lifecycle_lines(i: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        r#"{{"type":"agent_spawned","ts":"2026-09-11T00:00:00Z","data":{{"name":"w-{i:02}","provider":"claude","harness_session_id":"sess-{i:02}","cwd":"/tmp","substrate":"thread"}}}}"#
    ));
    out.push('\n');
    out.push_str(&format!(
        r#"{{"type":"agent_removed","ts":"2026-09-11T00:00:05Z","data":{{"name":"w-{i:02}","harness":"claude","harness_session_id":"sess-{i:02}"}}}}"#
    ));
    out.push('\n');
    out
}

/// A two-segment journal: mostly noise the parser must discard, plus ~600
/// lifecycle rows naming the seeded registry workers. Mirrors the live
/// shape the defect was measured on (2.6 percent handled lines).
fn seed_journal_segment(path: &std::path::Path, first: usize) -> u64 {
    let mut body = String::with_capacity(9 << 20);
    for i in 0..JOURNAL_SEGMENT_LINES {
        let line = if i % 3333 == 0 {
            lifecycle_lines((first + i) % REGISTRY_ROWS)
        } else if i % 2 == 0 {
            noise_line("inside_leg_report", i)
        } else {
            noise_line("control_plane_tick", i)
        };
        body.push_str(&line);
        body.push('\n');
    }
    std::fs::write(path, &body).unwrap();
    body.len() as u64
}

/// The graph the backlog reader derives: ~6,000 nodes (ready becomes cards,
/// idea/done do not), each carrying a long details string, all project fno.
fn seed_graph(path: &std::path::Path) -> u64 {
    let details = "d".repeat(DETAILS_BYTES);
    let mut body = String::with_capacity(16 << 20);
    body.push_str(r#"{"schema_version": 1, "entries": ["#);
    for i in 0..GRAPH_NODES {
        if i > 0 {
            body.push(',');
        }
        let status = match i % 3 {
            0 => "ready",
            1 => "idea",
            _ => "done",
        };
        body.push_str(&format!(
            r#"{{"id":"x-t{i:04}","slug":"node-{i:04}","status":"{status}","project":"fno","priority":"p2","created_at":"2026-09-01T00:00:00Z","details":"{details}"}}"#
        ));
    }
    body.push_str("]}");
    std::fs::write(path, &body).unwrap();
    body.len() as u64
}

#[test]
fn attached_idle_server_stays_under_the_cpu_budget() {
    let scratch = Scratch::new("idle_cpu_budget");
    let iso = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&iso).unwrap();
    seed_registry(&iso);
    let seg1 = seed_journal_segment(&iso.join("events.jsonl.1"), 0);
    let seg0 = seed_journal_segment(&iso.join("events.jsonl"), JOURNAL_SEGMENT_LINES);
    let graph = seed_graph(&scratch.0.join("iso-graph.json"));

    // spawn_server already points FNO_AGENTS_HOME at iso-agents and
    // FNO_GRAPH_JSON at iso-graph.json under the socket dir. No
    // FNO_BOARD_SCOPE is set, so board_scope_from_spawn_env answers All and
    // every seeded project-fno card stays on the board.
    let sock = scratch.0.join("work.sock");
    let server = spawn_server(&sock, &[]);
    let pid = server.0.id();

    let mut client = FakeClient::attach(&sock, 50, 200, "/tmp");

    // Positive markers before ANY measurement (AC1-EDGE): the row reader must
    // have published a seeded row, and the backlog reader must have derived
    // cards. A wait that times out panes the test here, never passes it.
    client.wait(
        20,
        "seeded registry row w-00 in layout.agents (row reader never ran)",
        |c| {
            c.layout
                .as_ref()
                .map(|l| l.agents.iter().any(|a| a.name == "w-00"))
        },
    );
    client.wait(
        20,
        "non-empty layout backlog (backlog reader never ran)",
        |c| c.layout.as_ref().map(|l| !l.backlog.is_empty()),
    );

    // Warm-up: let both readers settle onto their cached stamps so the
    // measured window is the steady state, not first contact.
    client.pump(Duration::from_secs(5));
    let cpu_before = process_cpu_secs(pid);
    client.pump(Duration::from_secs(20));
    let cpu_after = process_cpu_secs(pid);

    let delta = cpu_after - cpu_before;
    let panes = client
        .layout
        .as_ref()
        .map(|l| l.panes.len())
        .unwrap_or_default();
    assert!(
        delta <= BUDGET_SECS,
        "idle server burned {delta:.2}s CPU over the 20s window (budget {BUDGET_SECS}s): \
         panes={panes} registry_rows={REGISTRY_ROWS} journal_bytes={} graph_bytes={graph}",
        seg1 + seg0
    );
}
