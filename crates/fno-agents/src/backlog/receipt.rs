//! Receipt emission for write verbs: print after the store commit, and
//! never let a closed stdout turn a landed write into a reported failure.
//! A reader that closed the pipe (`fno backlog note | head`) cut the
//! display on purpose; the receipt is the display, the store write is the
//! product. Rust std ignores SIGPIPE, so a closed reader surfaces to
//! `println!` as EPIPE and panics: exit 101 with the write landed, which
//! every caller reads as failure. Swallowing the error is the contract.
use std::io::Write;

/// One receipt line to stdout, errors ignored on purpose (see module doc).
pub fn emit_line(line: &str) {
    emit_line_to(std::io::stdout(), line);
}

/// The same emission against any writer; the test seam for the contract.
pub fn emit_line_to<W: Write>(mut out: W, line: &str) {
    let _ = out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};

    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_broken_pipe_on_the_receipt_neither_panics_nor_propagates() {
        emit_line_to(BrokenPipe, "noted t-1: the ruling survives a closed pipe");
    }
}
