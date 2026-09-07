//! The board's ONE whole-board budget and the bounded subprocess runner.
//! Every per-source slice derives from it; there is no second timeout.
use super::SourceRead;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

/// The whole-board budget: every per-source slice derives from what remains
/// minus the serialization reserve; there is no second, independent per-source
/// timeout to invert.
pub(crate) struct Budget {
    deadline: Instant,
    last: Option<&'static str>,
}

impl Budget {
    pub(crate) fn new(budget_ms: u64) -> Self {
        Budget {
            deadline: Instant::now() + Duration::from_millis(budget_ms),
            last: None,
        }
    }
    /// The slice this next source may spend, or None once spent.
    pub(crate) fn slice(&self) -> Option<Duration> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .and_then(|d| d.checked_sub(Duration::from_millis(SERIALIZE_RESERVE_MS)));
        left.filter(|d| !d.is_zero())
    }
    /// Claim the budget for `name`; returns its slice or None once spent.
    pub(crate) fn start(&mut self, name: &'static str) -> Option<Duration> {
        let s = self.slice();
        if s.is_some() {
            self.last = Some(name);
        }
        s
    }
    pub(crate) fn spent_error(&self) -> String {
        match self.last {
            None => "not-read: board budget exhausted before any source".to_string(),
            Some(last) => format!("not-read: board budget exhausted after {last}"),
        }
    }
}

/// Run a subprocess with a hard wall-clock bound. A dedicated reader per pipe
/// drains stdout/stderr WHILE the child runs: a 210KB payload over a pipe the
/// parent never reads while waiting blocks the child on write until the kill,
/// which read every timeout as a dead child (measured: `fno inbox outstanding`
/// alone answers in 4s; the same command piped-only-at-exit died at the 26s
/// slice). The poll granularity (25ms) is far below any slice this board
/// hands out, and the kill is the degrade-not-crash contract's enforcement half.
pub(crate) fn run_with_timeout(
    cmd: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {}", cmd[0], e))?;
    let deadline = Instant::now() + timeout;
    // Drain the pipes concurrently; a killed child's pipe stays readable to
    // EOF, so the joins return promptly even on the kill path.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_reader.join().unwrap_or_default();
                let stderr = err_reader.join().unwrap_or_default();
                if !status.success() {
                    let detail = String::from_utf8_lossy(&stderr);
                    let detail = detail.trim();
                    let detail = if detail.is_empty() {
                        String::from_utf8_lossy(&stdout).trim().to_string()
                    } else {
                        detail.to_string()
                    };
                    return Err(format!(
                        "exit {}: {}",
                        status.code().unwrap_or(-1),
                        detail.chars().take(500).collect::<String>()
                    ));
                }
                return Ok(stdout);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let shown: Vec<String> = cmd.iter().take(6).cloned().collect::<Vec<_>>();
                    let mut shown = shown.join(" ");
                    if cmd.len() > 6 {
                        shown.push_str(" ...");
                    }
                    return Err(format!(
                        "{shown}: timed out after {:.1}s",
                        timeout.as_secs_f64()
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("{}: {}", cmd[0], e)),
        }
    }
}

pub(crate) fn run_json(cmd: Vec<String>, cwd: &Path, timeout: Duration) -> SourceRead {
    match run_with_timeout(&cmd, cwd, timeout) {
        Err(e) => SourceRead::err(e),
        Ok(stdout) => {
            if stdout.is_empty() {
                return SourceRead::ok(Value::Null);
            }
            match serde_json::from_slice::<Value>(&stdout) {
                Ok(v) => SourceRead::ok(v),
                Err(e) => SourceRead::err(format!("unparseable output: {e}")),
            }
        }
    }
}

/// The argv prefix for a Python `fno` self-shellout, resolved without a PATH
/// dependency (board.py `_fno` -> `_subprocess_util.fno_py_cmd`): the
/// `fno-py` console script, found on PATH first, then the bare name so a
/// genuinely-missing CLI surfaces a real subprocess error rather than a
/// silent no-op. A cargo-only install has no `fno` on PATH; `fno-py` (in
/// `~/.local/bin`) is what the mux forwards to, and PATH usually carries it.
pub(crate) fn fno_py_cmd() -> Vec<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("fno-py");
            if candidate.is_file() {
                return vec![candidate.display().to_string()];
            }
        }
    }
    vec!["fno-py".to_string()]
}

/// Held back from the sources so the board can still serialize and print its
/// payload before the caller's outer timer fires.
pub const SERIALIZE_RESERVE_MS: u64 = 1_000;

/// Whole-board budget when a human runs the board by hand and passes no
/// `--budget-ms`. This budgets the ENTIRE board, never a per-source read: an
/// independent per-source default is exactly what must not exist here (the old
/// 60s-per-read Python default was twice the 30s whole-board kill, so no inner
/// timeout could ever fire).
pub const HAND_RUN_BUDGET_MS: u64 = 30_000;

pub(crate) fn now_secs_board() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::king_board::{SRC_PRS, SRC_READY};

    #[test]
    fn budget_slices_shrink_and_exhaustion_names_the_last_source() {
        let mut b = Budget::new(5_000);
        assert!(b.start("claims").is_some());
        std::thread::sleep(Duration::from_millis(10));
        assert!(b.start(SRC_READY).is_some());
        // A zero budget is spent before any source.
        let mut b = Budget::new(0);
        assert!(b.start("claims").is_none());
        assert_eq!(
            b.spent_error(),
            "not-read: board budget exhausted before any source"
        );
    }

    #[test]
    fn the_spent_error_names_the_source_that_ran_last() {
        // The 1s serialization reserve means a usable budget starts above
        // 1000ms; 2.6s leaves room to spend one slice, then exhaust.
        let mut b = Budget::new(2_600);
        b.start("backlog undispatched");
        std::thread::sleep(Duration::from_millis(1_700));
        assert!(b.start(SRC_PRS).is_none());
        assert_eq!(
            b.spent_error(),
            "not-read: board budget exhausted after backlog undispatched"
        );
    }
}
