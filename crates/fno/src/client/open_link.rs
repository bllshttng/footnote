//! Which opener does a clicked pane link take?
//!
//! The mux client intercepts every `ServerMsg::OpenLink` URI and routes it:
//! fno's own pseudo schemes stay inside the mux (open the message, the
//! sender's session, or the file), and only plain web URLs reach the platform
//! opener. Classification is pure ([`route`]); [`start`] only dispatches, so
//! a new pseudo scheme is one arm here and never a fourth inline branch in
//! the select loop.

use std::path::PathBuf;

use tokio::sync::mpsc::UnboundedSender;

/// Where a clicked URI goes. `Message` finishes on the UI loop; the rest are
/// dispatched to blocking threads and report back through their channels.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Message(String),
    Sender(String),
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
    if let Some((path, line)) = crate::link::file_uri_parts(url) {
        return Route::File(path, line);
    }
    Route::Web
}

type LinkTx = UnboundedSender<(String, Result<(), String>)>;
type SenderTx = UnboundedSender<(String, Option<String>)>;

/// Start the off-loop leg for `url`'s route. Returns `Some` only for
/// [`Route::Message`], whose leg the caller finishes on the UI loop.
pub fn start(url: &str, link_tx: LinkTx, sender_tx: SenderTx) -> Option<Routed> {
    match route(url) {
        Route::Message(id) => Some(Routed { id }),
        Route::Sender(id) => {
            tokio::task::spawn_blocking(move || {
                let resolved = super::open_chooser::resolve_sender(&id);
                let _ = sender_tx.send((id, resolved));
            });
            None
        }
        r => {
            let url = url.to_string();
            tokio::task::spawn_blocking(move || {
                let outcome = match r {
                    Route::File(path, _line) => crate::link::open_fno_path(&path),
                    Route::Web => crate::link::open_url(&url),
                    _ => unreachable!("message and sender routes return early"),
                };
                let _ = link_tx.send((url, outcome));
            });
            None
        }
    }
}

/// The on-loop message route, carried out of [`start`] so the caller's match
/// stays one-armed.
pub struct Routed {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_schemes_route_before_the_web_fallback() {
        assert_eq!(
            route("fno-message:fmail-0123456789ab"),
            Route::Message("fmail-0123456789ab".into())
        );
        assert_eq!(
            route("fno-sender:fmail-0123456789ab"),
            Route::Sender("fmail-0123456789ab".into())
        );
        assert_eq!(
            route("fno-file:/tmp/notes.md:42"),
            Route::File(PathBuf::from("/tmp/notes.md"), Some(42))
        );
    }

    #[test]
    fn malformed_pseudo_schemes_fall_through_to_web() {
        assert_eq!(route("fno-file:relative/path.md"), Route::Web);
        assert_eq!(route("fno-file:/has space.md"), Route::Web);
        assert_eq!(route("fno-file:"), Route::Web);
    }

    #[test]
    fn plain_urls_route_web() {
        assert_eq!(route("https://example.com/a"), Route::Web);
    }
}
