// Wave 0 throwaway smoke prototype (Phase 6, ab-a09e1eaf).
//
// Question: when a supervisor process spawns a child through a PTY via
// portable-pty, then the supervisor is SIGKILLed (worst case, no cleanup),
// does the child survive? The design asserts "supervisor not controller"
// (children outlive supervisor restarts). POSIX suggests the opposite:
// closing the last master fd hangs up the slave -> SIGHUP -> child dies.
//
// This binary is the supervisor half. It spawns `bash <child_script>` on a
// fresh PTY, prints SUPERVISOR_PID and CHILD_PID on stdout (so the harness
// can SIGKILL the supervisor and poll the child), then blocks forever
// draining the master so the child never wedges on a full output pipe. On
// SIGKILL all fds close, including the master -- which is the event under
// test.
//
// Not production code. Lives under cli/scripts/smoke/ as reproducible
// evidence for the architecture decision memo, mirroring the existing
// capture-*.sh smoke scripts.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let child_script = args
        .next()
        .ok_or("usage: pty-survival-probe <child_script> [child_args...]")?;
    let child_args: Vec<String> = args.collect();

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("bash");
    cmd.arg(&child_script);
    for a in &child_args {
        cmd.arg(a);
    }
    let child = pair.slave.spawn_command(cmd)?;
    let child_pid = child
        .process_id()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Standard supervisor pattern: drop the slave handle so only the spawned
    // child holds the slave side. The supervisor keeps only the master.
    drop(pair.slave);

    println!("SUPERVISOR_PID={}", std::process::id());
    println!("CHILD_PID={}", child_pid);
    // Flush before the harness reads us.
    use std::io::Write;
    std::io::stdout().flush()?;

    // Block forever draining the master. This keeps the master fd open (the
    // thing whose close-on-death we are testing) and prevents the child from
    // blocking on a full pipe. The harness SIGKILLs us out of this loop.
    let mut reader = pair.master.try_clone_reader()?;
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,    // EOF: child closed slave (it exited)
            Ok(_) => continue, // discard heartbeat bytes
            Err(_) => break,
        }
    }

    // Hold master to end of scope so it is not dropped before the read loop.
    let _keep_master_alive = pair.master;
    Ok(())
}
