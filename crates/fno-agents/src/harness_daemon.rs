use std::fmt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The state needed to prove that a provider owns the thread outside the
/// calling pane. Provider adapters keep their native state in `raw`; the
/// shared lifecycle only consumes the identity and endpoint fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonState {
    pub raw: serde_json::Value,
    pub pid: Option<u32>,
    pub process_start_time: Option<u64>,
    pub endpoint: String,
    pub incarnation: String,
}

impl DaemonState {
    pub fn new(
        raw: serde_json::Value,
        pid: u32,
        process_start_time: u64,
        endpoint: impl Into<String>,
        incarnation: impl Into<String>,
    ) -> Result<Self, String> {
        if pid == 0 {
            return Err("missing daemon pid".to_string());
        }
        if process_start_time == 0 {
            return Err("missing daemon process start time".to_string());
        }
        let endpoint = endpoint.into();
        if endpoint.is_empty() {
            return Err("missing daemon endpoint".to_string());
        }
        let incarnation = incarnation.into();
        if incarnation.is_empty() {
            return Err("missing daemon incarnation".to_string());
        }
        Ok(Self {
            raw,
            pid: Some(pid),
            process_start_time: Some(process_start_time),
            endpoint,
            incarnation,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonReceipt {
    pub harness: String,
    pub durability: String,
    pub incarnation: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureResult {
    pub state: DaemonState,
    pub receipt: DaemonReceipt,
    pub reused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    Unreadable,
}

impl Liveness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Dead => "dead",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureError(String);

impl EnsureError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for EnsureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EnsureError {}

/// Harness-specific state, health, and replacement hooks. The orchestration
/// owns the lock, the three-valued liveness decision, start-time-guarded reap,
/// readiness validation, state persistence, and receipt shape.
pub trait HarnessDaemonAdapter {
    fn harness(&self) -> &str;
    fn state_path(&self) -> &Path;
    fn lock_path(&self) -> &Path;
    fn parse_state(&self, raw: &str) -> Result<DaemonState, String>;
    fn is_healthy(&self, state: &DaemonState) -> bool;
    fn boot(&self) -> Result<DaemonState, String>;

    fn may_replace(&self) -> bool {
        true
    }

    fn serialize_state(&self, state: &DaemonState) -> Result<String, String> {
        serde_json::to_string(&state.raw).map_err(|error| error.to_string())
    }
}

/// Ensure one provider daemon is healthy and return a positive durability
/// receipt. Missing state is a boot opportunity. A readable but unhealthy
/// record may be replaced. Torn, malformed, or identity-incomplete state is
/// unreadable and fails closed.
pub fn ensure_harness_daemon<A: HarnessDaemonAdapter>(
    adapter: &A,
) -> Result<EnsureResult, EnsureError> {
    let _lock = acquire_lock(adapter.lock_path()).ok_or_else(|| {
        EnsureError::new(format!(
            "harness={} observed=unreadable cannot lock daemon state {}; remedy=repair permissions or remove the lock",
            adapter.harness(),
            adapter.lock_path().display()
        ))
    })?;

    match std::fs::read_to_string(adapter.state_path()) {
        Ok(raw) => {
            let state = adapter.parse_state(&raw).map_err(|reason| {
                EnsureError::new(format!(
                    "harness={} observed={} state={} reason={reason}",
                    adapter.harness(),
                    Liveness::Unreadable.as_str(),
                    adapter.state_path().display()
                ))
            })?;
            if adapter.is_healthy(&state) {
                return Ok(success(adapter.harness(), state, true));
            }
            if !adapter.may_replace() {
                return Err(EnsureError::new(format!(
                    "harness={} observed={} remedy=restart the provider supervisor explicitly",
                    adapter.harness(),
                    Liveness::Dead.as_str()
                )));
            }
            reap_owned_process(&state);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(EnsureError::new(format!(
                "harness={} observed={} state={} reason=read failed: {error}",
                adapter.harness(),
                Liveness::Unreadable.as_str(),
                adapter.state_path().display()
            )));
        }
    }

    let state = adapter.boot().map_err(|reason| {
        EnsureError::new(format!(
            "harness={} observed={} remedy=boot daemon: {reason}",
            adapter.harness(),
            Liveness::Dead.as_str()
        ))
    })?;
    if !adapter.is_healthy(&state) {
        return Err(EnsureError::new(format!(
            "harness={} observed={} remedy=daemon boot did not pass the health probe",
            adapter.harness(),
            Liveness::Dead.as_str()
        )));
    }
    let serialized = adapter.serialize_state(&state).map_err(|reason| {
        EnsureError::new(format!(
            "harness={} observed=unreadable reason=state serialization failed: {reason}",
            adapter.harness()
        ))
    })?;
    write_state(adapter.state_path(), &serialized).map_err(|reason| {
        EnsureError::new(format!(
            "harness={} observed=unreadable reason=state persistence failed: {reason}",
            adapter.harness()
        ))
    })?;
    Ok(success(adapter.harness(), state, false))
}

fn success(harness: &str, state: DaemonState, reused: bool) -> EnsureResult {
    EnsureResult {
        receipt: DaemonReceipt {
            harness: harness.to_string(),
            durability: "daemon-owned".to_string(),
            incarnation: state.incarnation.clone(),
            endpoint: state.endpoint.clone(),
        },
        state,
        reused,
    }
}

fn acquire_lock(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    (rc == 0).then_some(file)
}

fn write_state(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    std::fs::write(&temp, contents).map_err(|error| error.to_string())?;
    std::fs::rename(&temp, path).map_err(|error| error.to_string())
}

fn reap_owned_process(state: &DaemonState) {
    let (Some(pid), Some(start)) = (state.pid, state.process_start_time) else {
        return;
    };
    if crate::daemon::pid_is_ours(pid, Some(start)) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure_harness_daemon, DaemonState, HarnessDaemonAdapter};
    use std::path::{Path, PathBuf};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct FakeAdapter {
        state_path: PathBuf,
        lock_path: PathBuf,
        boots: Arc<AtomicUsize>,
    }

    impl HarnessDaemonAdapter for FakeAdapter {
        fn harness(&self) -> &str {
            "fake"
        }
        fn state_path(&self) -> &Path {
            &self.state_path
        }
        fn lock_path(&self) -> &Path {
            &self.lock_path
        }
        fn parse_state(&self, raw: &str) -> Result<DaemonState, String> {
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            DaemonState::new(value, 42, 9001, "sock://fake", "42:9001")
        }
        fn is_healthy(&self, _state: &DaemonState) -> bool {
            true
        }
        fn boot(&self) -> Result<DaemonState, String> {
            self.boots.fetch_add(1, Ordering::SeqCst);
            DaemonState::new(
                serde_json::json!({"healthy": true}),
                42,
                9001,
                "sock://fake",
                "42:9001",
            )
        }
    }

    #[test]
    fn ac1_hp_healthy_record_is_reused_with_positive_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let lock_path = dir.path().join("state.lock");
        std::fs::write(&state_path, r#"{"healthy":true}"#).unwrap();
        let boots = Arc::new(AtomicUsize::new(0));
        let adapter = FakeAdapter {
            state_path,
            lock_path,
            boots: boots.clone(),
        };

        let result = ensure_harness_daemon(&adapter).unwrap();

        assert!(result.reused);
        assert_eq!(boots.load(Ordering::SeqCst), 0);
        assert_eq!(result.receipt.harness, "fake");
        assert_eq!(result.receipt.durability, "daemon-owned");
        assert_eq!(result.receipt.incarnation, "42:9001");
        assert_eq!(result.receipt.endpoint, "sock://fake");
    }

    #[test]
    fn ac1_err_unreadable_state_refuses_without_boot() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let lock_path = dir.path().join("state.lock");
        std::fs::write(&state_path, b"not-json").unwrap();
        let boots = Arc::new(AtomicUsize::new(0));
        let adapter = FakeAdapter {
            state_path,
            lock_path,
            boots: boots.clone(),
        };

        let error = ensure_harness_daemon(&adapter).unwrap_err().to_string();

        assert!(error.contains("observed=unreadable"), "{error}");
        assert_eq!(boots.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ac2_err_concurrent_dead_replacement_converges() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let lock_path = dir.path().join("state.lock");
        let boots = Arc::new(AtomicUsize::new(0));
        let adapter = Arc::new(FakeAdapter {
            state_path,
            lock_path,
            boots: boots.clone(),
        });
        let mut threads = Vec::new();
        for _ in 0..2 {
            let adapter = adapter.clone();
            threads.push(std::thread::spawn(move || {
                ensure_harness_daemon(adapter.as_ref()).unwrap()
            }));
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();

        assert_eq!(boots.load(Ordering::SeqCst), 1);
        assert_eq!(
            results[0].receipt.incarnation,
            results[1].receipt.incarnation
        );
        assert!(results.iter().any(|result| result.reused));
    }

    struct RefusingAdapter {
        state_path: PathBuf,
        lock_path: PathBuf,
    }

    impl HarnessDaemonAdapter for RefusingAdapter {
        fn harness(&self) -> &str {
            "refusing"
        }
        fn state_path(&self) -> &Path {
            &self.state_path
        }
        fn lock_path(&self) -> &Path {
            &self.lock_path
        }
        fn parse_state(&self, raw: &str) -> Result<DaemonState, String> {
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            DaemonState::new(value, 42, 9001, "sock://refusing", "42:9001")
        }
        fn is_healthy(&self, _state: &DaemonState) -> bool {
            false
        }
        fn may_replace(&self) -> bool {
            false
        }
        fn boot(&self) -> Result<DaemonState, String> {
            panic!("a non-replacing adapter must refuse before boot")
        }
    }

    #[test]
    fn ac1_err_non_replacing_adapter_refuses_dead_state_without_reap_or_boot() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        std::fs::write(&state_path, r#"{"healthy":false}"#).unwrap();
        let adapter = RefusingAdapter {
            state_path,
            lock_path: dir.path().join("state.lock"),
        };

        let error = ensure_harness_daemon(&adapter).unwrap_err().to_string();

        assert!(error.contains("observed=dead"), "{error}");
        assert!(error.contains("restart the provider supervisor"), "{error}");
    }
}
