//! Raw-bytes to key-token folding for the client's overlay key folders (
//! move out of `client.rs` for the shrink-only ratchet). Behavior is preserved
//! byte-for-byte; each folder keeps its own escape-carry contract.

/// A folded which-key modal key (US3). Arrows/pgup navigate the
/// reference; `Byte`/`Enter` execute; `Esc` dismisses. Distinct from
/// [`fold_selector_keys`] because the modal needs arrows kept as navigation
/// (not folded to hjkl, which are executable bindings) and pgup/pgdn as scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModalKey {
    Byte(u8),
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
}
/// The ceiling on a partially-read escape sequence, shared by all four folds.
/// A real CSI is far shorter, so this only ever fires on a pathological stream,
/// and it is what stops one from growing the carry without limit.
pub(super) const MAX_ESC_CARRY: usize = 16;
/// Fold raw modal-mode bytes into [`ModalKey`]s, carrying escape state in `esc`
/// ACROSS reads (same split-arrow safety as [`fold_selector_keys`]). Arrows and
/// PageUp/PageDown become navigation tokens; a bare Esc (a lone `0x1b` chunk is
/// special-cased by the caller for instant close) becomes `Esc`; every other
/// printable byte is `Byte`, resolved by the caller through the chord table.
pub(super) fn fold_modal_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<ModalKey> {
    let mut out = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            match (esc.as_slice(), b) {
                ([0x1b], b'[') => {
                    esc.push(b);
                    continue;
                }
                ([0x1b], _) => {
                    // The pending ESC was a bare Esc press; emit it, then let the
                    // fresh byte fall through to be processed below.
                    out.push(ModalKey::Esc);
                    esc.clear();
                }
                ([0x1b, b'['], b'A') => {
                    out.push(ModalKey::Up);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'B') => {
                    out.push(ModalKey::Down);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'C') => {
                    out.push(ModalKey::Right);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'D') => {
                    out.push(ModalKey::Left);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'5') | ([0x1b, b'['], b'6') => {
                    esc.push(b); // PageUp `ESC[5~` / PageDown `ESC[6~` pending
                    continue;
                }
                ([0x1b, b'[', b'5'], b'~') => {
                    out.push(ModalKey::PageUp);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'[', b'6'], b'~') => {
                    out.push(ModalKey::PageDown);
                    esc.clear();
                    continue;
                }
                _ => {
                    // Inside a CSI (`ESC [ ...`): consume the WHOLE sequence.
                    // "swallow it whole (never leak)" used to be a comment
                    // rather than a behaviour here: this arm dropped ONE byte
                    // and let the rest of the sequence fall through as plain
                    // keys, so Ctrl-Up (`ESC [ 1; 5 A`) leaked `;`, `5`, `A`.
                    // Same defect the selector fold had, same fix.
                    if b == 0x1b {
                        // ESC aborts an in-progress sequence and starts a fresh
                        // one, so a cancel is never eaten as a parameter.
                        esc.clear();
                        esc.push(0x1b);
                        continue;
                    }
                    if (0x40..=0x7e).contains(&b) || esc.len() >= MAX_ESC_CARRY {
                        esc.clear();
                        continue;
                    }
                    if (0x20..=0x3f).contains(&b) {
                        esc.push(b);
                        continue;
                    }
                    // A C0 control mid-sequence is malformed: abandon the
                    // sequence and reprocess the byte below rather than losing it.
                    esc.clear();
                }
            }
        }
        match b {
            0x1b => esc.push(0x1b),
            b'\r' | b'\n' => out.push(ModalKey::Enter),
            _ => out.push(ModalKey::Byte(b)),
        }
    }
    out
}
/// One folded search-input token (v12). A printable/control byte for the
/// query, or a bare Esc press. Complete arrow sequences are swallowed by the fold
/// (cursor motion is discretionary polish, Discretion 4).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SearchKey {
    Byte(u8),
    Esc,
}
/// Fold raw selector-mode bytes into simple key bytes, carrying escape state
/// in `esc` ACROSS reads (gemini medium: an arrow sequence split at a read
/// boundary must neither close the selector nor leak its tail into the
/// pane). Arrows map to their hjkl twins; unknown escape tails are
/// swallowed. A lone ESC stays pending until the next byte decides it - a
/// bare-Esc close lands on the following keypress (which is swallowed);
/// `q` closes instantly.
pub(super) fn fold_selector_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    let mut keys = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            if esc.as_slice() == [0x1b] && b == b'[' {
                esc.push(b);
                continue;
            }
            if esc.first() == Some(&0x1b) && esc.get(1) == Some(&b'[') {
                // Inside a CSI sequence. Consume until its FINAL byte (0x40-0x7E)
                // rather than dropping one byte and letting the tail out.
                //
                // "swallowed whole" used to be a comment, not a behaviour: a
                // modified arrow like Ctrl-Up (`ESC [ 1; 5 A`) had its `1`
                // swallowed and then leaked `;`, `5` and `A` as plain keys. That
                // was survivable while these overlays closed on any key they did
                // not recognise. Once a picker gained a cursor it was not: the
                // leaked `5` reads as a digit, which in the move picker COMMITS,
                // and the leaked `A`/`H` reads as an uppercase split key, which
                // in the attach picker commits too. A function key (`ESC [ 1 5 ~`)
                // leaks a digit the same way.
                //
                // Five overlays share this fold, so fixing it here fixes every
                // door at once instead of guarding the two that were probed.
                if (0x40..=0x7e).contains(&b) {
                    // A BARE `ESC [ X` is a plain arrow. A parameterised one is a
                    // modified arrow (ctrl/shift/alt) and means something this
                    // layer has no mapping for, so it is dropped entirely rather
                    // than aliased onto the unmodified key.
                    if esc.len() == 2 {
                        match b {
                            b'A' => keys.push(b'k'),
                            b'B' => keys.push(b'j'),
                            b'C' => keys.push(b'l'),
                            b'D' => keys.push(b'h'),
                            _ => {} // unknown final byte: swallowed whole
                        }
                    }
                    esc.clear();
                    continue;
                }
                if (0x20..=0x3f).contains(&b) && esc.len() < MAX_ESC_CARRY {
                    // A real parameter or intermediate byte (ECMA-48): keep
                    // accumulating, up to the shared ceiling.
                    esc.push(b);
                    continue;
                }
                if esc.len() >= MAX_ESC_CARRY {
                    // A pathological run of parameter bytes: drop the sequence
                    // rather than growing the carry without limit.
                    esc.clear();
                    continue;
                }
                // Anything else is malformed: a C0 control landed mid-sequence.
                // Treating it as a parameter would strand the parser and eat the
                // operator's escape hatch - a truncated `ESC [` in the carry (an
                // Alt-`[` press emits exactly that) would swallow the Esc meant
                // to cancel the picker, and then swallow the following `q` too,
                // because `q` is in the final-byte range. Abandon the sequence
                // and let the byte be handled as if it arrived fresh, so a
                // cancel always reaches the overlay. This also bounds the carry:
                // it can only ever hold parameter bytes.
                esc.clear();
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(b);
                }
                continue;
            }
            // Pending [ESC] + a non-'[' byte: that ESC was a bare Esc press.
            esc.clear();
            keys.push(0x1b);
            if b == 0x1b {
                esc.push(0x1b); // and a new one just started
            }
            continue;
        }
        if b == 0x1b {
            esc.push(0x1b);
            continue;
        }
        keys.push(b);
    }
    keys
}
pub(super) fn fold_search_input(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<SearchKey> {
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(SearchKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b); // CSI introducer: start accumulating the sequence
                } else {
                    // A lone [ESC] then a non-'[' byte: that ESC was a bare Esc
                    // press. Surface it, then reprocess `b`.
                    esc.clear();
                    keys.push(SearchKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b); // a new ESC just started
                    } else {
                        keys.push(SearchKey::Byte(b));
                    }
                }
            }
            // Inside a CSI (`ESC [ ...`): keep eating param/intermediate bytes,
            // swallowing the whole sequence at its final byte. Bounded so a
            // pathological stream can never grow `esc` without limit.
            // ponytail: 16-byte ceiling; real CSI sequences are far shorter.
            _ => {
                if b == 0x1b {
                    // ESC aborts any in-progress sequence and starts a fresh
                    // one (standard VT semantics). Without this, an ESC arriving
                    // mid-CSI (a split sequence in the buffer) would be eaten as
                    // a param byte, so pressing Esc to cancel search would
                    // silently fail. (gemini review, HIGH)
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) || esc.len() >= 16 {
                    esc.clear();
                } else {
                    esc.push(b);
                }
            }
        }
    }
    keys
}
