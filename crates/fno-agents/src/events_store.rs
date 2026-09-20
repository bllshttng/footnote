//! Transition shim: the store implementation moved to the `fno-event-store`
//! crate. `live_journal` still serves king_history; the module disappears
//! once that caller ports onto the library.

pub(crate) use fno_event_store::{journal_text, live_journal};
