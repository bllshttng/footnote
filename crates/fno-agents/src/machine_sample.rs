//! A single, portable snapshot of machine load and process pressure.
//!
//! The machine-watch arm owns the verdict, while this module owns the raw
//! sample and its journal readout.  Pure parsers live here so Linux and macOS
//! can share the same tests.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MACHINE_SNAPSHOT_NAME: &str = "machine-process-snapshot.txt";
const MACHINE_SNAPSHOT_ACTIVE_NAME: &str = "machine-process-snapshot.active";
const MAX_MACHINE_SNAPSHOT_BYTES: usize = 1024 * 1024;
const ABSOLUTE_PROCESS_SNAPSHOT_THRESHOLD: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTicks {
    pub busy: u64,
    pub system: u64,
    pub idle: u64,
}

/// Return `(busy_fraction, system_fraction)` between two monotonic samples.
pub fn busy_between(previous: HostTicks, current: HostTicks) -> Option<(f64, f64)> {
    let busy = current.busy.checked_sub(previous.busy)?;
    let system = current.system.checked_sub(previous.system)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    let total = busy.checked_add(idle)?;
    (total > 0).then(|| (busy as f64 / total as f64, system as f64 / total as f64))
}

/// Parse the first aggregate CPU row from Linux `/proc/stat`.
pub fn parse_proc_stat_cpu(text: &str) -> Option<HostTicks> {
    let row = text.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = row
        .split_whitespace()
        .skip(1)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 5 {
        return None;
    }
    let user = values[0];
    let nice = values[1];
    let system = values[2];
    let idle = values[3];
    let iowait = values.get(4).copied().unwrap_or(0);
    let irq = values.get(5).copied().unwrap_or(0);
    let softirq = values.get(6).copied().unwrap_or(0);
    let steal = values.get(7).copied().unwrap_or(0);
    Some(HostTicks {
        busy: user + nice + system + irq + softirq + steal,
        system: system + irq + softirq,
        idle: idle + iowait,
    })
}

#[cfg(target_os = "linux")]
pub fn host_ticks() -> Option<HostTicks> {
    parse_proc_stat_cpu(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(target_os = "macos")]
pub fn host_ticks() -> Option<HostTicks> {
    // host_statistics(HOST_CPU_LOAD_INFO) is the source used by Activity
    // Monitor and includes user, nice, system and idle ticks.
    #[repr(C)]
    struct CpuLoadInfo {
        cpu_ticks: [u32; 4],
    }
    extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    let mut info = CpuLoadInfo { cpu_ticks: [0; 4] };
    let mut count = 4;
    let status = unsafe {
        host_statistics(
            mach_host_self(),
            3,
            info.cpu_ticks.as_mut_ptr().cast(),
            &mut count,
        )
    };
    (status == 0).then(|| HostTicks {
        busy: u64::from(info.cpu_ticks[0])
            + u64::from(info.cpu_ticks[1])
            + u64::from(info.cpu_ticks[2]),
        system: u64::from(info.cpu_ticks[2]),
        idle: u64::from(info.cpu_ticks[3]),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn host_ticks() -> Option<HostTicks> {
    None
}

pub fn load_average() -> Option<(f64, f64, f64)> {
    let mut values = [0.0_f64; 3];
    let count = unsafe { libc::getloadavg(values.as_mut_ptr(), 3) };
    (count == 3).then_some((values[0], values[1], values[2]))
}

pub fn parse_vm_stat_memory(text: &str) -> Option<(f64, f64)> {
    let page_size = text
        .lines()
        .next()?
        .split("page size of")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    let mut occupied = None;
    let mut stored = None;
    for line in text.lines().skip(1) {
        let (label, value) = line.split_once(':')?;
        let pages = value.trim().trim_end_matches('.').parse::<u64>().ok()?;
        match label.trim() {
            "Pages occupied by compressor" => occupied = Some(pages),
            "Pages stored in compressor" => stored = Some(pages),
            _ => {}
        }
    }
    let scale = page_size as f64 / 1024.0 / 1024.0 / 1024.0;
    Some((occupied? as f64 * scale, stored? as f64 * scale))
}

pub fn parse_swapusage_mb(text: &str) -> Option<(f64, f64)> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let number = |token: &str| {
        token
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>()
            .parse::<f64>()
            .ok()
    };
    let mut total = None;
    let mut used = None;
    for i in 0..tokens.len().saturating_sub(2) {
        match tokens[i] {
            "total" => total = number(tokens[i + 2]),
            "used" => used = number(tokens[i + 2]),
            _ => {}
        }
    }
    let (total, used) = (total?, used?);
    (total > 0.0).then_some((total, used))
}

pub fn parse_meminfo_memory(text: &str) -> Option<(f64, f64, Option<f64>)> {
    let kb = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|value| value.parse::<f64>().ok())
    };
    let total = kb("SwapTotal")? / 1024.0 / 1024.0;
    let free = kb("SwapFree")? / 1024.0 / 1024.0;
    let zswapped = kb("Zswapped").map(|value| value / 1024.0 / 1024.0);
    Some((total - free, total, zswapped))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MachineSample {
    pub sample_id: Option<String>,
    pub load_1m: Option<f64>,
    pub load_5m: Option<f64>,
    pub load_15m: Option<f64>,
    pub cores: Option<f64>,
    pub busy_fraction: Option<f64>,
    pub sys_fraction: Option<f64>,
    pub busy_window_s: Option<f64>,
    pub runnable: Option<u64>,
    pub processes: Option<u64>,
    pub zombies: Option<u64>,
    pub cargo_processes: Option<u64>,
    pub test_processes: Option<u64>,
    pub compressor_gb: Option<f64>,
    pub compressed_gb: Option<f64>,
    pub swap_used_gb: Option<f64>,
    pub swap_total_gb: Option<f64>,
    pub live_rows: Option<u64>,
    pub kings: Option<u64>,
    pub workers: Option<u64>,
    pub verdict: Option<String>,
    pub busy_band: Option<f64>,
    pub load_band_per_core: Option<f64>,
    pub took_ms: Option<u64>,
    pub sessions: Option<Value>,
    pub unresolved: Option<Value>,
    pub top_rss: Option<Value>,
    pub top_names: Option<Value>,
    pub top_parents: Option<Value>,
    pub sessions_error: Option<String>,
    #[serde(skip)]
    pub(crate) procs: Vec<crate::census::ProcRow>,
    #[serde(skip)]
    pub(crate) top_cpu: Vec<crate::census::ProcRow>,
    #[serde(skip)]
    pub(crate) throttle_minutes: u64,
}

pub fn read(
    home: &crate::paths::AgentsHome,
    previous: Option<HostTicks>,
) -> (MachineSample, Option<HostTicks>) {
    let started = std::time::Instant::now();
    let (procs, _) = crate::census::process_table_ps();
    let current = host_ticks();
    let (busy_fraction, sys_fraction) = previous
        .zip(current)
        .and_then(|(old, new)| busy_between(old, new))
        .map_or((None, None), |(busy, system)| (Some(busy), Some(system)));
    let (load_1m, load_5m, load_15m) = load_average()
        .map(|(a, b, c)| (Some(a), Some(b), Some(c)))
        .unwrap_or((None, None, None));
    let mut sample = MachineSample {
        sample_id: Some(sample_id()),
        load_1m,
        load_5m,
        load_15m,
        cores: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as f64),
        busy_fraction,
        sys_fraction,
        busy_window_s: previous.zip(current).map(|_| 300.0),
        runnable: Some(procs.iter().filter(|row| row.state == 'R').count() as u64),
        processes: Some(procs.len() as u64),
        zombies: Some(procs.iter().filter(|row| row.state == 'Z').count() as u64),
        cargo_processes: Some(
            procs
                .iter()
                .filter(|row| {
                    crate::test_run::is_cargo_row(row) || crate::test_run::is_compile(row)
                })
                .count() as u64,
        ),
        test_processes: Some(
            procs
                .iter()
                .filter(|row| {
                    crate::orphan_reap::is_deps_test_binary(&row.command)
                        || row
                            .command
                            .split_whitespace()
                            .any(|part| part.rsplit('/').next() == Some("pytest"))
                })
                .count() as u64,
        ),
        compressor_gb: None,
        compressed_gb: None,
        swap_used_gb: None,
        swap_total_gb: None,
        live_rows: None,
        kings: None,
        workers: None,
        verdict: None,
        busy_band: None,
        load_band_per_core: None,
        took_ms: None,
        sessions: None,
        unresolved: None,
        top_rss: None,
        top_names: None,
        top_parents: None,
        sessions_error: None,
        procs,
        top_cpu: Vec::new(),
        throttle_minutes: 60,
    };
    sample.top_cpu = sample.procs.clone();
    sample.top_cpu.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sample.top_cpu.truncate(3);
    sample.top_names = Some(top_name_rows(&sample.procs));
    sample.top_parents = Some(top_parent_rows(&sample.procs));
    let mut warnings = Vec::new();
    let live = crate::spawn_gate::live_rows(&home.registry_json(), &mut warnings);
    sample.live_rows = Some(live.len() as u64);
    sample.kings = Some(live.iter().filter(|row| row.crown_level.is_some()).count() as u64);
    sample.workers = Some(live.iter().filter(|row| row.crown_level.is_none()).count() as u64);
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("vm_stat").output() {
            if let Some((occupied, stored)) =
                parse_vm_stat_memory(&String::from_utf8_lossy(&out.stdout))
            {
                sample.compressor_gb = Some(occupied);
                sample.compressed_gb = Some(stored);
            }
        }
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "vm.swapusage"])
            .output()
        {
            if let Some((total, used)) =
                crate::spawn_gate::parse_swapusage_mb(&String::from_utf8_lossy(&out.stdout))
            {
                sample.swap_total_gb = Some(total / 1024.0);
                sample.swap_used_gb = Some(used / 1024.0);
            }
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        if let Some((used, total, compressed)) = parse_meminfo_memory(&text) {
            sample.swap_used_gb = Some(used);
            sample.swap_total_gb = Some(total);
            sample.compressed_gb = compressed;
        }
    }
    sample.took_ms = Some(started.elapsed().as_millis() as u64);
    (sample, current)
}

impl MachineSample {
    pub fn to_data(&self, verdict: &str, busy_band: f64, load_band: f64) -> Value {
        let mut data = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        let object = data
            .as_object_mut()
            .expect("MachineSample serializes to object");
        object.insert("verdict".into(), json!(verdict));
        object.insert("busy_band".into(), json!(busy_band));
        object.insert("load_band_per_core".into(), json!(load_band));
        data
    }
}

pub fn newest(journal: &Path) -> Option<(String, Value)> {
    let text = std::fs::read_to_string(journal).ok()?;
    text.lines().rev().find_map(|line| {
        let event: Value = serde_json::from_str(line).ok()?;
        (event.get("type")?.as_str()? == "machine_sample").then(|| {
            let mut data = event.get("data").cloned().unwrap_or(Value::Null);
            if let Some(object) = data.as_object_mut() {
                if let Some(ts) = event.get("ts").and_then(Value::as_str) {
                    object.insert("_ts".into(), json!(ts));
                }
            }
            let id = data
                .get("sample_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (id, data)
        })
    })
}

fn display(value: Option<&Value>, digits: usize) -> String {
    match value {
        Some(Value::Number(number)) => number
            .as_f64()
            .map(|n| format!("{n:.digits$}"))
            .unwrap_or_else(|| "unmeasured".into()),
        _ => "unmeasured".into(),
    }
}

fn percent(value: Option<&Value>) -> String {
    match value.and_then(Value::as_f64) {
        Some(number) => format!("{:.0}", number * 100.0),
        None => "unmeasured".into(),
    }
}

pub fn footer_line(row: &(String, Value), now: DateTime<Utc>) -> String {
    let (id, data) = row;
    let ts = data
        .get("_ts")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|value| value.with_timezone(&Utc));
    let age = ts
        .map(|at| now.signed_duration_since(at).num_seconds().max(0))
        .or_else(|| data.get("age_s").and_then(Value::as_i64))
        .unwrap_or(0);
    let _ = id;
    format!(
        "{}% busy ({}% sys) of {} cores against band {}% -> {} · load_15m {} ({} per core, band {}) · {} runnable of {} processes · {} zombies · compressor {} GB · swap {} of {} GB · sampled {}s ago",
        percent(data.get("busy_fraction")),
        percent(data.get("sys_fraction")),
        display(data.get("cores"), 0),
        percent(data.get("busy_band")),
        data.get("verdict").and_then(Value::as_str).unwrap_or("unreadable"),
        display(data.get("load_15m"), 1),
        display(data.get("load_15m"), 1),
        display(data.get("load_band_per_core"), 0),
        data.get("runnable").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_else(|| "unmeasured".into()),
        data.get("processes").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_else(|| "unmeasured".into()),
        data.get("zombies").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_else(|| "unmeasured".into()),
        display(data.get("compressor_gb"), 1),
        display(data.get("swap_used_gb"), 1),
        display(data.get("swap_total_gb"), 1),
        age
    )
}

pub fn no_row_footer() -> String {
    "no machine_sample row yet; the daemon's machine_watch arm writes one every 300s".into()
}

pub fn stamp_refusal(
    mut refusal: crate::spawn_gate::Refusal,
    journal: &Path,
    now: DateTime<Utc>,
) -> crate::spawn_gate::Refusal {
    let mut sample = Map::new();
    let load_1m_now = load_average().map(|(one, _, _)| one);
    if let Some((id, data)) = newest(journal) {
        sample.insert("id".into(), json!(id));
        let age_s = data
            .get("_ts")
            .and_then(Value::as_str)
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|value| value.with_timezone(&Utc))
            .map(|ts| now.signed_duration_since(ts).num_seconds().max(0) as u64)
            .unwrap_or(0);
        sample.insert("age_s".into(), json!(age_s));
        for key in [
            "verdict",
            "busy_fraction",
            "load_15m",
            "compressor_gb",
            "swap_used_gb",
        ] {
            sample.insert(key.into(), data.get(key).cloned().unwrap_or(Value::Null));
        }
    } else {
        sample.insert("id".into(), Value::Null);
        sample.insert("reason".into(), json!("no machine_sample row yet"));
    }
    sample.insert(
        "load_1m_now".into(),
        load_1m_now.map_or(Value::Null, |v| json!(v)),
    );
    refusal.event.insert("sample".into(), Value::Object(sample));
    refusal
}

pub fn sample_id() -> String {
    format!("ms-{}-{}", now_ms(), std::process::id())
}

/// Top process groups by executable basename: the fold that names a runaway.
/// `ppid` is the most common parent inside the group, so one fork loop reads
/// as one line in the sample and in a page body.
pub fn top_name_rows(procs: &[crate::census::ProcRow]) -> Value {
    let mut groups: std::collections::HashMap<String, (u64, std::collections::HashMap<u32, u64>)> =
        Default::default();
    for row in procs {
        let name = row
            .command
            .split_whitespace()
            .next()
            .and_then(|token| token.rsplit('/').next())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let entry = groups.entry(name.to_string()).or_default();
        entry.0 += 1;
        *entry.1.entry(row.ppid).or_default() += 1;
    }
    let mut rows: Vec<(String, u64, u32)> = groups
        .into_iter()
        .map(|(name, (count, parents))| {
            let ppid = parents
                .into_iter()
                .max_by_key(|(pid, n)| (*n, std::cmp::Reverse(*pid)))
                .map(|(pid, _)| pid)
                .unwrap_or_default();
            (name, count, ppid)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(10);
    json!(rows
        .into_iter()
        .map(|(name, count, ppid)| json!({"name": name, "count": count, "ppid": ppid}))
        .collect::<Vec<_>>())
}

/// Top process parents by child count, with the parent's command when it was
/// present in the same process-table read.
pub fn top_parent_rows(procs: &[crate::census::ProcRow]) -> Value {
    let commands: std::collections::HashMap<u32, &str> = procs
        .iter()
        .map(|row| (row.pid, row.command.as_str()))
        .collect();
    let mut counts = std::collections::HashMap::<u32, u64>::new();
    for row in procs {
        *counts.entry(row.ppid).or_default() += 1;
    }
    let mut rows: Vec<(u32, u64)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(10);
    json!(rows
        .into_iter()
        .map(|(pid, children)| json!({
            "pid": pid,
            "children": children,
            "command": commands.get(&pid).copied().unwrap_or("unavailable"),
        }))
        .collect::<Vec<_>>())
}

pub(crate) fn maybe_capture_process_snapshot(
    home: &crate::paths::AgentsHome,
    processes: usize,
    baseline: Option<u64>,
    capture: impl FnOnce() -> io::Result<Vec<u8>>,
) -> io::Result<bool> {
    let dynamic_threshold = baseline
        .filter(|value| *value > 0)
        .map(|value| value.saturating_mul(2));
    let over_threshold = processes >= ABSOLUTE_PROCESS_SNAPSHOT_THRESHOLD
        || dynamic_threshold.is_some_and(|threshold| processes as u64 >= threshold);
    home.ensure_root()?;
    let active = home.root().join(MACHINE_SNAPSHOT_ACTIVE_NAME);
    if active.exists() {
        if over_threshold || baseline.is_none() {
            return Ok(false);
        }
        match std::fs::remove_file(&active) {
            Ok(()) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if !over_threshold {
        return Ok(false);
    }

    let raw = capture()?;
    let mut contents = format!(
        "captured_at: {}\nprocesses: {processes}\nthreshold: {}\n\n",
        chrono::Utc::now().to_rfc3339(),
        dynamic_threshold.map_or_else(
            || ABSOLUTE_PROCESS_SNAPSHOT_THRESHOLD.to_string(),
            |threshold| format!(
                "{ABSOLUTE_PROCESS_SNAPSHOT_THRESHOLD} or 2x baseline ({threshold})"
            ),
        )
    )
    .into_bytes();
    let remaining = MAX_MACHINE_SNAPSHOT_BYTES.saturating_sub(contents.len());
    contents.extend_from_slice(&raw[..raw.len().min(remaining)]);

    let snapshot = home.root().join(MACHINE_SNAPSHOT_NAME);
    let temporary = snapshot.with_extension(format!("txt.{}.{}.tmp", std::process::id(), now_ms()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let write_result = file.write_all(&contents).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, &snapshot) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::write(active, chrono::Utc::now().to_rfc3339())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_ticks_parse_busy_and_system() {
        let ticks = parse_proc_stat_cpu("cpu  10 2 3 20 4 5 6 7\ncpu0 1 1 1 1").unwrap();
        assert_eq!(ticks.busy, 33);
        assert_eq!(ticks.system, 14);
        assert_eq!(ticks.idle, 24);
    }

    #[test]
    fn busy_between_rejects_counter_regression_and_zero_window() {
        let a = HostTicks {
            busy: 3,
            system: 1,
            idle: 7,
        };
        assert!(busy_between(
            a,
            HostTicks {
                busy: 2,
                system: 1,
                idle: 8
            }
        )
        .is_none());
        assert!(busy_between(a, a).is_none());
    }

    #[test]
    fn swapusage_reads_megabytes() {
        assert_eq!(
            parse_swapusage_mb("total = 18432.00M used = 17080.75M free = 1M"),
            Some((18432.0, 17080.75))
        );
    }

    #[test]
    fn process_pressure_snapshot_is_bounded_once_per_episode_and_names_top_parents() {
        let row =
            |pid: u32, ppid: u32, command: &str| crate::census::test_proc_row(pid, ppid, command);
        let procs = vec![
            row(1, 100, "git status"),
            row(2, 100, "git diff"),
            row(3, 200, "git log"),
            row(4, 100, "ssh host"),
            row(5, 1, "/usr/bin/rustc main.rs"),
            row(6, 1, ""),
        ];
        let binding = top_name_rows(&procs);
        let rows = binding.as_array().unwrap();
        assert_eq!(rows[0]["name"], "git");
        assert_eq!(rows[0]["count"], 3);
        assert_eq!(rows[0]["ppid"], 100, "largest parent sub-group wins");
        assert_eq!(rows[1]["name"], "rustc", "count ties sort by name");
        assert_eq!(rows[2]["name"], "ssh");
        assert_eq!(rows.len(), 3, "an empty command names no group");

        let pressure_procs: Vec<_> = std::iter::once(row(1, 0, "/sbin/launchd"))
            .chain(
                (2..=ABSOLUTE_PROCESS_SNAPSHOT_THRESHOLD as u32).map(|pid| row(pid, 1, "worker")),
            )
            .collect();
        let parents = top_parent_rows(&pressure_procs);
        assert_eq!(parents[0]["pid"], 1);
        assert_eq!(parents[0]["children"], 1999);
        assert_eq!(parents[0]["command"], "/sbin/launchd");

        let dir = tempfile::tempdir().unwrap();
        let home = crate::paths::AgentsHome::at(dir.path());
        let mut captured = 0;
        let snapshot = maybe_capture_process_snapshot(&home, pressure_procs.len(), None, || {
            captured += 1;
            let mut ps_output = b"PID PPID RSS ELAPSED COMMAND\n".to_vec();
            ps_output.extend(vec![b'x'; MAX_MACHINE_SNAPSHOT_BYTES * 2]);
            Ok(ps_output)
        })
        .unwrap();
        assert!(snapshot);
        assert_eq!(captured, 1);
        assert!(
            !maybe_capture_process_snapshot(&home, pressure_procs.len(), None, || {
                captured += 1;
                Ok(Vec::new())
            })
            .unwrap()
        );
        assert_eq!(captured, 1, "one snapshot per pressure episode");
        let contents = std::fs::read(dir.path().join(MACHINE_SNAPSHOT_NAME)).unwrap();
        assert!(contents.starts_with(b"captured_at: "));
        let ps_header = b"PID PPID RSS ELAPSED COMMAND";
        assert!(contents
            .windows(ps_header.len())
            .any(|window| window == ps_header));
        assert!(contents.len() <= MAX_MACHINE_SNAPSHOT_BYTES);
        assert!(dir.path().join(MACHINE_SNAPSHOT_ACTIVE_NAME).exists());
        assert!(!maybe_capture_process_snapshot(&home, 199, Some(100), || {
            captured += 1;
            Ok(Vec::new())
        })
        .unwrap());
        assert!(dir.path().join(MACHINE_SNAPSHOT_NAME).exists());
        assert!(!dir.path().join(MACHINE_SNAPSHOT_ACTIVE_NAME).exists());
        assert!(maybe_capture_process_snapshot(&home, 201, Some(100), || {
            captured += 1;
            Ok(b"baseline threshold crossed\n".to_vec())
        })
        .unwrap());
        assert_eq!(
            captured, 2,
            "a later pressure episode gets one new snapshot"
        );
    }
}
