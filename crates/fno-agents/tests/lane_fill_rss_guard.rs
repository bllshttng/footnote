//! The lane-fill door holds a stated peak RSS.
//!
//! A door whose fill loop never terminated grew past 11GB RSS on a 7GB hosted
//! runner and killed it mid-shard (the runner "shutdown signal" freezes). The
//! sampler here fails the door above the stated peak, so any runaway in the
//! fill path fails this test instead of a CI runner.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The stated peak: a healthy door sits near 2MB, so 256MB is orders of
/// magnitude above any legitimate fill and far below a runner's budget.
const PEAK_RSS_KB: u64 = 256 * 1024;
const WALL: Duration = Duration::from_secs(30);

fn sample_rss_kb(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().parse::<u64>().ok()
}

#[test]
fn lane_fill_door_stays_under_the_stated_peak_rss() {
    let scratch = std::env::temp_dir().join(format!("lane-fill-rss-{}", std::process::id()));
    let home = scratch.join("home");
    let claims = scratch.join("claims");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(home.join(".fno")).unwrap();
    std::fs::create_dir_all(&claims).unwrap();
    std::fs::create_dir_all(cwd.join("plans")).unwrap();
    std::fs::write(
        home.join(".fno").join("graph.json"),
        r#"{"nodes":[{"id":"x-rss1","slug":"rss-one","status":"ready","type":"feature","title":"rss node","plan_path":"plans/rss-plan.md","priority":"p1","created_at":"2026-10-01T00:00:00Z"}]}"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("plans").join("rss-plan.md"),
        "# plan\n\n## Files to modify\n\n| file |\n|---|\n| cli/src/x.py |\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["backlog", "lane-fill", "--json", "--claim", "--max", "2"])
        .envs(fno_agents::test_run::self_owner_env())
        .env("HOME", &home)
        .env("FNO_STATE_DIR", home.join(".fno"))
        .env("FNO_CLAIMS_ROOT", &claims)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the lane-fill door");

    let pid = child.id();
    let started = Instant::now();
    let mut peak_kb: u64 = 0;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > WALL {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "door ran past {:?}; peak RSS {}KB (bound {}KB)",
                        WALL, peak_kb, PEAK_RSS_KB
                    );
                }
                if let Some(rss) = sample_rss_kb(pid) {
                    peak_kb = peak_kb.max(rss);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("door wait failed: {e}"),
        }
    };
    let _ = child.wait();
    assert!(
        status.success(),
        "healthy fixture must exit 0, got {status:?}"
    );
    assert!(
        peak_kb <= PEAK_RSS_KB,
        "door peaked at {peak_kb}KB, above the stated {PEAK_RSS_KB}KB bound"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}
