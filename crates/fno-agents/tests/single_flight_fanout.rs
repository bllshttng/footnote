//! Prove the fan-out is gone, not that the reader still works.
//!
//! The node that asked for this named the trap, and it is the whole test
//! design: asserting that `agents truth` still returns correct output proves the
//! READER and says nothing about how many children answered it. So this harness
//! counts CHILD PROCESSES. A fake `fno` on PATH appends a line every time it
//! runs and sleeps first, standing in for the roster read measured between
//! 940 ms and 23.2 s on a loaded box.
//!
//! The control matters more than the assertion. A run that passes with nothing
//! deduped is measuring a quiet machine, so the second test drives the SAME
//! harness with handle sets the latch cannot merge and asserts the child count
//! DOES climb. Without it, a broken probe that spawned nothing at all would read
//! as a perfect latch.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use fno_agents::truth_probe::family1_truth_probe_many;

/// How long the fake roster read takes. Long enough that every thread is inside
/// the same window, short enough to keep the suite quick.
const SLOW_READ: Duration = Duration::from_secs(2);

const CALLERS: usize = 5;

/// Install the fake `fno` and pin the claims root, once for this binary.
///
/// Both are process-global, and these tests run threaded, so a per-test write
/// would race. One setup plus a distinct handle name per test isolates them.
fn harness() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("fno-fanout-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_fake_fno(&bin.join("fno"));
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", bin.display(), path));
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        root
    })
}

/// A stand-in for `fno agents truth --handles <list> --json`: it records that it
/// ran, waits, then answers every handle it was asked about.
fn write_fake_fno(path: &Path) {
    let script = format!(
        r#"#!/bin/sh
echo run >> "$FNO_FANOUT_LOG"
sleep {secs}
handles=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--handles" ]; then handles="$2"; fi
  shift
done
body=""
IFS=','
for h in $handles; do
  body="$body\"$h\":{{\"state\":\"working\"}},"
done
printf '{{%s}}' "${{body%,}}"
"#,
        secs = SLOW_READ.as_secs()
    );
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Run `CALLERS` concurrent probes and return how many children actually ran.
///
/// Serialized across tests: the fake reads its counter path from the
/// environment, which is process-global, so two overlapping runs would write
/// each other's log and both counts would be fiction.
fn concurrent_children(log_name: &str, handles_for: impl Fn(usize) -> Vec<String>) -> usize {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let log = harness().join(log_name);
    let _ = std::fs::remove_file(&log);
    std::env::set_var("FNO_FANOUT_LOG", &log);

    let threads: Vec<_> = (0..CALLERS)
        .map(|i| {
            let handles = handles_for(i);
            std::thread::spawn(move || family1_truth_probe_many(&handles))
        })
        .collect();
    for (i, t) in threads.into_iter().enumerate() {
        let probes = t.join().unwrap();
        assert!(
            !probes.is_empty(),
            "caller {i} got no answer; the harness measured nothing"
        );
    }
    std::fs::read_to_string(&log).map_or(0, |s| s.lines().count())
}

/// AC15: concurrent callers over the SAME handle set cost exactly one child.
#[test]
fn one_handle_set_costs_one_child_however_many_callers_arrive() {
    let children = concurrent_children("same.log", |_| vec!["ses-shared".to_string()]);
    assert_eq!(
        children, 1,
        "{CALLERS} callers over one handle set spawned {children} children"
    );
}

/// AC16, the positive control: the same harness, with handle sets the latch
/// cannot merge, MUST spawn one child per set.
///
/// This is what makes the assertion above mean something. A probe that spawned
/// nothing at all would satisfy "never more than one" perfectly.
#[test]
fn distinct_handle_sets_still_spawn_one_child_each() {
    let children = concurrent_children("distinct.log", |i| vec![format!("ses-{i}")]);
    assert_eq!(
        children, CALLERS,
        "{CALLERS} distinct handle sets spawned {children} children; \
         the harness cannot observe a fan-out, so the shared-set result proves nothing"
    );
}

/// AC17: the ceiling is not the defect. 319 against 96 was the machine telling
/// the truth, and raising the cap would have deleted the signal instead of the
/// load.
#[test]
fn the_spawn_gate_load_ceiling_is_unchanged() {
    assert_eq!(fno_agents::agents_config::DEFAULT_MAX_LOAD_PER_CPU, 8.0);
}
