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
/// PageUp/PageDown become navigation tokens; a bare Esc becomes `Esc` - the
/// next byte decides it, or an empty read (the client's quiet-window flush)
/// releases it; every other printable byte is `Byte`, resolved by the caller
/// through the chord table.
pub(super) fn fold_modal_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<ModalKey> {
    if bytes.is_empty() {
        return if crate::keys::take_lone_esc(esc) {
            vec![ModalKey::Esc]
        } else {
            Vec::new()
        };
    }
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
/// swallowed. A lone ESC stays pending until the next byte decides it, or
/// an empty read (the client's quiet-window flush) releases it at once - a
/// bare-Esc close lands on the following keypress (which is swallowed);
/// `q` closes instantly.
pub(super) fn fold_selector_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return if crate::keys::take_lone_esc(esc) {
            vec![0x1b]
        } else {
            Vec::new()
        };
    }
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
    if bytes.is_empty() {
        return if crate::keys::take_lone_esc(esc) {
            vec![SearchKey::Esc]
        } else {
            Vec::new()
        };
    }
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

/// Navigator fold keys. Superset of [`SearchKey`]: the same split-arrow escape
/// fold, but a completed CSI whose final byte is Up/Down/Shift-Tab surfaces as a
/// motion token instead of being swallowed. Every other CSI is
/// still consumed whole, so no escape tail leaks into the query or the pane.
pub(super) enum NavKey {
    Byte(u8),
    Esc,
    Up,
    Down,
    /// Bare Right: reach the selected row (the Enter/goto arm).
    Right,
    /// Bare Left: close (the Esc arm) - back to the pane you came
    /// from. The overlay owns every keystroke, so a bare arrow is free.
    Left,
    ShiftTab,
}

/// Fold navigator-mode bytes. Identical escape-carry semantics to
/// [`fold_search_input`] (whole CSI consumed, split sequences carried across
/// reads via `esc`), except the arrow-Up `ESC [ A`, arrow-Down `ESC [ B`,
/// arrow-Right/Left `ESC [ C`/`ESC [ D`, and Shift-Tab `ESC [ Z` finals become
/// [`NavKey::Up`]/[`Down`]/[`Right`]/[`Left`]/[`ShiftTab`] so the navigator can
/// move its cursor, goto, close, and reverse-cycle the state chip. A modified
/// arrow (`ESC [ 1; 5 A`) shares the final byte and maps to the same motion -
/// harmless. All other finals are swallowed, same leak-safety as search.
pub(super) fn fold_nav_input(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<NavKey> {
    if bytes.is_empty() {
        return if crate::keys::take_lone_esc(esc) {
            vec![NavKey::Esc]
        } else {
            Vec::new()
        };
    }
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(NavKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b);
                } else {
                    esc.clear();
                    keys.push(NavKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b);
                    } else {
                        keys.push(NavKey::Byte(b));
                    }
                }
            }
            _ => {
                if b == 0x1b {
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) {
                    // CSI complete. Surface the three motion finals; swallow the
                    // rest. Only a BARE `ESC [ X` counts: a parameterised
                    // sequence is a MODIFIED key (Ctrl-Up is `ESC [ 1; 5 A`),
                    // and aliasing it onto the unmodified one silently
                    // reinterprets a chord the operator meant as something else.
                    // This fold serves prefix+f, the navigator this change
                    // promotes to the primary route, so it is the last place
                    // that should guess.
                    if esc.len() == 2 {
                        match b {
                            b'A' => keys.push(NavKey::Up),
                            b'B' => keys.push(NavKey::Down),
                            b'C' => keys.push(NavKey::Right),
                            b'D' => keys.push(NavKey::Left),
                            b'Z' => keys.push(NavKey::ShiftTab),
                            _ => {}
                        }
                    }
                    esc.clear();
                } else if esc.len() >= MAX_ESC_CARRY {
                    esc.clear();
                } else {
                    esc.push(b);
                }
            }
        }
    }
    keys
}
