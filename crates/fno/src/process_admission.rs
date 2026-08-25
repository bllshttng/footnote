use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// The process budget that a launch consumes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Fleet,
    Tab,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fleet => "fleet",
            Self::Tab => "tab",
        }
    }
}

/// The already-resolved ceiling for one admission decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionLimits {
    pub scope: Scope,
    pub ceiling: usize,
}

impl AdmissionLimits {
    pub const fn fleet(ceiling: usize) -> Self {
        Self {
            scope: Scope::Fleet,
            ceiling,
        }
    }

    pub const fn tab(ceiling: usize) -> Self {
        Self {
            scope: Scope::Tab,
            ceiling,
        }
    }
}

/// A process census is complete only when its count is known. Unknown input is
/// deliberately distinct from an empty snapshot so it can never become free
/// headroom.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Census {
    Complete { count: usize },
    Unavailable { reason: String },
}

impl Census {
    pub const fn complete(count: usize) -> Self {
        Self::Complete { count }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionReason {
    OverLimit,
    MeasurementUnavailable,
    LockUnavailable,
}

/// The result of one pre-spawn decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    Admit,
    Refuse {
        count: Option<usize>,
        ceiling: usize,
        scope: Scope,
        reason: AdmissionReason,
    },
}

/// A refusal that can cross a process-launch API without becoming a stringly
/// typed success or an empty error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionFailure {
    decision: AdmissionDecision,
    detail: String,
}

impl AdmissionFailure {
    pub fn decision(&self) -> &AdmissionDecision {
        &self.decision
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for AdmissionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.decision.fmt(f)?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for AdmissionFailure {}

/// The lock is held from the census through the child-creation syscall. It is
/// intentionally released before the child lifetime begins.
pub struct AdmissionPermit {
    _lock: File,
    scope: Scope,
    count: usize,
    ceiling: usize,
}

impl AdmissionPermit {
    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn ceiling(&self) -> usize {
        self.ceiling
    }

    pub(crate) fn record_child(&self, pid: u32) -> io::Result<()> {
        let dir = crate::proto::mux_dir();
        crate::proto::ensure_private_dir(&dir)?;
        let path = dir.join(CHILD_MARKERS_FILE);
        let mut markers = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(markers, "{pid}")
    }
}

impl AdmissionDecision {
    pub fn scope(&self) -> Option<Scope> {
        match self {
            Self::Admit => None,
            Self::Refuse { scope, .. } => Some(*scope),
        }
    }

    pub fn refusal(&self) -> Option<String> {
        let Self::Refuse {
            count,
            ceiling,
            scope,
            reason,
        } = self
        else {
            return None;
        };
        let count = count
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".into());
        let reason = match reason {
            AdmissionReason::OverLimit => "over-limit",
            AdmissionReason::MeasurementUnavailable => "measurement-unavailable",
            AdmissionReason::LockUnavailable => "lock-unavailable",
        };
        Some(format!(
            "process admission refused: count={count} ceiling={ceiling} scope={} reason={reason}",
            scope.as_str()
        ))
    }
}

impl fmt::Display for AdmissionDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.refusal() {
            Some(message) => f.write_str(&message),
            None => f.write_str("process admission admitted"),
        }
    }
}

pub fn decide(census: &Census, limits: AdmissionLimits) -> AdmissionDecision {
    match census {
        Census::Complete { count } if *count < limits.ceiling => AdmissionDecision::Admit,
        Census::Complete { count } => AdmissionDecision::Refuse {
            count: Some(*count),
            ceiling: limits.ceiling,
            scope: limits.scope,
            reason: AdmissionReason::OverLimit,
        },
        Census::Unavailable { .. } => AdmissionDecision::Refuse {
            count: None,
            ceiling: limits.ceiling,
            scope: limits.scope,
            reason: AdmissionReason::MeasurementUnavailable,
        },
    }
}

pub const DEFAULT_MAX_LIVE: usize = 3;
pub const DEFAULT_PANE_GROUP_MAX: usize = 4;
const LOCK_FILE: &str = ".process-admission.lock";
const CHILD_MARKERS_FILE: &str = ".process-admission.children";

/// Read the already-resolved fleet cap carried by the Python launcher. A
/// direct Rust client uses the same conservative default as Python.
pub fn configured_max_live() -> Result<usize, String> {
    match std::env::var("FNO_MUX_MAX_LIVE") {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "agents.max_live is missing or invalid".into()),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_MAX_LIVE),
        Err(std::env::VarError::NotUnicode(_)) => Err("agents.max_live is not valid UTF-8".into()),
    }
}

/// Resolve a legacy or omitted wire cap to a bounded effective value.
pub fn configured_pane_group_max(requested: Option<usize>) -> usize {
    requested
        .filter(|value| *value > 0)
        .or_else(|| {
            std::env::var("FNO_MUX_PANE_GROUP_MAX")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .filter(|value| *value > 0)
        })
        .unwrap_or(DEFAULT_PANE_GROUP_MAX)
}

/// Acquire the machine-global admission lock, measure the relevant process
/// tree, and return a permit that must remain alive through the spawn syscall.
pub fn admit_fleet() -> Result<AdmissionPermit, AdmissionFailure> {
    let (ceiling, config_error) = match configured_max_live() {
        Ok(value) => (value, None),
        Err(error) => (DEFAULT_MAX_LIVE, Some(error)),
    };
    if let Some(detail) = config_error {
        return Err(AdmissionFailure {
            decision: AdmissionDecision::Refuse {
                count: None,
                ceiling,
                scope: Scope::Fleet,
                reason: AdmissionReason::MeasurementUnavailable,
            },
            detail,
        });
    }
    #[cfg(test)]
    if std::env::var_os("FNO_MUX_NATIVE_TEST_ADMISSION").is_none() {
        return Ok(test_permit(Scope::Fleet, 0, ceiling));
    }
    let lock = acquire_lock().map_err(|detail| AdmissionFailure {
        decision: AdmissionDecision::Refuse {
            count: None,
            ceiling,
            scope: Scope::Fleet,
            reason: AdmissionReason::LockUnavailable,
        },
        detail,
    })?;
    let census = process_census();
    let decision = decide(&census, AdmissionLimits::fleet(ceiling));
    match decision {
        AdmissionDecision::Admit => Ok(AdmissionPermit {
            _lock: lock,
            scope: Scope::Fleet,
            count: census.count().expect("admitted census has a count"),
            ceiling,
        }),
        decision => Err(AdmissionFailure {
            decision,
            detail: census.reason().unwrap_or_default().to_string(),
        }),
    }
}

/// Acquire the same machine lock for a pane-tab decision. The pane count is
/// read from the serialized server state before a PTY is opened or a child is
/// created.
pub fn admit_tab(
    pane_count: usize,
    requested_cap: Option<usize>,
) -> Result<AdmissionPermit, AdmissionFailure> {
    let ceiling = configured_pane_group_max(requested_cap);
    #[cfg(test)]
    if std::env::var_os("FNO_MUX_NATIVE_TEST_ADMISSION").is_none() {
        return Ok(test_permit(Scope::Tab, pane_count, ceiling));
    }
    let lock = acquire_lock().map_err(|detail| AdmissionFailure {
        decision: AdmissionDecision::Refuse {
            count: None,
            ceiling,
            scope: Scope::Tab,
            reason: AdmissionReason::LockUnavailable,
        },
        detail,
    })?;
    let census = Census::complete(pane_count);
    let decision = decide(&census, AdmissionLimits::tab(ceiling));
    match decision {
        AdmissionDecision::Admit => Ok(AdmissionPermit {
            _lock: lock,
            scope: Scope::Tab,
            count: pane_count,
            ceiling,
        }),
        decision => Err(AdmissionFailure {
            decision,
            detail: String::new(),
        }),
    }
}

/// One permit for a pane launch. Fleet admission is evaluated first, then the
/// target-tab cap, while one machine lock covers both measurements and the
/// eventual child-creation syscall.
pub fn admit_pane(
    pane_count: usize,
    requested_cap: Option<usize>,
) -> Result<AdmissionPermit, AdmissionFailure> {
    let (fleet_ceiling, config_error) = match configured_max_live() {
        Ok(value) => (value, None),
        Err(error) => (DEFAULT_MAX_LIVE, Some(error)),
    };
    let tab_ceiling = configured_pane_group_max(requested_cap);
    if let Some(detail) = config_error {
        return Err(AdmissionFailure {
            decision: AdmissionDecision::Refuse {
                count: None,
                ceiling: fleet_ceiling,
                scope: Scope::Fleet,
                reason: AdmissionReason::MeasurementUnavailable,
            },
            detail,
        });
    }
    #[cfg(test)]
    if std::env::var_os("FNO_MUX_NATIVE_TEST_ADMISSION").is_none() {
        return Ok(test_permit(Scope::Fleet, 0, fleet_ceiling));
    }
    let lock = acquire_lock().map_err(|detail| AdmissionFailure {
        decision: AdmissionDecision::Refuse {
            count: None,
            ceiling: fleet_ceiling,
            scope: Scope::Fleet,
            reason: AdmissionReason::LockUnavailable,
        },
        detail,
    })?;
    let fleet = process_census();
    let fleet_decision = decide(&fleet, AdmissionLimits::fleet(fleet_ceiling));
    if !matches!(fleet_decision, AdmissionDecision::Admit) {
        return Err(AdmissionFailure {
            decision: fleet_decision,
            detail: fleet.reason().unwrap_or_default().to_string(),
        });
    }
    let tab = decide(
        &Census::complete(pane_count),
        AdmissionLimits::tab(tab_ceiling),
    );
    if !matches!(tab, AdmissionDecision::Admit) {
        return Err(AdmissionFailure {
            decision: tab,
            detail: String::new(),
        });
    }
    Ok(AdmissionPermit {
        _lock: lock,
        scope: Scope::Fleet,
        count: fleet.count().expect("admitted census has a count"),
        ceiling: fleet_ceiling,
    })
}

#[cfg(test)]
fn test_permit(scope: Scope, count: usize, ceiling: usize) -> AdmissionPermit {
    AdmissionPermit {
        _lock: File::open(std::env::current_exe().expect("test executable path"))
            .expect("test executable is readable"),
        scope,
        count,
        ceiling,
    }
}

fn admission_io_error(error: AdmissionFailure) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}

pub fn std_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}

pub fn std_spawn(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    let permit = admit_fleet().map_err(admission_io_error)?;
    let mut child = command.spawn()?;
    if let Err(error) = permit.record_child(child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

pub fn std_output(command: &mut std::process::Command) -> io::Result<std::process::Output> {
    let child = std_spawn(command)?;
    child.wait_with_output()
}

pub fn std_status(command: &mut std::process::Command) -> io::Result<std::process::ExitStatus> {
    let mut child = std_spawn(command)?;
    child.wait()
}

#[cfg(unix)]
pub fn std_exec(command: &mut std::process::Command) -> io::Error {
    match admit_fleet() {
        Ok(_permit) => command.exec(),
        Err(error) => admission_io_error(error),
    }
}

pub fn tokio_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    tokio::process::Command::new(program)
}

pub fn tokio_spawn(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    let permit = admit_fleet().map_err(admission_io_error)?;
    let mut child = command.spawn()?;
    if let Some(pid) = child.id() {
        if let Err(error) = permit.record_child(pid) {
            let _ = child.start_kill();
            return Err(error);
        }
    }
    Ok(child)
}

pub async fn tokio_output(
    command: &mut tokio::process::Command,
) -> io::Result<std::process::Output> {
    let child = tokio_spawn(command)?;
    child.wait_with_output().await
}

pub async fn tokio_status(
    command: &mut tokio::process::Command,
) -> io::Result<std::process::ExitStatus> {
    let mut child = tokio_spawn(command)?;
    child.wait().await
}

fn acquire_lock() -> Result<File, String> {
    let dir = crate::proto::mux_dir();
    crate::proto::ensure_private_dir(&dir)
        .map_err(|e| format!("cannot prepare admission state: {e}"))?;
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        loop {
            // SAFETY: `fd` is an open file descriptor owned by `file`; the
            // blocking exclusive lock serializes census plus spawn.
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(format!("cannot lock {}: {error}", path.display()));
            }
        }
    }
    Ok(file)
}

#[derive(Clone, Debug)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    name: String,
}

impl Census {
    fn count(&self) -> Option<usize> {
        match self {
            Self::Complete { count } => Some(*count),
            Self::Unavailable { .. } => None,
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Complete { .. } => None,
            Self::Unavailable { reason } => Some(reason),
        }
    }
}

fn process_census() -> Census {
    let result = snapshot_processes().and_then(|rows| {
        let attributed = attributed_pids(&rows)?;
        let markers = marker_count(&rows, &attributed)?;
        Ok(attributed.len() + markers)
    });
    match result {
        Ok(count) => Census::complete(count),
        Err(reason) => Census::unavailable(reason),
    }
}

fn marker_count(rows: &[ProcessRow], attributed: &HashSet<u32>) -> Result<usize, String> {
    let path = crate::proto::mux_dir().join(CHILD_MARKERS_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("child marker ledger unavailable: {error}")),
    };
    let roots = process_root_names()?;
    let names: HashMap<u32, &str> = rows
        .iter()
        .map(|row| (row.pid, row.name.as_str()))
        .collect();
    let mut live = Vec::new();
    let mut seen = HashSet::new();
    let mut unattributed_live = 0;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let pid = line
            .trim()
            .parse::<u32>()
            .map_err(|error| format!("malformed child marker ledger: {error}"))?;
        if !seen.insert(pid) {
            continue;
        }
        let dead = crate::proto::pid_confirmed_dead(pid as libc::pid_t);
        if dead {
            continue;
        }
        live.push(pid);
        let is_root = names.get(&pid).is_some_and(|name| roots.contains(*name));
        if !attributed.contains(&pid) && !is_root {
            unattributed_live += 1;
        }
    }
    let body = live
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    if body.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else if body != raw.trim_end() {
        std::fs::write(&path, format!("{body}\n"))
            .map_err(|error| format!("child marker ledger update failed: {error}"))?;
    }
    Ok(unattributed_live)
}

fn attributed_pids(rows: &[ProcessRow]) -> Result<HashSet<u32>, String> {
    let roots = process_root_names()?;
    let by_pid: HashMap<u32, &ProcessRow> = rows.iter().map(|row| (row.pid, row)).collect();
    let mut attributed = HashSet::new();
    for row in rows {
        // The mux/client/test executable is the admission observer, not a
        // worker slot. Count its descendants, including zombies, so a fresh
        // server can admit two children under a ceiling of two.
        let is_attributed = !roots.contains(&row.name) && reaches_root(row, &by_pid, &roots)?;
        if is_attributed {
            attributed.insert(row.pid);
        }
    }
    Ok(attributed)
}

fn process_root_names() -> Result<HashSet<String>, String> {
    let current_name = std::env::current_exe()
        .map_err(|e| format!("current executable unavailable: {e}"))?
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "current executable name unavailable".to_string())?
        .to_string();
    let mut roots = HashSet::from([current_name.clone()]);
    // A production fno binary sees every fno root so separate mux servers
    // share one machine budget. A cargo-test binary sees only its own subtree,
    // which keeps process-admission fixtures isolated from the operator's live
    // fleet while still exercising the native census and lock.
    if current_name == "fno" || current_name.starts_with("fno-") {
        roots.insert("fno".to_string());
    }
    Ok(roots)
}

fn reaches_root(
    row: &ProcessRow,
    by_pid: &HashMap<u32, &ProcessRow>,
    roots: &HashSet<String>,
) -> Result<bool, String> {
    let mut seen = HashSet::new();
    let mut current = row;
    loop {
        if roots.contains(&current.name) {
            return Ok(true);
        }
        if !seen.insert(current.pid) {
            return Err(format!("process ancestry cycle at pid={}", current.pid));
        }
        let Some(parent) = by_pid.get(&current.ppid) else {
            return Ok(false);
        };
        current = parent;
    }
}

fn snapshot_processes() -> Result<Vec<ProcessRow>, String> {
    #[cfg(target_os = "macos")]
    {
        return snapshot_macos();
    }
    #[cfg(target_os = "linux")]
    {
        return snapshot_linux();
    }
    #[allow(unreachable_code)]
    Err("native process snapshot unavailable on this platform".into())
}

#[cfg(target_os = "macos")]
fn snapshot_macos() -> Result<Vec<ProcessRow>, String> {
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(format!(
            "proc_listallpids failed: {}",
            io::Error::last_os_error()
        ));
    }
    let mut pids = vec![0 as libc::pid_t; needed as usize];
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<libc::pid_t>())
        .map_err(|_| "process snapshot is too large".to_string())?;
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if found < 0 {
        return Err(format!(
            "proc_listallpids failed: {}",
            io::Error::last_os_error()
        ));
    }
    if found as usize > pids.len() {
        return Err("process snapshot changed while being read".into());
    }
    let mut rows = Vec::with_capacity(found as usize);
    for pid in pids.into_iter().take(found as usize).filter(|pid| *pid > 0) {
        let mut name_buf = [0 as libc::c_char; 256];
        let name_len = unsafe {
            libc::proc_name(
                pid,
                name_buf.as_mut_ptr().cast(),
                u32::try_from(name_buf.len()).expect("process name fits in u32"),
            )
        };
        if name_len <= 0 {
            continue;
        }
        let name = c_name(&name_buf);
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                i32::try_from(size).expect("proc_bsdinfo fits in c_int"),
            )
        };
        if got == 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            // proc_listallpids is a point-in-time list. A process that exits
            // between that list and proc_pidinfo is no longer live capacity,
            // not an incomplete measurement.
            continue;
        }
        if got != i32::try_from(size).expect("proc_bsdinfo fits in c_int") {
            // System services owned by another user can expose a name but
            // deny BSD-info reads. They cannot be attributed without a root
            // name, so omit them. A denied fno root remains unknown and fails
            // closed rather than pretending its descendants are absent.
            let current_name = std::env::current_exe().ok().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
            let current_is_fno = current_name
                .as_deref()
                .is_some_and(|value| value == "fno" || value.starts_with("fno-"));
            if current_name.as_deref() != Some(name.as_str()) && !(current_is_fno && name == "fno")
            {
                continue;
            }
            return Err(format!("process snapshot row unavailable for pid={pid}"));
        }
        let info = unsafe { info.assume_init() };
        rows.push(ProcessRow {
            pid: info.pbi_pid,
            ppid: info.pbi_ppid,
            name,
        });
    }
    let current_pid = std::process::id() as libc::pid_t;
    if !rows.iter().any(|row| row.pid == current_pid as u32) {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        let got = unsafe {
            libc::proc_pidinfo(
                current_pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                i32::try_from(size).expect("proc_bsdinfo fits in c_int"),
            )
        };
        if got != i32::try_from(size).expect("proc_bsdinfo fits in c_int") {
            return Err("current process missing from process snapshot".into());
        }
        let info = unsafe { info.assume_init() };
        rows.push(ProcessRow {
            pid: info.pbi_pid,
            ppid: info.pbi_ppid,
            name: std::env::current_exe()
                .ok()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .ok_or_else(|| "current executable name unavailable".to_string())?,
        });
    }
    Ok(rows)
}

#[cfg(target_os = "macos")]
fn c_name(bytes: &[libc::c_char]) -> String {
    let bytes = bytes
        .iter()
        .map(|byte| *byte as u8)
        .take_while(|byte| *byte != 0)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "linux")]
fn snapshot_linux() -> Result<Vec<ProcessRow>, String> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| format!("/proc unavailable: {e}"))? {
        let entry = entry.map_err(|e| format!("/proc entry unavailable: {e}"))?;
        let name = entry.file_name();
        let Some(pid_text) = name
            .to_str()
            .filter(|text| text.chars().all(|c| c.is_ascii_digit()))
        else {
            continue;
        };
        let pid = pid_text
            .parse::<u32>()
            .map_err(|e| format!("invalid /proc pid {pid_text}: {e}"))?;
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "process snapshot row unavailable for pid={pid}: {error}"
                ))
            }
        };
        let Some((comm, rest)) = stat.rsplit_once(") ") else {
            return Err(format!("malformed process snapshot row for pid={pid}"));
        };
        let name = comm
            .split_once(" (")
            .map(|(_, name)| name)
            .unwrap_or(comm)
            .to_string();
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        let Some(ppid_text) = fields.get(1) else {
            return Err(format!("malformed process snapshot row for pid={pid}"));
        };
        let ppid = ppid_text
            .parse::<u32>()
            .map_err(|e| format!("invalid parent pid for pid={pid}: {e}"))?;
        rows.push(ProcessRow { pid, ppid, name });
    }
    if rows.is_empty() {
        return Err("process snapshot is empty".into());
    }
    Ok(rows)
}
