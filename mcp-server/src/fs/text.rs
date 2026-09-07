//! Text file helpers: UTF-8 BOM and line-ending preservation
//! (pi-spec §20, §21).

pub const BOM: &str = "\u{FEFF}";

/// Split a leading UTF-8 BOM (U+FEFF) off the text.
pub fn strip_bom(text: &str) -> (bool, &str) {
    match text.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

/// Re-attach the BOM that was stripped before editing.
pub fn restore_bom(had_bom: bool, content: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(content.len() + 3);
    if had_bom {
        bytes.extend_from_slice(BOM.as_bytes());
    }
    bytes.extend_from_slice(content.as_bytes());
    bytes
}

/// CRLF when CRLF endings dominate the file (a tie counts as LF-only).
pub fn is_crlf(content: &str) -> bool {
    let crlf = content.matches("\r\n").count();
    let lone_lf = content.matches('\n').count().saturating_sub(crlf);
    crlf > 0 && crlf > lone_lf
}

/// Convert inserted replacement text to the file's line-ending style so an
/// edit never leaks foreign endings into the file.
pub fn adopt_line_endings(text: &str, crlf: bool) -> String {
    let lf = text.replace("\r\n", "\n");
    if crlf {
        lf.replace('\n', "\r\n")
    } else {
        lf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_strip_and_restore_roundtrip() {
        assert_eq!(strip_bom("hello"), (false, "hello"));
        let (had, rest) = strip_bom("\u{FEFF}hello");
        assert!(had);
        assert_eq!(rest, "hello");
        assert_eq!(restore_bom(had, rest), "\u{FEFF}hello".as_bytes());
        assert_eq!(restore_bom(false, "x"), b"x");
    }

    #[test]
    fn crlf_detection() {
        assert!(is_crlf("a\r\nb\r\n"));
        assert!(is_crlf("a\r\nb\r\nc\r\nd\r\n")); // dominant CRLF
        assert!(!is_crlf("a\nb\n"));
        assert!(!is_crlf("single line"));
        assert!(!is_crlf(""));
        // mixed: lone LF dominates -> LF file
        assert!(!is_crlf("a\r\nb\nc\nd\n"));
    }

    #[test]
    fn adopt_line_endings_matches_target_style() {
        assert_eq!(adopt_line_endings("x\ny", true), "x\r\ny");
        assert_eq!(adopt_line_endings("x\r\ny", true), "x\r\ny"); // no doubling
        assert_eq!(adopt_line_endings("x\r\ny", false), "x\ny");
        assert_eq!(adopt_line_endings("x\ny", false), "x\ny");
    }
}
