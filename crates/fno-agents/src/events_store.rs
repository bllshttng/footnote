//! Transition shim: the store implementation moved to the `fno-event-store`
//! crate. `live_journal` still serves king_history; the module disappears
//! once that caller ports (the x-0915 cutover).

pub(crate) use fno_event_store::live_journal;
