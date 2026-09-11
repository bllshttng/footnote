//! Sidecar readers for mux session rows (x-f188 change 4): the `.ver` wire
//! stamp and the `.pid` stamp. Split out of mux_cli so the census-facing
//! fields live beside the sidecars they read (proto.rs) without growing an
//! over-budget file.

use crate::proto;
use std::path::Path;

/// Read a session socket's `.ver` sidecar (x-1a85) and parse the stamped wire
/// version. `None` on any read/parse failure (absent sidecar = older server).
pub(crate) fn read_wire_version(sock: &Path) -> Option<u32> {
    std::fs::read_to_string(proto::version_sidecar_path(sock))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The serving pid from the session's pid sidecar (`<pid>:<start>` or
/// `<pid>`), for the `ls --json` Live row. `None` when the sidecar is
/// absent or unparseable: a pre-sidecar server is not a fault.
pub(crate) fn pid_from_sidecar(name: &str) -> Option<u32> {
    let sock = proto::socket_path(name).ok()?;
    let raw = std::fs::read_to_string(proto::pid_sidecar_path(&sock)).ok()?;
    raw.trim().split(':').next()?.parse().ok()
}

#[cfg(test)]
mod pid_sidecar_tests {
    // x-f188 change 4: the `ls --json` Live row's pid comes from the pid
    // sidecar; absent or unparseable reads null, never a fault.
    use super::*;

    #[test]
    fn pid_from_sidecar_parses_pid_and_pid_start_shapes() {
        let sock = proto::socket_path("pid-sidecar-ok").unwrap();
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let sidecar = proto::pid_sidecar_path(&sock);
        std::fs::write(&sidecar, "4242:999111\n").unwrap();
        assert_eq!(pid_from_sidecar("pid-sidecar-ok"), Some(4242));
        std::fs::write(&sidecar, "777").unwrap();
        assert_eq!(pid_from_sidecar("pid-sidecar-ok"), Some(777));
        std::fs::remove_file(&sidecar).ok();
    }

    #[test]
    fn pid_from_sidecar_reads_null_when_absent_or_unparseable() {
        // AC4-ERR: no sidecar at all -> None (the row serializes null) and
        // nothing panics.
        assert_eq!(pid_from_sidecar("pid-sidecar-absent"), None);
        let sock = proto::socket_path("pid-sidecar-junk").unwrap();
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        std::fs::write(proto::pid_sidecar_path(&sock), "not-a-pid").unwrap();
        assert_eq!(pid_from_sidecar("pid-sidecar-junk"), None);
        std::fs::remove_file(proto::pid_sidecar_path(&sock)).ok();
    }
}
