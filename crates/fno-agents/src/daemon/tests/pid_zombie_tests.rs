//! daemon::pid_confirmed_dead reads a zombie (dead, unreaped) as gone, via
//! census::pid_is_zombie. Beside the callers this pins.

use super::*;

#[test]
fn pid_confirmed_dead_reads_an_unreaped_zombie_as_dead() {
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .unwrap();
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline && !crate::census::pid_is_zombie(pid) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        crate::census::pid_is_zombie(pid),
        "fixture: the child must be a zombie"
    );
    assert!(
        pid_confirmed_dead(pid),
        "a zombie is dead, not merely unreachable"
    );
    child.wait().unwrap();
    assert!(
        !pid_confirmed_dead(std::process::id() as u32),
        "a live pid is not dead"
    );
}
