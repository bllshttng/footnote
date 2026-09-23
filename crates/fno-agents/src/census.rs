//! The running-process census : one row per long-lived process,
//! answering "is this running fno process an older build than the binary it
//! was launched from?". Python's `update.running_components` is a thin
//! adapter over `fno-agents census --json`; the walking and classifying live
//! here beside the drift classifier they reuse.
//!
//! Classifier sources, no third rule: a build SELF-REPORT (the keeper's
//! Identify reply carries `drift`, computed live from
//! [`crate::drift::self_drift`]) or, for a pre-report build, the process's
//! start time against its executable's mtime. A probe that cannot decide
//! reads `unknown` with the failure named, never `current`.

use crate::drift;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROBE_BUDGET: Duration = Duration::from_millis(750);

const TAG_IDENTIFY_GRAPH: u8 = 3; // graph_keeper.rs
const TAG_IDENTIFY_PANE: u8 = 4; // pane_keeper.rs Frame::Identify
const TAG_REPLY: u8 = 5; // both keepers

/// One row of the process table: the columns `ps -Ao
/// pid,ppid,state,etime,%cpu,rss,command` reports, read without exec'ing
/// `ps` (setuid on macOS, so a sandboxed caller's seatbelt refuses it).
#[derive(Debug, Clone)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    pub state: char,
    pub elapsed_s: u64,
    pub cpu_pct: f64,
    pub rss_kb: u64,
    pub command: String,
}

/// The shared Codex app-server's census row: health and installed-version
/// readiness are DIFFERENT axes, so the row carries both. A healthy daemon
/// running a version older than the installed CLI reads healthy + stale,
/// never healthy alone and never no row.
fn codex_app_server_rows() -> Vec<Value> {
    let readiness = crate::codex_daemon_readiness::codex_daemon_readiness();
    let verdict = match readiness.verdict {
        crate::codex_daemon_readiness::VersionVerdict::Current => "current",
        crate::codex_daemon_readiness::VersionVerdict::Stale => "stale",
        crate::codex_daemon_readiness::VersionVerdict::Ahead => "ahead",
        crate::codex_daemon_readiness::VersionVerdict::Unknown => "unknown",
    };
    let health = if readiness.healthy { "healthy" } else { "down" };
    let evidence = format!(
        "installed {}, live {}, {}, home {}",
        readiness
            .installed_version
            .as_deref()
            .unwrap_or("unreadable"),
        readiness.live_version.as_deref().unwrap_or("unreadable"),
        health,
        readiness.codex_home,
    );
    // No exe: the readiness reader knows the pid, never the binary path, and
    // the exe-position field must not carry a directory and read like one.
    let mut row = row(
        "codex-app-server",
        readiness.pid,
        Some("codex-app-server".to_string()),
        None,
        readiness.start_token.map(|t| t as f64),
        verdict,
        evidence.as_str(),
    );
    row["installed_version"] = json!(readiness.installed_version);
    row["live_version"] = json!(readiness.live_version);
    row["codex_home"] = json!(readiness.codex_home);
    vec![row]
}

#[cfg(test)]
pub(crate) fn test_proc_row(pid: u32, ppid: u32, command: &str) -> ProcRow {
    ProcRow {
        pid,
        ppid,
        state: 'S',
        elapsed_s: 1,
        cpu_pct: 0.0,
        rss_kb: 0,
        command: command.to_string(),
    }
}

/// True while `pid` is a zombie: dead but not yet reaped by its parent, so
/// `kill(pid, 0)` keeps succeeding while it holds no fds and serves nothing.
/// On macOS the kernel record is the sysctl `KERN_PROC_PID` read (the
/// `proc_pidinfo` BSD-status read answers a zero write for a zombie, so it
/// can never fire; measured 2026-09-18); on Linux the state letter after
/// the last `)` of `/proc/<pid>/stat`. An unreadable pid reads not-zombie:
/// this helper never invents a death.
#[cfg(target_os = "macos")]
pub fn pid_is_zombie(pid: u32) -> bool {
    use std::mem;
    // libc does not export `kinfo_proc` on Apple targets, so the read pins
    // the one field this decision needs: `extern_proc` prefix (p_un 16, two
    // pointers 16, p_flag 4) puts `p_stat` at byte 36 of the 648-byte
    // record. The ABI is stable on all 64-bit Darwin.
    #[repr(C, align(8))]
    struct KinfoProcScratch {
        head: KinfoProcHead,
        tail: [u8; 768],
    }
    #[repr(C)]
    struct KinfoProcHead {
        p_un: [u8; 16],
        p_vmspace: u64,
        p_sigacts: u64,
        p_flag: libc::c_int,
        p_stat: u8,
    }
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut info: KinfoProcScratch = unsafe { mem::zeroed() };
    let mut size = mem::size_of::<KinfoProcScratch>();
    // SAFETY: sysctl fills a caller-owned zeroed buffer; mib and size live
    // in this frame and are read only during the call.
    let done = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            &mut info as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    done == 0 && info.head.p_stat == libc::SZOMB as u8
}

#[cfg(target_os = "linux")]
pub fn pid_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| Some(s.rsplit_once(')')?.1.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pid_is_zombie(_pid: u32) -> bool {
    false
}

/// The process table plus the count of pids whose row could not be read.
pub fn process_table() -> (Vec<ProcRow>, usize) {
    #[cfg(target_os = "macos")]
    {
        process_table_libproc()
    }
    #[cfg(not(target_os = "macos"))]
    {
        process_table_ps()
    }
}

#[cfg(target_os = "macos")]
fn process_table_libproc() -> (Vec<ProcRow>, usize) {
    use std::mem;
    // libc has no PROC_PIDLISTTHREADS constant.
    const PROC_PIDLISTTHREADS: libc::c_int = 6;

    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return (Vec::new(), 0);
    }
    let mut pids = vec![0u32; count as usize + 64];
    let filled = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast::<libc::c_void>(),
            (pids.len() * mem::size_of::<u32>()) as libc::c_int,
        )
    };
    if filled <= 0 {
        return (Vec::new(), 0);
    }
    let now = epoch_now();
    let mut rows: Vec<ProcRow> = Vec::new();
    let mut unreadable = 0usize;
    for &pid in &pids[..filled as usize] {
        let mut bsd: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        let bsd_size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut bsd as *mut _ as *mut libc::c_void,
                bsd_size,
            )
        };
        // A short write is a pid that exited mid-walk or a kernel task:
        // neither belongs in the table, but both count as unreadable.
        if written != bsd_size {
            unreadable += 1;
            continue;
        }
        // Zombies never reach this line: their proc_pidinfo read is a zero
        // write and was counted unreadable above, so the sysctl-based
        // pid_is_zombie would be a dead second read per pid here.
        let zombie = bsd.pbi_status == libc::SZOMB;
        let mut rss_kb = 0u64;
        let mut usage_sum = 0i64;
        let mut state = if zombie { 'Z' } else { 'S' };
        if !zombie {
            let mut task: libc::proc_taskinfo = unsafe { mem::zeroed() };
            let task_size = mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
            let got_task = unsafe {
                libc::proc_pidinfo(
                    pid as libc::c_int,
                    libc::PROC_PIDTASKINFO,
                    0,
                    &mut task as *mut _ as *mut libc::c_void,
                    task_size,
                )
            };
            if got_task == task_size {
                rss_kb = task.pti_resident_size / 1024;
                let mut handles = vec![0u64; task.pti_threadnum.max(0) as usize];
                let got = unsafe {
                    libc::proc_pidinfo(
                        pid as libc::c_int,
                        PROC_PIDLISTTHREADS,
                        0,
                        handles.as_mut_ptr().cast::<libc::c_void>(),
                        (handles.len() * mem::size_of::<u64>()) as libc::c_int,
                    )
                };
                if got > 0 {
                    let mut any_running = false;
                    let mut any_stopped = false;
                    let mut any_uninterruptible = false;
                    for handle in &handles[..got as usize / mem::size_of::<u64>()] {
                        let mut thread: libc::proc_threadinfo = unsafe { mem::zeroed() };
                        let thread_size = mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
                        let thread_written = unsafe {
                            libc::proc_pidinfo(
                                pid as libc::c_int,
                                libc::PROC_PIDTHREADINFO,
                                *handle,
                                &mut thread as *mut _ as *mut libc::c_void,
                                thread_size,
                            )
                        };
                        if thread_written != thread_size {
                            continue;
                        }
                        usage_sum += thread.pth_cpu_usage as i64;
                        match thread.pth_run_state {
                            1 => any_running = true,
                            2 => any_stopped = true,
                            4 => any_uninterruptible = true,
                            _ => {}
                        }
                    }
                    state = if any_running {
                        'R'
                    } else if any_stopped {
                        'T'
                    } else if any_uninterruptible {
                        'U'
                    } else {
                        'S'
                    };
                }
            }
        }
        let command = argv_of(pid)
            .map(|argv| argv.join(" "))
            .unwrap_or_else(|| comm_string(&bsd.pbi_comm));
        rows.push(ProcRow {
            pid,
            ppid: bsd.pbi_ppid,
            state,
            elapsed_s: now.saturating_sub(bsd.pbi_start_tvsec),
            // pth_cpu_usage sums in TH_USAGE_SCALE (1000) units of one
            // thread; /10 reads percent, the number `ps %cpu` reports.
            cpu_pct: usage_sum as f64 / 10.0,
            rss_kb,
            command,
        });
    }
    (rows, unreadable)
}

/// The live argv of one pid, for the caller outside the census that needs
/// it: a pane-to-thread conversion carries the running writer's own pins
/// into the relaunch rather than re-deriving them from a default.
#[cfg(target_os = "macos")]
pub(crate) fn process_argv(pid: u32) -> Option<Vec<String>> {
    argv_of(pid)
}

/// The `ps` leg. It splits on whitespace, so an argument that CONTAINS a
/// space comes back as two. Every flag the conversion carries takes a
/// space-free value, and the caller drops what it does not recognise.
#[cfg(not(target_os = "macos"))]
pub(crate) fn process_argv(pid: u32) -> Option<Vec<String>> {
    let out = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let argv: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    (!argv.is_empty()).then_some(argv)
}

/// The live argv of `pid` from `KERN_PROCARGS2`, `None` when unreadable.
/// Mirrors `fno::pane_argv::process_argv`; that crate is a dev-only link
/// here, so the read lives beside its only production caller.
#[cfg(target_os = "macos")]
fn argv_of(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return None;
    }
    let mut buf = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    parse_procargs2(&buf[..size])
}

/// The macOS `KERN_PROCARGS2` buffer layout: an `i32` argc, the truncated
/// exec path (NUL-terminated), NUL padding to alignment, then argc
/// NUL-terminated strings (argv[0] is the path again), then the environment.
#[cfg(target_os = "macos")]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    if argc == 0 {
        return None;
    }
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    if i >= buf.len() {
        return None;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut args = Vec::with_capacity(argc);
    while args.len() < argc && i < buf.len() {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        args.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        i += 1;
    }
    (args.len() == argc).then_some(args)
}

#[cfg(target_os = "macos")]
fn comm_string(comm: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "macos")]
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `ps` leg for platforms where `ps` is not setuid: exec and parse the
/// same columns the native read returns.
pub(crate) fn process_table_ps() -> (Vec<ProcRow>, usize) {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-Ao", "pid,ppid,state,etime,%cpu,rss,command"])
        .output()
    else {
        return (Vec::new(), 0);
    };
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
        if let Some(row) = parse_ps_row(line) {
            rows.push(row);
        }
    }
    (rows, 0)
}

/// One `ps -Ao pid,ppid,state,etime,%cpu,rss,command` data line. Compiled
/// only where it has a caller: the Linux leg and its tests, both of which
/// are `not(target_os = "macos")`. A macOS test build otherwise compiles
/// it with no caller and deny-warnings turns the dead code into a failure.
pub(crate) fn parse_ps_row(line: &str) -> Option<ProcRow> {
    // ps right-aligns the numeric columns, so tokens must split on
    // whitespace RUNS - a per-char split yields empty fields and every
    // aligned column reads as a parse failure.
    let mut fields = line.trim().split_whitespace();
    let (Some(pid), Some(ppid), Some(state), Some(etime), Some(cpu), Some(rss)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return None;
    };
    // A pid that fails to parse is a torn line, not pid 0; keep a real
    // pid-0 row (the swapper, where a ps dialect lists it).
    let pid = pid.parse().ok()?;
    Some(ProcRow {
        pid,
        ppid: ppid.parse().unwrap_or(0),
        state: state.chars().next().unwrap_or('?'),
        elapsed_s: crate::gc::parse_etime(etime).unwrap_or(0),
        cpu_pct: cpu.parse().unwrap_or(0.0),
        rss_kb: rss.parse().unwrap_or(0),
        command: fields.collect::<Vec<_>>().join(" "),
    })
}

fn format_elapsed(secs: u64) -> String {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let d = secs / 86_400;
    if d > 0 {
        format!("{d:02}-{h:02}:{m:02}:{s:02}")
    } else if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// One line per process is this table's whole contract, and an argv may hold
/// a newline that third-party launchers write. Escape the two characters that
/// break a line, the way `ps` itself does, so the reader sees the bytes a real
/// `ps` would have handed it.
fn escape_row_command(command: &str) -> std::borrow::Cow<'_, str> {
    if !command.contains(['\n', '\r']) {
        return std::borrow::Cow::Borrowed(command);
    }
    std::borrow::Cow::Owned(command.replace('\n', "\\012").replace('\r', "\\015"))
}

/// The table as the `ps -Ao pid,ppid,state,etime,%cpu,rss,command` text the
/// Python footprint reader already parses.
pub fn ps_text(rows: &[ProcRow]) -> String {
    let mut out = String::from("PID PPID STAT ELAPSED %CPU RSS COMMAND\n");
    for row in rows {
        out.push_str(&format!(
            "{} {} {} {} {:.1} {} {}\n",
            row.pid,
            row.ppid,
            row.state,
            format_elapsed(row.elapsed_s),
            row.cpu_pct,
            row.rss_kb,
            escape_row_command(&row.command)
        ));
    }
    out
}

fn started_epoch(etime: Option<f64>) -> Option<f64> {
    let etime = etime?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some(now - etime)
}

fn started_before_rewrite(started_at: Option<f64>, exe: Option<&Path>) -> bool {
    let Some(started) = started_at else {
        return false;
    };
    let Some(exe) = exe else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(exe) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let Ok(nanos) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    started < nanos.as_secs_f64()
}

/// One bounded frame round-trip: write `request`, read the reply frame
/// (`reply_tag`), parse its JSON payload. `None` on any failure.
fn frame_round_trip(sock: &Path, request: [u8; 5], reply_tag: u8) -> Option<Value> {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(PROBE_BUDGET)).ok()?;
    stream.set_write_timeout(Some(PROBE_BUDGET)).ok()?;
    stream.write_all(&request).ok()?;
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).ok()?;
    if header[0] != reply_tag {
        return None;
    }
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len.min(1 << 20)];
    stream.read_exact(&mut payload).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// One Identify with a short bound; `Some` only when the keeper answers
/// with a parseable IdentifyReply. `identify_tag` selects the lane's frame
/// (the graph and pane keepers use different request tags, the same reply).
fn identify_reply(sock: &Path, identify_tag: u8) -> Option<Value> {
    frame_round_trip(sock, [identify_tag, 0, 0, 0, 0], TAG_REPLY)
}

/// The `on_restart` / `survives` pair, fixed per component so every surface
/// says the same thing.
fn fate(component: &str) -> (&'static str, &'static str) {
    match component {
        "daemon" => ("restarts", "workers and panes"),
        "store-keeper" => ("cycles; the next read respawns it", "the graph on disk"),
        "mux-server" => ("kept; only `--mux` replaces it", "its panes"),
        "codex-app-server" => (
            "safe upgrade only through the session-preserving transaction",
            "threads survive the daemon swap",
        ),
        _ => ("kept", "its pane; current only when that pane ends"),
    }
}

fn row(
    component: &str,
    pid: Option<u32>,
    name: Option<String>,
    exe: Option<String>,
    started_at: Option<f64>,
    verdict: &str,
    evidence: &str,
) -> Value {
    let (on_restart, survives) = fate(component);
    json!({
        "component": component,
        "pid": pid,
        "name": name,
        "exe": exe,
        "started_at": started_at,
        "verdict": verdict,
        "evidence": evidence,
        "on_restart": on_restart,
        "survives": survives,
    })
}

/// Keeper rows off the process table: argv[0] basename `fno-agents-worker`,
/// lane from the argv flag, socket and session from argv. Keepers sharing
/// one socket are listed as duplicates (change 2 retires them).
fn keeper_rows() -> Vec<Value> {
    let (table, _) = process_table();
    keeper_rows_from(&table)
}

fn keeper_rows_from(table: &[ProcRow]) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    for proc_row in table {
        let pid = proc_row.pid;
        let argv: Vec<&str> = proc_row.command.split_whitespace().collect();
        let Some(argv0) = argv.first() else {
            continue;
        };
        if Path::new(argv0)
            .file_name()
            .is_none_or(|n| n != "fno-agents-worker")
        {
            continue;
        }
        let flag = argv
            .iter()
            .find(|a| ["--pane", "--keeper", "--store-keeper"].contains(a));
        let component = match flag {
            Some(&"--store-keeper") => "store-keeper",
            Some(&"--keeper") => "thread-keeper",
            _ => "pane-keeper",
        };
        let sock = argv
            .windows(2)
            .find(|w| w[0] == "--sock")
            .map(|w| PathBuf::from(w[1]));
        let session = argv
            .windows(2)
            .find(|w| w[0] == "--session")
            .map(|w| w[1].to_string());
        let name = session
            .clone()
            .or_else(|| sock.as_ref().map(|s| s.display().to_string()));
        let started = started_epoch(Some(proc_row.elapsed_s as f64));
        let mut store_graph: Option<String> = None;
        let (verdict, evidence) = match sock.as_deref() {
            None => ("unknown", "argv declares no socket"),
            Some(sock) => {
                let tag = if component == "store-keeper" {
                    TAG_IDENTIFY_GRAPH
                } else {
                    TAG_IDENTIFY_PANE
                };
                match identify_reply(sock, tag) {
                    Some(reply) => {
                        if component == "store-keeper" {
                            store_graph =
                                reply.get("graph").and_then(Value::as_str).map(String::from);
                        }
                        match reply.get("drift").and_then(Value::as_str) {
                            Some("drifted") => ("stale", "build self-report"),
                            Some("fresh") => ("current", "build self-report"),
                            _ => {
                                // A reply with no drift key is a keeper built
                                // before the self-report; start time is the only
                                // reading it gives. An unreadable start time is
                                // no verdict, never current.
                                let exe = Path::new(argv0);
                                if started_before_rewrite(started, Some(exe)) {
                                    ("stale", "predates build self-report")
                                } else if started.is_some() {
                                    ("current", "started at-or-after the binary was written")
                                } else {
                                    ("unknown", "no readable start time")
                                }
                            }
                        }
                    }
                    None => ("unknown", "no Identify answer"),
                }
            }
        };
        let mut row = row(
            component,
            Some(pid),
            name,
            Some(argv0.to_string()),
            started,
            verdict,
            evidence,
        );
        if let Some(graph) = store_graph {
            row["graph"] = json!(graph);
        }
        if let Some(sock) = &sock {
            row["sock"] = json!(sock.display().to_string());
            if let Some(holder) = seen.get(&sock.display().to_string()) {
                row["evidence"] = json!(format!(
                    "{}; duplicate keeper on {} (seat held by pid {})",
                    row["evidence"].as_str().unwrap_or_default(),
                    sock.display(),
                    holder
                ));
            } else {
                seen.insert(sock.display().to_string(), pid);
            }
        }
        rows.push(row);
    }
    rows
}

/// The daemon's row, read in-process: one agent.status call feeds both the
/// pid and the drift verdict. A failed call reads unknown, never current.
async fn daemon_row() -> Value {
    use crate::client::{call_if_running, ClientError};
    use crate::protocol::Request;
    let resp = call_if_running(
        &crate::paths::AgentsHome::from_env(),
        &Request::new(1, "agent.status", json!({})),
    )
    .await;
    let pid = resp
        .as_ref()
        .ok()
        .and_then(|r| r.result())
        .and_then(|r| r.pointer("/daemon/pid"))
        .and_then(Value::as_u64)
        .map(|p| p as u32);
    let (verdict, evidence) = match resp {
        Ok(resp) => match resp.result() {
            Some(result) => {
                let drift = crate::client::drift_from_status(result);
                let label = drift::drift_label(&drift);
                match label {
                    "fresh" => ("current".to_string(), "build self-report".to_string()),
                    "drifted" => ("stale".to_string(), "build self-report".to_string()),
                    _ => (
                        "unknown".to_string(),
                        "status reply carries no drift verdict".to_string(),
                    ),
                }
            }
            None => (
                "unknown".to_string(),
                "status reply unparseable".to_string(),
            ),
        },
        Err(ClientError::DaemonNotRunning) => ("current".to_string(), "no daemon running".into()),
        Err(e) => ("unknown".to_string(), format!("status call failed: {e}")),
    };
    row(
        "daemon",
        pid,
        Some("agents home".into()),
        None,
        None,
        &verdict,
        &evidence,
    )
}

/// Mux server rows from the front door's own `ls --json`; the pid sidecar
/// field (change 4) is what the census classifies.
fn mux_rows(table: &[ProcRow]) -> Vec<Value> {
    let elapsed: BTreeMap<u32, u64> = table
        .iter()
        .map(|proc_row| (proc_row.pid, proc_row.elapsed_s))
        .collect();
    let started_of = |pid: u32| started_epoch(elapsed.get(&pid).map(|&secs| secs as f64));
    let Some(fno) = resolve_fno() else {
        return Vec::new();
    };
    let Ok(out) = std::process::Command::new(&fno)
        .args(["mux", "ls", "--json"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(rows) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Vec::new();
    };
    let Some(list) = rows.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in list {
        if r.get("state").and_then(Value::as_str) != Some("live") {
            continue;
        }
        let session = r
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or("unnamed");
        let _panes = r.get("panes").and_then(Value::as_u64).unwrap_or(0);
        let pid = r.get("pid").and_then(Value::as_u64).map(|p| p as u32);
        let (verdict, evidence) = match pid {
            Some(pid) => {
                let started = started_of(pid);
                if started_before_rewrite(started, Some(&fno)) {
                    ("stale", "predates build self-report")
                } else if started.is_some() {
                    ("current", "started at-or-after the binary was written")
                } else {
                    ("unknown", "no readable start time")
                }
            }
            None => ("unknown", "no pid sidecar"),
        };
        let mut row = row(
            "mux-server",
            pid,
            Some(session.to_string()),
            Some(fno.display().to_string()),
            started_of(pid.unwrap_or(0)),
            verdict,
            evidence,
        );
        // The kept/unkept split: kept panes survive the server (their keeper
        // re-adopts them); unkept ones end with it. The wording names both
        // counts so an operator sees exactly who a restart would cost.
        let mut kept = 0u64;
        let mut unkept = 0u64;
        if let Ok(pane_out) = std::process::Command::new(&fno)
            .args(["mux", "pane", "ls", "--session", session, "--json"])
            .output()
        {
            if let Ok(keeper_out) = std::process::Command::new(&fno)
                .args(["mux", "pane", "keeper", "list", "--json"])
                .output()
            {
                let keeper_rows: Vec<Value> =
                    serde_json::from_slice(&keeper_out.stdout).unwrap_or_default();
                let live_keepers: Vec<u64> = keeper_rows
                    .iter()
                    .filter(|k| k.get("session").and_then(Value::as_str) == Some(session))
                    .filter(|k| k.get("stale").is_none())
                    .filter_map(|k| k.get("child_pid").and_then(Value::as_u64))
                    .collect();
                if let Ok(pane_rows) = serde_json::from_slice::<Vec<Value>>(&pane_out.stdout) {
                    for pane in &pane_rows {
                        let child = pane.get("child_pid").and_then(Value::as_u64);
                        if child.is_some_and(|c| live_keepers.contains(&c)) {
                            kept += 1;
                        } else {
                            unkept += 1;
                        }
                    }
                }
            }
        }
        row["on_restart"] = json!(format!(
            "ending {unkept} unkept shell(s); keeps {kept} kept pane(s)"
        ));
        row["survives"] = json!(format!(
            "{kept} kept pane(s){}; {unkept} unkept",
            if kept + unkept == 0 {
                " (no panes)"
            } else {
                ""
            }
        ));
        out.push(row);
    }
    out
}

fn resolve_fno() -> Option<PathBuf> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"));
    let candidate = cargo_home.join("bin").join("fno");
    if candidate.is_file() {
        return Some(candidate);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("fno"))
            .find(|p| p.is_file())
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The census: daemon, keepers, mux servers. Bounded: every probe and every
/// subprocess carries a timeout, so a wedged keeper delays one row, never
/// the census.
pub async fn census() -> Vec<Value> {
    let (table, _) = process_table();
    let mut rows = vec![daemon_row().await];
    rows.extend(keeper_rows_from(&table));
    rows.extend(mux_rows(&table));
    rows.extend(codex_app_server_rows());
    rows
}

/// Run the daemon-free census subcommand.  The process walk stays in Rust so
/// Python callers and the machine sample share one table implementation.
pub async fn run_verb(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--tree-rss") {
        let Some(index) = args.iter().position(|arg| arg == "--tree-rss") else {
            unreachable!()
        };
        let Some(raw) = args.get(index + 1) else {
            eprintln!("fno-agents census: --tree-rss needs a pid list");
            return 2;
        };
        let mut pids = Vec::new();
        for token in raw.split(',').filter(|token| !token.is_empty()) {
            match token.parse::<u32>() {
                Ok(pid) => pids.push(pid),
                Err(_) => {
                    eprintln!("fno-agents census: invalid pid {token}");
                    return 2;
                }
            }
        }
        let (rows, _) = process_table_ps();
        println!(
            "{}",
            json!({"rss_mb": crate::session_cost::tree_rss(&rows, &pids)})
        );
        return 0;
    }
    if args.iter().any(|arg| arg == "--ps") {
        let (rows, unreadable) = process_table();
        println!(
            "{}",
            json!({"ps": ps_text(&rows), "unreadable": unreadable})
        );
        return 0;
    }
    let rows = census().await;
    println!(
        "{}",
        serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
    );
    0
}

/// Walk the keeper rows synchronously (tests, and callers already holding no
/// daemon context).
pub fn census_blocking() -> Vec<Value> {
    let (table, _) = process_table();
    let mut rows = Vec::new();
    rows.extend(keeper_rows_from(&table));
    rows.extend(mux_rows(&table));
    rows
}

/// One store keeper the restart verb cycled (or spared).
pub struct CycledKeeper {
    pub graph: Option<String>,
    pub old_pid: Option<u32>,
    pub result: String,
}

/// Shutdown the stale store keepers the census found (change 6). No
/// respawn is attempted here: the next read respawns each keeper on the
/// installed binary - the same self-heal the fate text promises - and the
/// Python client's spawner owns the launch flags (read_source, events).
/// A keeper answering `busy` keeps its seat and is reported, not forced.
pub async fn cycle_stale_store_keepers() -> (Vec<CycledKeeper>, usize) {
    let rows = keeper_rows();
    let stale_panes = rows
        .iter()
        .filter(|r| {
            matches!(
                r["component"].as_str(),
                Some("pane-keeper") | Some("thread-keeper")
            ) && r["verdict"] == "stale"
        })
        .count();
    let mut out = Vec::new();
    for r in rows.iter() {
        if r["component"] != "store-keeper" || r["verdict"] != "stale" {
            continue;
        }
        let Some(sock) = r["sock"].as_str().map(PathBuf::from) else {
            continue;
        };
        let result = match shutdown_reply(&sock) {
            Some(reply) if reply.get("ok") == Some(&json!(true)) => {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline && sock.exists() {
                    std::thread::sleep(Duration::from_millis(50));
                }
                "cycled".to_string()
            }
            Some(reply) => format!(
                "spared: {}",
                reply
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("mutation in flight")
            ),
            None => "spared: no shutdown answer".to_string(),
        };
        out.push(CycledKeeper {
            graph: r["graph"].as_str().map(String::from),
            old_pid: r["pid"].as_u64().map(|p| p as u32),
            result,
        });
    }
    (out, stale_panes)
}

/// Send one Shutdown frame and read the reply (tag 2 out, response tag 4).
fn shutdown_reply(sock: &Path) -> Option<Value> {
    frame_round_trip(sock, [2, 0, 0, 0, 0], 4)
}

#[cfg(test)]
mod process_table_tests {
    use super::{pid_is_zombie, process_table, ps_text};

    #[test]
    fn process_table_reads_its_own_row() {
        let (table, _unreadable) = process_table();
        let me = std::process::id();
        let row = table
            .iter()
            .find(|row| row.pid == me)
            .expect("the test process reads its own row");
        assert_eq!(row.ppid, unsafe { libc::getppid() } as u32);
        assert!(row.rss_kb > 0, "resident memory reads nonzero");
        let exe_name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .expect("current_exe resolves");
        assert!(
            row.command.contains(&exe_name),
            "the command holds the test binary name: {} (want {exe_name})",
            row.command
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn process_table_reads_a_spinning_child_like_ps_does() {
        // The CPU reading is a lifetime average, so the bar needs a process
        // whose lifetime IS the spin: a young busy-loop child. Read `ps %cpu`
        // for the same pid as the ground truth the table must agree with.
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("while :; do :; done")
            .spawn()
            .expect("spawn the spinner child");
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let (table, _unreadable) = process_table();
        let row = table
            .iter()
            .find(|row| row.pid == child.id())
            .expect("the spinning child reads a row");
        let ps_row = std::process::Command::new("ps")
            .args(["-o", "%cpu=", "-p", &child.id().to_string()])
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
        child.kill().ok();
        child.wait().ok();

        assert!(
            row.cpu_pct >= 15.0,
            "a young full-spin child reads as CPU: {}",
            row.cpu_pct
        );
        let ps_cpu: f64 = ps_row
            .and_then(|text| text.parse().ok())
            .expect("ps answers %cpu for a live child");
        assert!(
            (row.cpu_pct - ps_cpu).abs() <= 20.0,
            "the table and ps disagree on the same child: table {} vs ps {}",
            row.cpu_pct,
            ps_cpu
        );
    }

    // The parser it pins compiles only beside its Linux caller.
    #[cfg(not(target_os = "macos"))]
    #[test]
    // The Linux leg under test: parse_ps_row is not compiled on macOS
    // (deny-warnings kills the dead code there), so neither does its test.
    #[cfg(not(target_os = "macos"))]
    fn ps_leg_parses_right_aligned_columns() {
        let row = super::parse_ps_row("  1234  2556 S 02:03  1.5  10240 /bin/sleep 37")
            .expect("an aligned ps row parses");
        assert_eq!(row.pid, 1234);
        assert_eq!(row.ppid, 2556);
        assert_eq!(row.state, 'S');
        assert_eq!(row.elapsed_s, 123);
        assert!((row.cpu_pct - 1.5).abs() < f64::EPSILON);
        assert_eq!(row.rss_kb, 10240);
        assert_eq!(row.command, "/bin/sleep 37");
        assert!(super::parse_ps_row("").is_none(), "a torn line drops");
    }

    #[test]
    fn ps_text_emits_the_footprint_columns() {
        let rows = vec![super::ProcRow {
            pid: 100,
            ppid: 1,
            state: 'R',
            elapsed_s: 3600,
            cpu_pct: 86.0,
            rss_kb: 1024,
            command: "fno-agents-worker --run".into(),
        }];
        assert_eq!(
            ps_text(&rows),
            "PID PPID STAT ELAPSED %CPU RSS COMMAND\n100 1 R 01:00:00 86.0 1024 fno-agents-worker --run\n"
        );
    }

    #[test]
    fn ps_text_escapes_a_newline_argv_to_one_line_per_row() {
        let rows = vec![
            super::ProcRow {
                pid: 101,
                ppid: 1,
                state: 'R',
                elapsed_s: 60,
                cpu_pct: 0.0,
                rss_kb: 1024,
                command: "tr -d \"\nmore\"\r".into(),
            },
            super::ProcRow {
                pid: 102,
                ppid: 1,
                state: 'S',
                elapsed_s: 60,
                cpu_pct: 0.0,
                rss_kb: 1024,
                command: "clean argv".into(),
            },
        ];
        let text = ps_text(&rows);
        assert_eq!(text.lines().count(), 3, "header plus one line per row");
        assert!(
            text.contains("101 1 R 01:00 0.0 1024 tr -d \"\\012more\"\\015"),
            "newline and CR carry as the four characters \\012 and \\015: {text}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn argv_copy_agrees_with_the_fno_crate_reader() {
        // The argv reader here is a verbatim copy of fno::pane_argv's (the
        // dev-only link blocks a shared call in production). Both readers
        // parsing this process's live argv pins the copies together: a
        // layout drift fails here, not silently in the census.
        let mine = super::argv_of(std::process::id()).expect("own argv readable");
        let theirs = fno::pane_argv::process_argv(std::process::id())
            .expect("fno crate reads the same argv");
        assert_eq!(mine, theirs);
    }

    #[test]
    fn pid_is_zombie_reads_an_unreaped_exit_as_zombie_and_a_live_pid_as_not() {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !pid_is_zombie(pid) {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            pid_is_zombie(pid),
            "an exited, unwaited child reads as zombie"
        );
        assert!(
            !pid_is_zombie(std::process::id()),
            "a live pid is not zombie"
        );
        child.wait().unwrap();
    }
}
