//! Shared output truncation (pi-spec §3): 2000 lines or 50 KiB, whichever
//! comes first. `read` keeps the head (agents read files front-to-back);
//! `bash` keeps the tail (errors and summaries land at the end).

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

/// KiB with one decimal, e.g. `50.0KB`.
pub fn format_kb(bytes: usize) -> String {
    format!("{:.1}KB", bytes as f64 / 1024.0)
}

/// Number of lines, counting a final unterminated line as a line.
pub fn count_lines(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

/// Head truncation result.
#[derive(Debug)]
pub struct Head {
    /// The kept text (whole lines only).
    pub text: String,
    /// How many leading lines were kept.
    pub kept_lines: usize,
    /// Stopped early because the byte budget would be exceeded.
    pub byte_limited: bool,
    /// Even the first line alone exceeds the byte budget.
    pub first_line_too_big: bool,
}

/// Keep the first `max_lines` lines within `max_bytes` (pi-spec §3).
pub fn take_head(text: &str, max_lines: usize, max_bytes: usize) -> Head {
    let parts: Vec<&str> = text.split_inclusive('\n').collect();
    let total = parts.len();
    if text.len() <= max_bytes && total <= max_lines {
        return Head {
            text: text.to_string(),
            kept_lines: total,
            byte_limited: false,
            first_line_too_big: false,
        };
    }

    let mut kept = 0usize;
    let mut bytes = 0usize;
    let mut byte_limited = false;
    while kept < max_lines && kept < total {
        let part = parts[kept];
        if bytes + part.len() > max_bytes {
            byte_limited = true;
            break;
        }
        bytes += part.len();
        kept += 1;
    }

    Head {
        text: parts[..kept].concat(),
        kept_lines: kept,
        byte_limited: byte_limited && kept < total,
        first_line_too_big: kept == 0 && !parts.is_empty(),
    }
}

/// Tail truncation result.
#[derive(Debug)]
pub struct Tail {
    /// The kept text (whole lines, or a byte-sliced tail of one giant line).
    pub text: String,
    /// 1-based line number (in the original text) of the first kept line.
    pub first_line: usize,
    /// How many lines the kept text spans.
    pub kept_lines: usize,
    /// Stopped early because of the byte budget.
    pub byte_limited: bool,
    /// The kept text begins inside a line that alone exceeded the budget.
    pub cut_mid_line: bool,
    /// Whether anything was dropped at all.
    pub truncated: bool,
}

/// Keep the last `max_lines` lines within `max_bytes` (pi-spec §3, §32).
pub fn take_tail(text: &str, max_lines: usize, max_bytes: usize) -> Tail {
    let max_lines = max_lines.max(1);
    let parts: Vec<&str> = text.split_inclusive('\n').collect();
    let total = parts.len();
    if text.len() <= max_bytes && total <= max_lines {
        return Tail {
            text: text.to_string(),
            first_line: 1,
            kept_lines: total,
            byte_limited: false,
            cut_mid_line: false,
            truncated: false,
        };
    }

    let mut start = total.saturating_sub(max_lines);
    let mut bytes: usize = parts[start..].iter().map(|p| p.len()).sum();
    let mut byte_limited = false;
    let mut cut_mid_line = false;
    while bytes > max_bytes {
        if start == total - 1 {
            // The last line alone exceeds the budget: keep its final
            // `max_bytes` bytes (UTF-8 boundary safe, pi-spec §32).
            byte_limited = true;
            cut_mid_line = true;
            break;
        }
        bytes -= parts[start].len();
        start += 1;
        byte_limited = true;
    }

    let mut kept = parts[start..].concat();
    if cut_mid_line {
        let over = kept.len().saturating_sub(max_bytes);
        let mut cut = over;
        while cut < kept.len() && !kept.is_char_boundary(cut) {
            cut += 1;
        }
        kept = kept[cut..].to_string();
    }

    Tail {
        text: kept,
        first_line: start + 1,
        kept_lines: total - start,
        byte_limited,
        cut_mid_line,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_small_text_untouched() {
        let head = take_head("a\nb\nc\n", MAX_LINES, MAX_BYTES);
        assert_eq!(head.text, "a\nb\nc\n");
        assert_eq!(head.kept_lines, 3);
        assert!(!head.byte_limited);
        assert!(!head.first_line_too_big);
    }

    #[test]
    fn head_line_cap() {
        let text: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
        let head = take_head(&text, MAX_LINES, MAX_BYTES);
        assert_eq!(head.kept_lines, 2000);
        assert!(head.text.starts_with("line 1\n"));
        assert!(head.text.ends_with("line 2000\n"));
        assert!(!head.byte_limited);
    }

    #[test]
    fn head_byte_cap() {
        let line = format!("{}\n", "a".repeat(300));
        let text = line.repeat(300); // 300 lines × 301 bytes ≈ 90KB
        let head = take_head(&text, MAX_LINES, MAX_BYTES);
        assert!(head.kept_lines < 300);
        assert!(head.byte_limited);
        assert_eq!(head.text.lines().count(), head.kept_lines);
        assert!(head.text.len() <= MAX_BYTES);
    }

    #[test]
    fn head_first_line_too_big() {
        let text = format!("{}\nb\n", "x".repeat(60 * 1024));
        let head = take_head(&text, MAX_LINES, MAX_BYTES);
        assert!(head.first_line_too_big);
        assert_eq!(head.kept_lines, 0);
        assert!(head.text.is_empty());
    }

    #[test]
    fn tail_small_text_untouched() {
        let tail = take_tail("a\nb\n", MAX_LINES, MAX_BYTES);
        assert!(!tail.truncated);
        assert_eq!(tail.text, "a\nb\n");
    }

    #[test]
    fn tail_line_cap_keeps_last_2000() {
        let text: String = (1..=30000).map(|i| format!("line {i}\n")).collect();
        let tail = take_tail(&text, MAX_LINES, MAX_BYTES);
        assert!(tail.truncated);
        assert!(!tail.byte_limited);
        assert_eq!(tail.first_line, 28001);
        assert_eq!(tail.kept_lines, 2000);
        assert!(tail.text.starts_with("line 28001\n"));
        assert!(tail.text.ends_with("line 30000\n"));
    }

    #[test]
    fn tail_byte_cap_drops_front_lines() {
        let line = format!("{}\n", "a".repeat(100));
        let text = line.repeat(3000); // last 2000 lines = 200KB > 50KB
        let tail = take_tail(&text, MAX_LINES, MAX_BYTES);
        assert!(tail.truncated);
        assert!(tail.byte_limited);
        assert!(!tail.cut_mid_line);
        assert!(tail.text.len() <= MAX_BYTES);
        assert!(tail.text.ends_with(&line));
        // shown range must be reported for the message
        assert_eq!(tail.first_line + tail.kept_lines - 1, 3000);
    }

    #[test]
    fn tail_giant_single_line_cut_at_utf8_boundary() {
        let text = "é".repeat(30000); // 60000 bytes, no newline
        let tail = take_tail(&text, MAX_LINES, MAX_BYTES);
        assert!(tail.truncated);
        assert!(tail.cut_mid_line);
        assert!(tail.byte_limited);
        assert_eq!(tail.first_line, 1);
        let kept_chars = tail.text.chars().count();
        assert!((25590..=25610).contains(&kept_chars), "kept {kept_chars} chars");
        assert!(tail.text.len() >= MAX_BYTES - 4 && tail.text.len() <= MAX_BYTES + 4);
        // every kept char is a complete é
        assert!(tail.text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn count_lines_variants() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("one"), 1);
        assert_eq!(count_lines("one\n"), 1);
        assert_eq!(count_lines("one\ntwo"), 2);
        assert_eq!(count_lines("one\ntwo\n"), 2);
    }

    #[tokio::test]
    async fn probe_let_binding_visibility() {
        async fn helper(_dir: &str, _args: &str) -> Result<String, String> {
            Ok("line 100\n".to_string())
        }
        let dir = "d";
        let window = helper(dir, r#"{"path":"big.txt","offset":100,"limit":50}"#)
            .await
            .unwrap();
        assert!(window.starts_with("line 100\n"));
    }

    #[test]
    fn kb_formatting() {
        assert_eq!(format_kb(50 * 1024), "50.0KB");
        assert_eq!(format_kb(60 * 1024), "60.0KB");
        assert_eq!(format_kb(85350), "83.3KB");
    }
}
