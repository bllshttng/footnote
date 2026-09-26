//! Transition shim: the store implementation is the `event_store` module.
//! `live_journal` still serves king_history; the module disappears
//! once that caller ports onto the library.

pub(crate) use crate::event_store::{journal_text, live_journal, review_text};
