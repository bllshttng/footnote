//! What is a pane's child running right now? The live argv of a pty's child
//! pid, read from the same sources `ps -o command=` reads. The send gate keys
//! on it: a claude portal seat is only typed into while its child still runs
//! `attach <id>`, so a viewer a person detached to agent view can no longer
//! receive programmatic keystrokes.
//!
//! ponytail: one syscall/one file per read, no caching, no watching - a send
//! is rare and the argv must be current, not remembered.

/// The live argv of `pid`, or `None` when it cannot be read (process gone,
/// permission, platform without a reader). The first element is the exec path
/// the same way `std::env::args().next()` reports it.
pub fn process_argv(pid: u32) -> Option<Vec<String>> {
    process_argv_os(pid)
}

#[cfg(target_os = "macos")]
fn process_argv_os(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // A NULL oldp is the size probe; 0 means there is nothing to read.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return None;
    }
    let mut buf = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    parse_procargs2(&buf[..size])
}

#[cfg(target_os = "linux")]
fn process_argv_os(pid: u32) -> Option<Vec<String>> {
    let buf = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if buf.is_empty() {
        return None;
    }
    Some(
        buf.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_argv_os(_pid: u32) -> Option<Vec<String>> {
    None
}

/// The macOS `KERN_PROCARGS2` buffer layout: an `i32` argc, the truncated
/// exec path (NUL-terminated), NUL padding to alignment, then argc
/// NUL-terminated strings (argv[0] is the path again), then the environment.
/// Kept OS-independent so the parse tests run on Linux CI.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    if argc == 0 {
        return None;
    }
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    if i >= buf.len() {
        return None;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut args = Vec::with_capacity(argc);
    while args.len() < argc && i < buf.len() {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        args.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        i += 1;
    }
    (args.len() == argc).then_some(args)
}

/// True when two adjacent tokens read `attach` and `id`. `claude
/// --settings /s.json attach <id>` and the isolated-account `env
/// CLAUDE_CONFIG_DIR=<dir> claude attach <id>` both pass, before and after
/// `env` execs.
pub fn attaches(argv: &[String], id: &str) -> bool {
    argv.windows(2).any(|w| w[0] == "attach" && w[1] == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_procargs2_reads_the_macos_layout() {
        // argc=3, path "claude", padding, then the three argv strings.
        let mut buf = Vec::new();
        buf.extend_from_slice(&3i32.to_ne_bytes());
        buf.extend_from_slice(b"claude\0");
        buf.extend_from_slice(&[0, 0, 0]); // alignment padding
        for s in ["claude", "attach", "deadbee1"] {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        assert_eq!(
            parse_procargs2(&buf),
            Some(argv(&["claude", "attach", "deadbee1"]))
        );
        // Too short / zero argc / truncated string list read as unreadable.
        assert_eq!(parse_procargs2(&buf[..3]), None);
        assert_eq!(parse_procargs2(&[0, 0, 0, 0]), None);
    }

    #[test]
    fn attaches_keys_on_adjacent_tokens() {
        assert!(attaches(
            &argv(&["claude", "attach", "deadbee1", "--settings", "/s.json"]),
            "deadbee1"
        ));
        assert!(attaches(
            &argv(&["env", "claude", "attach", "deadbee1"]),
            "deadbee1"
        ));
        assert!(!attaches(&argv(&["claude", "agents"]), "deadbee1"));
        assert!(!attaches(&argv(&["claude", "attach", "other"]), "deadbee1"));
        assert!(!attaches(&argv(&[]), "deadbee1"));
    }

    #[test]
    fn process_argv_reads_the_calling_process() {
        // Positive control: the reader reads a real process, not only test
        // buffers - our own argv's first element is this binary's path.
        let own = process_argv(std::process::id()).expect("own argv is readable");
        assert_eq!(
            own.first().map(String::as_str),
            std::env::args().next().as_deref(),
            "argv[0] is the exec path"
        );
    }

    #[test]
    fn process_argv_reads_a_real_child() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 5")
            .spawn()
            .expect("spawn sh");
        let read = process_argv(child.id());
        child.kill().ok();
        child.wait().ok();
        let read = read.expect("a live child's argv is readable");
        assert!(
            read.windows(2).any(|w| w[0] == "sleep" && w[1] == "5")
                || read.iter().any(|t| t.ends_with("/sh") || t == "sh"),
            "the reader sees the child, not only itself: {read:?}"
        );
    }
}
