//! `fno-agents hook stop` - the Stop hook's native handler (x-09d2, Task 3.1).
//!
//! Owns the translation the shell shim carried: payload read, ownership via
//! the salvaged stop-gate code, the bounded-block counters, foreign-session
//! guard, cargo build-dir export, the in-process decide call, harness-shaped
//! block output, and terminal cleanup. The stop/allow decision itself stays
//! in loopcheck.rs; this module is transport + the shell's translation.

/// Entry point the `hook stop` dispatch arm calls. Wire-up lands with the
/// exec wrapper in Task 3.1; until then the wrapper does not exist, so this
/// arm is unreachable in production.
pub fn run(args: &[String]) -> i32 {
    let _ = args;
    eprintln!("fno-agents hook stop: not wired yet; the shell shim owns the Stop path");
    2
}
