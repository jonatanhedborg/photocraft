//! Reading a drop's payload from the pipe the drag source writes into. The source is another
//! program, so the read is bounded in size and time and never blocks: the listener calls
//! [`Collect::read_from`] once each time `poll` says the pipe is readable.

use std::io::{ErrorKind, Read};
use std::time::{Duration, Instant};

/// The most a URI list may hold (thousands of paths).
pub const MAX_BYTES: usize = 1 << 20;
/// How long the source gets to write the list.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Where a read left the payload.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// More to come.
    Pending,
    /// The source closed the pipe: this is everything.
    Done(Vec<u8>),
    /// Too big, too slow or unreadable.
    Failed(String),
}

/// A payload being read.
#[derive(Debug)]
pub struct Collect {
    data: Vec<u8>,
    deadline: Instant,
}

impl Collect {
    /// Start a read that has [`TIMEOUT`] from `now`.
    pub fn new(now: Instant) -> Self {
        Self { data: Vec::new(), deadline: now + TIMEOUT }
    }

    /// One `read` call (the caller knows it won't block).
    pub fn read_from(&mut self, r: &mut impl Read) -> Step {
        let mut chunk = [0u8; 64 * 1024];
        match r.read(&mut chunk) {
            Ok(0) => Step::Done(std::mem::take(&mut self.data)),
            Ok(n) => {
                let Some(read) = chunk.get(..n) else { return Step::Failed("read past the buffer".into()) };
                if self.data.len().saturating_add(n) > MAX_BYTES {
                    return Step::Failed(format!("the dropped file list is over {MAX_BYTES} bytes"));
                }
                self.data.extend_from_slice(read);
                Step::Pending
            }
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => Step::Pending,
            Err(e) => Step::Failed(format!("couldn't read the dropped file list: {e}")),
        }
    }

    /// `Failed` once the source took too long (nothing to read by the deadline).
    pub fn check_deadline(&self, now: Instant) -> Step {
        if now >= self.deadline { Step::Failed("the drag source didn't send the dropped file list in time".into()) } else { Step::Pending }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_until_the_writer_closes() {
        let (mut r, mut w) = std::io::pipe().unwrap();
        let mut c = Collect::new(Instant::now());
        w.write_all(b"file:///a.png\r\n").unwrap();
        assert_eq!(c.read_from(&mut r), Step::Pending);
        w.write_all(b"file:///b.png\r\n").unwrap();
        drop(w);
        assert_eq!(c.read_from(&mut r), Step::Pending);
        assert_eq!(c.read_from(&mut r), Step::Done(b"file:///a.png\r\nfile:///b.png\r\n".to_vec()));
    }

    #[test]
    fn an_empty_payload_is_done_and_empty() {
        let mut c = Collect::new(Instant::now());
        assert_eq!(c.read_from(&mut std::io::empty()), Step::Done(Vec::new()));
    }

    #[test]
    fn an_oversized_payload_fails() {
        let mut c = Collect::new(Instant::now());
        let mut big = std::io::repeat(b'x');
        let mut steps = 0;
        let step = loop {
            match c.read_from(&mut big) {
                Step::Pending => steps += 1,
                other => break other,
            }
        };
        assert!(matches!(step, Step::Failed(ref e) if e.contains("over")), "{step:?}");
        assert_eq!(steps, MAX_BYTES / (64 * 1024));
    }

    #[test]
    fn a_read_error_fails_and_interruptions_wait() {
        struct Erring(ErrorKind);
        impl Read for Erring {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(self.0.into())
            }
        }
        let mut c = Collect::new(Instant::now());
        assert_eq!(c.read_from(&mut Erring(ErrorKind::Interrupted)), Step::Pending);
        assert_eq!(c.read_from(&mut Erring(ErrorKind::WouldBlock)), Step::Pending);
        assert!(matches!(c.read_from(&mut Erring(ErrorKind::BrokenPipe)), Step::Failed(_)));
    }

    #[test]
    fn a_slow_source_times_out() {
        let start = Instant::now();
        let c = Collect::new(start);
        assert_eq!(c.check_deadline(start), Step::Pending);
        assert_eq!(c.check_deadline(start + TIMEOUT - Duration::from_millis(1)), Step::Pending);
        assert!(matches!(c.check_deadline(start + TIMEOUT), Step::Failed(_)));
    }
}
