//! Transition shim: the store implementation moved to the `fno-event-store`
//! crate. Every caller keeps its `crate::events_store::*` path until its wave
//! ports it to direct library calls; the module disappears once the last
//! caller ports (the `x-0915` cutover).

pub(crate) use fno_event_store::{live_journal, open_read, store_path, sync};
