//! Which opener does a clicked pane link take?
//!
//! The mux client intercepts every `ServerMsg::OpenLink` URI and routes it:
//! fno's own pseudo schemes stay inside the mux (open the message, filter
//! Messages to a clicked `@handle`, open the sender's session, or open the
//! file), and only plain web URLs reach the platform opener. Classification
//! is pure ([`route`]); [`start`] only dispatches, so a new pseudo scheme is
//! one arm here and never a fourth inline branch in the select loop.

use std::path::PathBuf;

use tokio::sync::mpsc::UnboundedSender;

/// Where a clicked URI goes. `Message` and `Handle` finish on the UI loop;
/// the rest are dispatched to blocking threads and report back through their
/// channels.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Message(String),
    Sender(String),
    Handle(String),
    File(PathBuf, Option<u32>),
    Web,
}

/// Classify an OpenLink URI. Order matters: fno's pseudo schemes are tested
/// before the web fallback, and `is_file_uri` is strict enough that no web
/// URL can read as a file.
pub fn route(url: &str) -> Route {
    if let Some(id) = crate::link::message_id_from_uri(url) {
        return Route::Message(id.to_string());
    }
    if crate::link::is_sender_uri(url) {
        return Route::Sender(
            url.trim_start_matches(crate::link::SENDER_SCHEME)
                .to_string(),
        );
    }
    if let Some(name) = crate::link::handle_from_uri(url) {
        return Route::Handle(name.to_string());
    }
    if let Some((path, line)) = crate::link::file_uri_parts(url) {
        return Route::File(path, line);
    }
    Route::Web
}

type LinkTx = UnboundedSender<(String, Result<(), String>)>;
type SenderTx = UnboundedSender<(String, Option<String>)>;

/// The on-loop message route, carried out of [`start`] so the caller's match
/// stays one-armed.
#[derive(Debug, PartialEq, Eq)]
pub enum RoutedKind {
    /// The mail a `fno-message:` URI named.
    Message(String),
    /// The handle a `fno-handle:` URI named; the Messages tab filters to it.
    Handle(String),
}

pub struct Routed {
    pub kind: RoutedKind,
}

/// Start the off-loop leg for `url`'s route. Returns `Some` only for
/// [`Route::Message`] and [`Route::Handle`], whose legs [`finish`] lands on
/// the UI loop.
pub fn start(url: &str, link_tx: LinkTx, sender_tx: SenderTx) -> Option<Routed> {
    let kind = match route(url) {
        Route::Message(id) => RoutedKind::Message(id),
        Route::Handle(name) => RoutedKind::Handle(name),
        Route::Sender(id) => {
            tokio::task::spawn_blocking(move || {
                let resolved = super::open_chooser::resolve_sender(&id);
                let _ = sender_tx.send((id, resolved));
            });
            return None;
        }
        r => {
            let url = url.to_string();
            tokio::task::spawn_blocking(move || {
                let outcome = match r {
                    Route::File(path, _line) => crate::link::open_fno_path(&path),
                    Route::Web => crate::link::open_url(&url),
                    _ => unreachable!("message, handle and sender routes return early"),
                };
                let _ = link_tx.send((url, outcome));
            });
            return None;
        }
    };
    Some(Routed { kind })
}

/// Land a routed UI-loop leg: open the message, or filter Messages to the
/// clicked handle. The caller repaints.
pub fn finish(routed: Routed, view: &mut super::View) {
    match routed.kind {
        RoutedKind::Message(id) => super::messages_view::open_message(view, id),
        RoutedKind::Handle(name) => super::messages_view::open_handle(view, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One table over the classifier: fno's pseudo schemes route before the
    /// web fallback, a malformed pseudo URI never reads as a file (it dies in
    /// `is_openable` as a web URL), and plain URLs route web.
    #[test]
    fn route_classification_table() {
        for (url, want) in [
            (
                "fno-message:fmail-0123456789ab",
                Route::Message("fmail-0123456789ab".into()),
            ),
            (
                "fno-sender:fmail-0123456789ab",
                Route::Sender("fmail-0123456789ab".into()),
            ),
            ("fno-handle:nemo", Route::Handle("nemo".into())),
            (
                "fno-file:/tmp/notes.md:42",
                Route::File(PathBuf::from("/tmp/notes.md"), Some(42)),
            ),
            ("fno-file:relative/path.md", Route::Web),
            ("fno-file:/has space.md", Route::Web),
            ("fno-file:", Route::Web),
            ("https://example.com/a", Route::Web),
        ] {
            assert_eq!(route(url), want, "{url}");
        }
    }
}
