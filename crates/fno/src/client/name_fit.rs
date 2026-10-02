/// Pad `s` with spaces to `width` columns. A longer `s` stays whole: the
/// overlay it lands in grows to fit it, then wraps.
pub(crate) fn pad_to(s: &str, width: usize) -> String {
    let cols = crate::chrome::str_cols(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(cols)))
}
