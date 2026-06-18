//! Bounded-backpressure stdin queue (design module `write_queue.rs`).
//!
//! Inputs destined for an agent's PTY stdin are enqueued here rather than
//! written directly, so a slow or wedged child cannot block the caller and a
//! burst of `ask`/drive keystrokes cannot grow memory without bound. The queue
//! is capacity-bounded; `enqueue` returns [`WriteQueueError::Full`] when at
//! capacity so the caller surfaces backpressure rather than buffering forever.
//!
//! Runtime-agnostic by design (Wave 1): the worker's writer loop pops messages
//! and writes them to the PTY master. Wave 3's daemon drives this from a tokio
//! task; nothing here assumes an async runtime.

use std::collections::VecDeque;

/// A single unit of work for the PTY writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteMsg {
    /// Raw bytes to write to the child's stdin (already envelope-wrapped by the
    /// caller for non-Claude providers; the queue is content-agnostic).
    Bytes(Vec<u8>),
    /// A terminal resize to apply before subsequent writes (drive resize, etc.).
    Resize { rows: u16, cols: u16 },
}

impl WriteMsg {
    /// Approximate in-memory weight, used only for diagnostics/observability.
    pub fn byte_len(&self) -> usize {
        match self {
            WriteMsg::Bytes(b) => b.len(),
            WriteMsg::Resize { .. } => 0,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WriteQueueError {
    #[error("write queue at capacity ({capacity} messages); apply backpressure")]
    Full { capacity: usize },
}

/// FIFO queue with a hard message-count capacity.
#[derive(Debug)]
pub struct WriteQueue {
    inner: VecDeque<WriteMsg>,
    capacity: usize,
}

impl WriteQueue {
    /// Create a queue holding at most `capacity` messages. A capacity of 0 is
    /// clamped to 1 so the queue is always usable.
    pub fn new(capacity: usize) -> Self {
        WriteQueue {
            inner: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.inner.len() >= self.capacity
    }

    /// Enqueue a message. Returns [`WriteQueueError::Full`] without mutating the
    /// queue when at capacity (the message is returned to the caller untouched
    /// so it can be retried after backpressure clears).
    pub fn enqueue(&mut self, msg: WriteMsg) -> Result<(), (WriteMsg, WriteQueueError)> {
        if self.is_full() {
            return Err((
                msg,
                WriteQueueError::Full {
                    capacity: self.capacity,
                },
            ));
        }
        self.inner.push_back(msg);
        Ok(())
    }

    /// Pop the next message in FIFO order.
    pub fn dequeue(&mut self) -> Option<WriteMsg> {
        self.inner.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order_preserved() {
        let mut q = WriteQueue::new(4);
        q.enqueue(WriteMsg::Bytes(b"a".to_vec())).unwrap();
        q.enqueue(WriteMsg::Resize { rows: 24, cols: 80 }).unwrap();
        q.enqueue(WriteMsg::Bytes(b"b".to_vec())).unwrap();
        assert_eq!(q.dequeue(), Some(WriteMsg::Bytes(b"a".to_vec())));
        assert_eq!(q.dequeue(), Some(WriteMsg::Resize { rows: 24, cols: 80 }));
        assert_eq!(q.dequeue(), Some(WriteMsg::Bytes(b"b".to_vec())));
        assert_eq!(q.dequeue(), None);
    }

    #[test]
    fn full_returns_backpressure_and_does_not_drop_message() {
        let mut q = WriteQueue::new(2);
        q.enqueue(WriteMsg::Bytes(b"1".to_vec())).unwrap();
        q.enqueue(WriteMsg::Bytes(b"2".to_vec())).unwrap();
        let rejected = WriteMsg::Bytes(b"3".to_vec());
        let err = q.enqueue(rejected.clone()).unwrap_err();
        assert_eq!(
            err.0, rejected,
            "rejected message must be returned for retry"
        );
        assert_eq!(err.1, WriteQueueError::Full { capacity: 2 });
        assert_eq!(q.len(), 2, "full queue must not have grown past capacity");

        // Draining one frees a slot so the retry succeeds.
        assert!(q.dequeue().is_some());
        assert!(q.enqueue(rejected).is_ok());
    }

    #[test]
    fn zero_capacity_clamps_to_one() {
        let mut q = WriteQueue::new(0);
        assert_eq!(q.capacity(), 1);
        assert!(q.enqueue(WriteMsg::Bytes(b"x".to_vec())).is_ok());
        assert!(q.is_full());
    }
}
