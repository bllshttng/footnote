//! The zombie read: an exited, unwaited child is a zombie, and both helpers
//! read it as gone. Beside the helpers it pins; proto.rs is near the file
//! budget, so the test body lives here.

use super::*;

#[test]
fn an_unreaped_zombie_reads_dead_and_a_live_child_reads_alive() {
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    // Poll up to 2 s: becoming a zombie takes a scheduler tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline && !pid_is_zombie(pid) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(pid_is_zombie(pid), "fixture: the child must be a zombie");
    assert!(pid_confirmed_dead(pid), "a zombie reads as dead");
    child.wait().unwrap();
    // Positive control: a live child reads alive on both helpers.
    let mut live = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let live_pid = live.id() as i32;
    assert!(!pid_is_zombie(live_pid), "a live child is not a zombie");
    assert!(!pid_confirmed_dead(live_pid), "a live child reads alive");
    live.kill().unwrap();
    live.wait().unwrap();
}
