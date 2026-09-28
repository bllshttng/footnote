//! Reader cost over a window: CPU-seconds per process class, with the load
//! average beside it, for the x-a973 question: does the fleet's own reading
//! track the machine's load? One inbound caller: `crate::intel::run_intel`
//! (`fno-agents intel --readers <secs>`). On-demand sampler, never a watcher.

use serde::Serialize;

/// Cumulative CPU time of one pid in nanoseconds, `None` when unreadable
/// (exited, kernel task, other uid): callers count it, never read it as
/// zero. The macOS read is mach ticks, converted with the shared timebase
/// (an unconverted read overstates ~41x on Apple Silicon).
#[cfg(target_os = "macos")]
pub fn cpu_time_ns(pid: u32) -> Option<u64> {
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            &mut task as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        return None;
    }
    Some(crate::mach_ticks_to_ns(
        task.pti_total_user.saturating_add(task.pti_total_system),
    ))
}

/// Linux: fields 14 (utime) and 15 (stime) of /proc/<pid>/stat, in clock
/// ticks over _SC_CLK_TCK.
#[cfg(target_os = "linux")]
pub fn cpu_time_ns(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some(utime.saturating_add(stime) * 1_000_000_000 / hz)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn cpu_time_ns(_pid: u32) -> Option<u64> {
    None
}

/// Program basenames whose rows are the fleet's own readers; the class is
/// the basename plus the first two non-flag argv tokens.
const FNO_PROGRAMS: [&str; 8] = [
    "fno",
    "fno-py",
    "fno-agents",
    "fno-agents-daemon",
    "fno-agents-worker",
    "fno-footprint-cause",
    "fno-gh-proxy",
    "macmon",
];

/// Build tools fold to one class.
const BUILD_TOOLS: [&str; 4] = ["rustc", "cargo", "cc", "ld"];

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Classify one command line by TOKEN match, never substring (the retired
/// sampler's counting rule): a wrapper whose own line carries a pattern never
/// becomes that class, because only the program basename is matched.
/// `python3 .../fno-py agents truth` classifies by the script basename.
pub fn classify(argv: &str) -> String {
    let tokens: Vec<&str> = argv.split_whitespace().collect();
    let Some(program) = tokens.first() else {
        return "other:unknown".to_string();
    };
    let base = basename(program);
    let (prog_base, rest): (&str, &[&str]) = if base.starts_with("python") {
        match tokens[1..]
            .iter()
            .position(|t| FNO_PROGRAMS.contains(&basename(t)))
        {
            Some(i) => (basename(tokens[i + 1]), &tokens[i + 2..]),
            None => return format!("other:{base}"),
        }
    } else {
        (base, &tokens[1..])
    };
    if BUILD_TOOLS.contains(&prog_base) {
        return "build".to_string();
    }
    if FNO_PROGRAMS.contains(&prog_base) {
        let mut parts = vec![prog_base.to_string()];
        for token in rest.iter().filter(|t| !t.starts_with('-')) {
            parts.push((*token).to_string());
            if parts.len() == 3 {
                break;
            }
        }
        return parts.join(" ");
    }
    format!("other:{prog_base}")
}

/// The fleet's own readers: every class classify() named after an
/// FNO_PROGRAMS entry (directly or hosted by python). `build` and `other:*`
/// are not.
fn is_fno_class(class: &str) -> bool {
    !class.starts_with("other:") && class != "build"
}

/// One process sighting.
#[derive(Debug, Clone)]
pub struct Row {
    pub pid: u32,
    pub class: String,
    pub cpu_ns: u64,
}

/// One pass over the process table.
#[derive(Debug, Clone)]
pub struct Sample {
    pub t_ms: u64,
    pub load_1m: f64,
    pub rows: Vec<Row>,
}

#[derive(Debug, Serialize)]
pub struct ClassStat {
    pub class: String,
    pub cpu_s: f64,
    pub share: f64,
    pub births: u64,
    pub max_concurrent: u64,
    pub mean_life_s: f64,
}

#[derive(Debug, Serialize)]
pub struct BucketStat {
    pub kind: &'static str,
    pub t_start_ms: u64,
    pub load_1m: f64,
    pub machine_cpu_s: f64,
    pub fno_cpu_s: f64,
    pub build_cpu_s: f64,
    pub fno_procs: u64,
}

#[derive(Debug, Serialize)]
pub struct LoadRange {
    pub first: f64,
    pub last: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub kind: &'static str,
    pub window_s: u64,
    pub every_ms: u64,
    pub samples: usize,
    pub capacity_cores: f64,
    pub machine_cpu_s: f64,
    pub busy_fraction: f64,
    pub load_1m: LoadRange,
    pub fno_cpu_s: f64,
    pub fno_share: f64,
    pub build_cpu_s: f64,
    pub r_load_fno: Option<f64>,
    pub r_load_build: Option<f64>,
    pub edge: &'static str,
    pub classes: Vec<ClassStat>,
    pub observer_cpu_s: f64,
    pub unreadable: u64,
    pub sampled_births: u64,
    pub missed_cpu_bound_s: f64,
}

struct PidTrack {
    first_idx: usize,
    first_ns: u64,
    last_ns: u64,
    first_t_ms: u64,
    last_t_ms: u64,
}

/// Streaming 10 s bucket accumulator. A pid's step delta is its CPU reading
/// minus the reading at its previous sighting; a pid never seen before counts
/// its whole reading, except at sample 0 where the pre-window baseline is
/// unknown and the step counts 0. Steps land in the bucket of the later
/// sighting, so bucket totals always sum to the window totals.
struct BucketAccumulator {
    t0: u64,
    first_push_done: bool,
    prev_ns: std::collections::HashMap<u32, u64>,
    partials: Vec<Partial>,
}

struct Partial {
    t_start_ms: u64,
    machine: f64,
    fno: f64,
    build: f64,
    fno_procs: u64,
    load_sum: f64,
    load_n: u64,
}

const BUCKET_MS: u64 = 10_000;

impl BucketAccumulator {
    fn new(t0: u64) -> Self {
        BucketAccumulator {
            t0,
            first_push_done: false,
            prev_ns: std::collections::HashMap::new(),
            partials: Vec::new(),
        }
    }

    fn push(&mut self, sample: &Sample) {
        let b_idx = (sample.t_ms.saturating_sub(self.t0) / BUCKET_MS) as usize;
        while self.partials.len() <= b_idx {
            let n = self.partials.len() as u64;
            self.partials.push(Partial {
                t_start_ms: self.t0 + n * BUCKET_MS,
                machine: 0.0,
                fno: 0.0,
                build: 0.0,
                fno_procs: 0,
                load_sum: 0.0,
                load_n: 0,
            });
        }
        let partial = &mut self.partials[b_idx];
        partial.load_sum += sample.load_1m;
        partial.load_n += 1;
        let fno_here = sample
            .rows
            .iter()
            .filter(|r| is_fno_class(&r.class))
            .count() as u64;
        if fno_here > partial.fno_procs {
            partial.fno_procs = fno_here;
        }
        for row in &sample.rows {
            let prev = self.prev_ns.insert(row.pid, row.cpu_ns);
            let delta_ns = match prev {
                Some(p) => row.cpu_ns.saturating_sub(p),
                None => {
                    if !self.first_push_done {
                        0
                    } else {
                        row.cpu_ns
                    }
                }
            };
            let delta_s = delta_ns as f64 / 1e9;
            partial.machine += delta_s;
            if is_fno_class(&row.class) {
                partial.fno += delta_s;
            } else if row.class == "build" {
                partial.build += delta_s;
            }
        }
        self.first_push_done = true;
    }

    fn finish(&self) -> Vec<BucketStat> {
        self.partials
            .iter()
            .map(|p| BucketStat {
                kind: "reader_cost_bucket",
                t_start_ms: p.t_start_ms,
                load_1m: if p.load_n > 0 {
                    p.load_sum / p.load_n as f64
                } else {
                    0.0
                },
                machine_cpu_s: p.machine,
                fno_cpu_s: p.fno,
                build_cpu_s: p.build,
                fno_procs: p.fno_procs,
            })
            .collect()
    }
}

fn buckets_from(samples: &[Sample]) -> Vec<BucketStat> {
    let Some(t0) = samples.first().map(|s| s.t_ms) else {
        return Vec::new();
    };
    let mut acc = BucketAccumulator::new(t0);
    for s in samples.iter() {
        acc.push(s);
    }
    acc.finish()
}

fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 6 {
        return None;
    }
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx) * (x - mx);
        syy += (y - my) * (y - my);
    }
    if sxx <= f64::EPSILON || syy <= f64::EPSILON {
        return None;
    }
    Some(sxy / (sxx.sqrt() * syy.sqrt()))
}

/// A rising edge is a load above the first reading by over 10 percent, a
/// falling edge a load below it; both names a window holding both.
fn edge_of(loads: &[f64]) -> &'static str {
    let Some(first) = loads.first() else {
        return "none";
    };
    let scale = first.abs().max(1.0);
    let rose = loads.iter().any(|l| l - first > 0.1 * scale);
    let fell = loads.iter().any(|l| first - l > 0.1 * scale);
    match (rose, fell) {
        (true, true) => "both",
        (true, false) => "rising",
        (false, true) => "falling",
        (false, false) => "none",
    }
}

/// The pure fold: per-class CPU-seconds, births, concurrency and life;
/// per-bucket machine/fno/build CPU beside load; the load correlations and
/// the edge. `observer_cpu_s` and `unreadable` arrive from the sampler.
pub fn fold(
    samples: &[Sample],
    capacity_cores: f64,
    window_s: u64,
    every_ms: u64,
    observer_cpu_s: f64,
    unreadable: u64,
) -> Summary {
    let mut per_class: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<u32, PidTrack>,
    > = Default::default();
    let mut max_conc: std::collections::BTreeMap<String, u64> = Default::default();
    for (idx, s) in samples.iter().enumerate() {
        let mut per_class_here: std::collections::BTreeMap<&str, u64> = Default::default();
        for row in &s.rows {
            let track = per_class
                .entry(row.class.clone())
                .or_default()
                .entry(row.pid)
                .or_insert(PidTrack {
                    first_idx: idx,
                    first_ns: row.cpu_ns,
                    last_ns: row.cpu_ns,
                    first_t_ms: s.t_ms,
                    last_t_ms: s.t_ms,
                });
            track.last_ns = row.cpu_ns;
            track.last_t_ms = s.t_ms;
            *per_class_here.entry(row.class.as_str()).or_default() += 1;
        }
        for (class, n) in per_class_here {
            let slot = max_conc.entry(class.to_string()).or_default();
            if n > *slot {
                *slot = n;
            }
        }
    }
    let mut classes: Vec<ClassStat> = Vec::new();
    let mut machine_cpu_s = 0.0;
    let mut sampled_births = 0u64;
    for (class, pids) in &per_class {
        let mut cpu_ticks = 0u64;
        let mut births = 0u64;
        let mut life_sum = 0.0;
        for t in pids.values() {
            let ns = if t.first_idx == 0 {
                t.last_ns.saturating_sub(t.first_ns)
            } else {
                t.last_ns
            };
            cpu_ticks += ns;
            if t.first_idx > 0 {
                births += 1;
            }
            life_sum += (t.last_t_ms.saturating_sub(t.first_t_ms)) as f64 / 1000.0;
        }
        let cpu_s = cpu_ticks as f64 / 1e9;
        machine_cpu_s += cpu_s;
        sampled_births += births;
        classes.push(ClassStat {
            class: class.clone(),
            cpu_s,
            share: 0.0,
            births,
            max_concurrent: *max_conc.get(class).unwrap_or(&0),
            mean_life_s: if pids.is_empty() {
                0.0
            } else {
                life_sum / pids.len() as f64
            },
        });
    }
    let fno_cpu_s: f64 = classes
        .iter()
        .filter(|c| is_fno_class(&c.class))
        .map(|c| c.cpu_s)
        .sum();
    let build_cpu_s: f64 = classes
        .iter()
        .filter(|c| c.class == "build")
        .map(|c| c.cpu_s)
        .sum();
    for c in &mut classes {
        c.share = if machine_cpu_s > 0.0 {
            c.cpu_s / machine_cpu_s
        } else {
            0.0
        };
    }
    classes.sort_by(|a, b| b.cpu_s.total_cmp(&a.cpu_s));
    let buckets = buckets_from(samples);
    let loads: Vec<f64> = samples.iter().map(|s| s.load_1m).collect();
    let (load_first, load_last) = (
        loads.first().copied().unwrap_or(0.0),
        loads.last().copied().unwrap_or(0.0),
    );
    let load_min = loads.iter().cloned().fold(f64::INFINITY, f64::min);
    let load_max = loads.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let r_load_fno = pearson(
        &buckets.iter().map(|b| b.load_1m).collect::<Vec<_>>(),
        &buckets.iter().map(|b| b.fno_cpu_s).collect::<Vec<_>>(),
    );
    let r_load_build = pearson(
        &buckets.iter().map(|b| b.load_1m).collect::<Vec<_>>(),
        &buckets.iter().map(|b| b.build_cpu_s).collect::<Vec<_>>(),
    );
    Summary {
        kind: "reader_cost_summary",
        window_s,
        every_ms,
        samples: samples.len(),
        capacity_cores,
        machine_cpu_s,
        busy_fraction: if window_s > 0 {
            machine_cpu_s / (capacity_cores * window_s as f64)
        } else {
            0.0
        },
        load_1m: LoadRange {
            first: load_first,
            last: load_last,
            min: if loads.is_empty() { 0.0 } else { load_min },
            max: if loads.is_empty() { 0.0 } else { load_max },
        },
        fno_cpu_s,
        fno_share: if machine_cpu_s > 0.0 {
            fno_cpu_s / machine_cpu_s
        } else {
            0.0
        },
        build_cpu_s,
        r_load_fno,
        r_load_build,
        edge: edge_of(&loads),
        classes,
        observer_cpu_s,
        unreadable,
        sampled_births,
        missed_cpu_bound_s: sampled_births as f64 * (every_ms as f64 / 1000.0),
    }
}

#[derive(Debug)]
pub struct Config {
    pub window_s: u64,
    pub every_ms: u64,
    pub json: bool,
}

fn parse_window(v: &str) -> Result<u64, String> {
    match v.parse::<u64>() {
        Ok(0) => Err(format!(
            "invalid --readers value '0': window must be 10 to 3600 seconds (1 to 9 runs a smoke window)"
        )),
        Ok(n) if n > 3600 => Err(format!(
            "invalid --readers value '{v}': window must be 10 to 3600 seconds"
        )),
        Ok(n) => Ok(n),
        Err(_) => Err(format!(
            "invalid --readers value '{v}': must be an integer of seconds in the 10 to 3600 range"
        )),
    }
}

pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut window: Option<u64> = None;
    let mut every = 1000u64;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--readers" => {
                i += 1;
                let v = args.get(i).ok_or("--readers needs a value in seconds")?;
                window = Some(parse_window(v)?);
            }
            "--every-ms" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--every-ms needs a value in milliseconds")?;
                let n = v.parse::<u64>().map_err(|_| {
                    format!("invalid --every-ms value '{v}': must be an integer of milliseconds")
                })?;
                every = n.max(250);
            }
            "--json" => json = true,
            _ => {}
        }
        i += 1;
    }
    let window_s = window.ok_or(
        "missing --readers <secs>: the window must be 10 to 3600 seconds (1 to 9 runs a smoke window)",
    )?;
    Ok(Config {
        window_s,
        every_ms: every,
        json,
    })
}

fn capacity_cores() -> f64 {
    // SAFETY: _SC_NPROCESSORS_ONLN is a standard sysconf name.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n <= 0 {
        1.0
    } else {
        n as f64
    }
}

fn load_1m() -> f64 {
    let mut avg = 0.0f64;
    // SAFETY: a one-element array owned by this frame.
    let n = unsafe { libc::getloadavg(&mut avg, 1) };
    if n < 1 {
        0.0
    } else {
        avg
    }
}

fn system_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One pass over the table: pid, class, cpu time. Lighter than
/// census::process_table, which also walks every thread of every pid for
/// state and cpu_pct this probe does not use.
#[cfg(target_os = "macos")]
fn sample_rows() -> (Vec<Row>, u64) {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return (Vec::new(), 0);
    }
    let mut pids = vec![0u32; count as usize + 64];
    let filled = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast::<libc::c_void>(),
            (pids.len() * std::mem::size_of::<u32>()) as libc::c_int,
        )
    };
    if filled <= 0 {
        return (Vec::new(), 0);
    }
    let mut rows = Vec::new();
    let mut unreadable = 0u64;
    for &pid in &pids[..filled as usize] {
        let Some(argv) = crate::census::process_argv(pid) else {
            unreadable += 1;
            continue;
        };
        let Some(cpu_ns) = cpu_time_ns(pid) else {
            unreadable += 1;
            continue;
        };
        rows.push(Row {
            pid,
            class: classify(&argv.join(" ")),
            cpu_ns,
        });
    }
    (rows, unreadable)
}

#[cfg(target_os = "linux")]
fn sample_rows() -> (Vec<Row>, u64) {
    let mut rows = Vec::new();
    let mut unreadable = 0u64;
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return (rows, 0);
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(cpu_ns) = cpu_time_ns(pid) else {
            unreadable += 1;
            continue;
        };
        let Ok(cmd) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            unreadable += 1;
            continue;
        };
        let argv: Vec<&str> = cmd.split('\0').filter(|t| !t.is_empty()).collect();
        if argv.is_empty() {
            unreadable += 1;
            continue;
        }
        rows.push(Row {
            pid,
            class: classify(&argv.join(" ")),
            cpu_ns,
        });
    }
    (rows, unreadable)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample_rows() -> (Vec<Row>, u64) {
    (Vec::new(), 0)
}

pub fn run(args: &[String]) -> i32 {
    let config = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    run_config(&config)
}

fn run_config(config: &Config) -> i32 {
    let capacity = capacity_cores();
    let observer_start = cpu_time_ns(std::process::id());
    let started = std::time::Instant::now();
    let mut samples: Vec<Sample> = Vec::new();
    let mut unreadable = 0u64;
    let t0 = system_ms();
    let mut acc = BucketAccumulator::new(t0);
    let mut printed = 0usize;
    loop {
        let (rows, unr) = sample_rows();
        unreadable += unr;
        let sample = Sample {
            t_ms: system_ms(),
            load_1m: load_1m(),
            rows,
        };
        acc.push(&sample);
        samples.push(sample);
        while printed + 1 < acc.partials.len() {
            print_bucket(&acc.finish()[printed]);
            printed += 1;
        }
        if started.elapsed().as_secs() >= config.window_s {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(config.every_ms));
    }
    for b in &acc.finish()[printed..] {
        print_bucket(b);
    }
    let observer_cpu_s = observer_start
        .zip(cpu_time_ns(std::process::id()))
        .map(|(a, b)| b.saturating_sub(a) as f64 / 1e9)
        .unwrap_or(0.0);
    let summary = fold(
        &samples,
        capacity,
        config.window_s,
        config.every_ms,
        observer_cpu_s,
        unreadable,
    );
    if config.json {
        println!("{}", serde_json::to_string(&summary).unwrap_or_default());
    } else {
        println!(
            "{:<44} {:>9} {:>7} {:>6} {:>6}",
            "class", "cpu_s", "share", "births", "concur"
        );
        for c in &summary.classes {
            println!(
                "{:<44} {:>9.1} {:>7.3} {:>6} {:>6}",
                c.class, c.cpu_s, c.share, c.births, c.max_concurrent
            );
        }
        println!(
            "window {}s machine {:.1}s fno {:.1}s ({:.1}%) build {:.1}s edge {}",
            config.window_s,
            summary.machine_cpu_s,
            summary.fno_cpu_s,
            summary.fno_share * 100.0,
            summary.build_cpu_s,
            summary.edge
        );
    }
    0
}

fn print_bucket(b: &BucketStat) {
    println!("{}", serde_json::to_string(b).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, class: &str, cpu_s: f64) -> Row {
        Row {
            pid,
            class: class.to_string(),
            cpu_ns: (cpu_s * 1e9) as u64,
        }
    }

    fn sample(t_ms: u64, load: f64, rows: Vec<Row>) -> Sample {
        Sample {
            t_ms,
            load_1m: load,
            rows,
        }
    }

    #[test]
    fn fold_attributes_per_pid_cpu_deltas_to_classes() {
        let samples = vec![
            sample(
                0,
                600.0,
                vec![row(101, "fno-py agents truth", 1.0), row(202, "build", 0.0)],
            ),
            sample(
                1000,
                690.0,
                vec![
                    row(101, "fno-py agents truth", 2.25),
                    row(202, "build", 5.0),
                ],
            ),
            sample(
                2000,
                700.0,
                vec![
                    row(101, "fno-py agents truth", 3.5),
                    row(202, "build", 10.0),
                ],
            ),
        ];
        let s = fold(&samples, 12.0, 2, 1000, 0.0, 0);
        let truth = s
            .classes
            .iter()
            .find(|c| c.class == "fno-py agents truth")
            .unwrap();
        let build = s.classes.iter().find(|c| c.class == "build").unwrap();
        assert!((truth.cpu_s - 2.5).abs() < 1e-9, "truth {}", truth.cpu_s);
        assert!((build.cpu_s - 10.0).abs() < 1e-9, "build {}", build.cpu_s);
        assert!((s.fno_share - 2.5 / 12.5).abs() < 1e-9);
        assert!((s.machine_cpu_s - 12.5).abs() < 1e-9);
        assert!(s.r_load_fno.is_none(), "one bucket cannot correlate");
        assert_eq!(s.edge, "rising");
    }

    #[test]
    fn wrapper_argv_never_becomes_an_fno_class() {
        assert_eq!(
            classify("/bin/zsh -c 'fno-py agents truth --handles x'"),
            "other:zsh"
        );
    }

    #[test]
    fn wrapper_shell_quote_never_becomes_an_fno_class() {
        let wrapper = r#"/bin/zsh -c 'ps -Ao command | grep "truth --handles"'"#;
        assert_eq!(classify(wrapper), "other:zsh");
    }

    #[test]
    fn python_hosted_scripts_classify_by_script_basename() {
        assert_eq!(
            classify(
                "python3 /Users/x/.local/share/uv/tools/fno/bin/fno-py agents truth --handles a --json"
            ),
            "fno-py agents truth"
        );
        assert_eq!(classify("python3 -c print(1)"), "other:python3");
    }

    #[test]
    fn build_tools_fold_and_flags_are_skipped() {
        assert_eq!(classify("/usr/bin/rustc --crate-name x"), "build");
        assert_eq!(classify("fno backlog get x-a973"), "fno backlog get");
        assert_eq!(
            classify("fno-agents-daemon --home /Users/op/.fno/agents"),
            "fno-agents-daemon /Users/op/.fno/agents"
        );
    }

    #[test]
    fn births_concurrency_and_life_track_the_window() {
        let samples = vec![
            sample(0, 5.0, vec![row(1, "fno-py agents truth", 1.0)]),
            sample(
                1000,
                5.0,
                vec![
                    row(1, "fno-py agents truth", 2.0),
                    row(2, "fno-py agents truth", 4.0),
                ],
            ),
            sample(
                2000,
                5.0,
                vec![
                    row(1, "fno-py agents truth", 3.0),
                    row(2, "fno-py agents truth", 5.0),
                ],
            ),
        ];
        let s = fold(&samples, 12.0, 2, 1000, 0.0, 0);
        let c = s.classes.first().unwrap();
        assert_eq!(c.births, 1, "pid 2 was born after sample 0");
        assert_eq!(c.max_concurrent, 2);
        assert!((c.mean_life_s - 1.5).abs() < 1e-9);
        assert!(
            (c.cpu_s - 7.0).abs() < 1e-9,
            "pid1 2.0 window + pid2 5.0 born: {}",
            c.cpu_s
        );
        assert_eq!(s.sampled_births, 1);
        assert!((s.missed_cpu_bound_s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn edge_and_correlations_follow_the_load_series() {
        let mut samples = Vec::new();
        for i in 0..12u32 {
            let load = 10.0 * (i as f64 + 1.0);
            // One reader born per bucket, reading its whole (growing) life
            // at first sighting: bucket CPU then tracks the birth rate.
            samples.push(sample(
                (i as u64) * 10_000,
                load,
                vec![row(10 + i, "fno-py agents truth", i as f64 + 1.0)],
            ));
        }
        let s = fold(&samples, 12.0, 120, 1000, 0.0, 0);
        assert_eq!(s.edge, "rising");
        let r = s.r_load_fno.expect("12 buckets correlate");
        assert!(r > 0.99, "r {r}");
        assert!(
            s.r_load_build.is_none(),
            "flat build series has no variance"
        );
    }

    #[test]
    fn flat_load_gives_null_correlations_and_no_edge() {
        let mut samples = Vec::new();
        for i in 0..12u64 {
            samples.push(sample(
                i * 10_000,
                400.0,
                vec![row(1, "fno-py agents truth", i as f64)],
            ));
        }
        let s = fold(&samples, 12.0, 120, 1000, 0.0, 0);
        assert_eq!(s.edge, "none");
        assert!(s.r_load_fno.is_none());
    }

    #[test]
    fn bucket_totals_sum_to_class_totals_across_buckets() {
        let samples = vec![
            sample(0, 5.0, vec![row(1, "fno-py agents truth", 1.0)]),
            sample(10_000, 5.0, vec![row(1, "fno-py agents truth", 3.0)]),
            sample(
                15_000,
                5.0,
                vec![
                    row(1, "fno-py agents truth", 4.0),
                    row(2, "fno-py agents truth", 2.0),
                ],
            ),
            sample(20_000, 5.0, vec![row(1, "fno-py agents truth", 6.0)]),
        ];
        let s = fold(&samples, 12.0, 21, 1000, 0.0, 0);
        let c = s.classes.first().unwrap();
        assert!((c.cpu_s - 7.0).abs() < 1e-9, "5.0 resident + 2.0 born");
        assert!((s.machine_cpu_s - c.cpu_s).abs() < 1e-9);
    }

    #[test]
    fn invalid_window_refuses_with_value_and_range() {
        let e0 = parse_window("0").unwrap_err();
        assert!(e0.contains("10 to 3600"), "{e0}");
        let e = parse_window("abc").unwrap_err();
        assert!(e.contains("abc") && e.contains("10 to 3600"), "{e}");
        assert!(parse_window("3700").is_err());
        assert!(parse_window("5").is_ok(), "1 to 9 runs a smoke window");
    }

    #[test]
    fn missing_readers_refuses() {
        let args: Vec<String> = vec!["--json".to_string()];
        let e = parse_args(&args).unwrap_err();
        assert!(e.contains("--readers"), "{e}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cpu_time_ns_agrees_with_getrusage_within_20_percent() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let mut x = 1u64;
        while std::time::Instant::now() < deadline {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            std::hint::black_box(x);
        }
        let proc_ns = cpu_time_ns(std::process::id()).expect("own pid readable");
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: a valid, owned rusage the kernel fills.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        assert_eq!(rc, 0);
        let rusage_ns = ((ru.ru_utime.tv_sec + ru.ru_stime.tv_sec) as f64 * 1e9)
            + ((ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) as f64 * 1e3);
        let rel = (proc_ns as f64 - rusage_ns).abs() / rusage_ns.max(1.0);
        assert!(rel <= 0.2, "procinfo {proc_ns} vs rusage {rusage_ns}");
    }
}
