//! `edit` tool (pi-spec §13–§23): multiple disjoint replacements matched
//! against the ORIGINAL file contents, all-or-nothing, with CRLF/BOM
//! preservation, limited normalized fallback, and a diff of what changed.

use std::ops::Range;

use unicode_normalization::UnicodeNormalization;

use crate::config::Config;
use crate::fs::atomic_write::write_atomic;
use crate::fs::mutation_queue;
use crate::fs::path::resolve_in_workspace;
use crate::fs::text;
use crate::tools::{EditOutput, EditParams};

const BINARY_SNIFF_LEN: usize = 8192;
const DIFF_CONTEXT_LINES: usize = 3;

pub async fn run(config: &Config, params: &EditParams) -> Result<EditOutput, String> {
    let path = params.path.as_str();
    let edits = &params.edits;
    if edits.is_empty() {
        return Err("edits must contain at least one replacement".to_string());
    }
    for entry in edits {
        if entry.old_text.is_empty() {
            return Err("oldText must not be empty".to_string());
        }
    }

    let resolved = resolve_in_workspace(&config.workspace, path)?;
    // Whole mutation transaction under the per-file lock (§24): read current
    // content → validate → modify → write final content.
    let _guard = mutation_queue::global().lock(&resolved).await;

    let original_bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|_| format!("File not found: {path}"))?;
    if original_bytes[..original_bytes.len().min(BINARY_SNIFF_LEN)].contains(&0) {
        return Err(format!("{path} appears to be a binary file"));
    }
    let original_text = String::from_utf8(original_bytes)
        .map_err(|_| format!("{path} is not valid UTF-8 text"))?;
    let (had_bom, content) = text::strip_bom(&original_text);
    let crlf = text::is_crlf(content);

    // Match every edit against the ORIGINAL contents (§15).
    let mut matches: Vec<(usize, Range<usize>, String)> = Vec::new();
    for (index, entry) in edits.iter().enumerate() {
        let range = match find_unique_range(content, &entry.old_text) {
            Ok(range) => range,
            Err(MatchError::NotFound) => return Err(not_found_message(index, path, edits.len())),
            Err(MatchError::Multiple(count)) => {
                return Err(multiple_match_message(index, count, path, edits.len()))
            }
        };
        matches.push((index, range, text::adopt_line_endings(&entry.new_text, crlf)));
    }

    // Overlap detection (§17): reject the whole call if any two ranges touch.
    if matches.len() > 1 {
        let mut sorted: Vec<(usize, Range<usize>)> =
            matches.iter().map(|(i, r, _)| (*i, r.clone())).collect();
        sorted.sort_by_key(|(_, range)| range.start);
        for pair in sorted.windows(2) {
            let (a_index, a_range) = &pair[0];
            let (b_index, b_range) = &pair[1];
            if b_range.start < a_range.end {
                let (low, high) = (a_index.min(b_index), a_index.max(b_index));
                return Err(format!(
                    "edits[{low}] and edits[{high}] overlap in {path}.\nMerge them into one edit or target disjoint regions."
                ));
            }
        }
    }

    // Apply from the tail of the file towards the head so earlier
    // replacements never shift later byte offsets (§15).
    let mut final_content = content.to_string();
    let mut by_start = matches.clone();
    by_start.sort_by_key(|(_, range, _)| std::cmp::Reverse(range.start));
    for (_, range, new_text) in by_start {
        final_content.replace_range(range, &new_text);
    }

    if final_content == content {
        return Err(format!(
            "No changes made to {path}.\nThe replacement produced identical content."
        ));
    }

    // Diff view (§23): unified hunks for the agent, full patch, and the first
    // changed line number in the new file.
    let diff = similar::TextDiff::from_lines(content, final_content.as_str());
    let first_changed_line = diff
        .ops()
        .iter()
        .find(|op| !matches!(op.tag(), similar::DiffTag::Equal))
        .map(|op| op.new_range().start + 1);
    let mut unified = diff.unified_diff();
    unified.context_radius(DIFF_CONTEXT_LINES);
    let diff_text: String = unified.iter_hunks().map(|hunk| hunk.to_string()).collect();
    unified.header(&format!("a/{path}"), &format!("b/{path}"));
    let patch = unified.to_string();

    // Single atomic commit (§38), BOM restored (§21).
    let bytes = text::restore_bom(had_bom, &final_content);
    write_atomic(&resolved, &bytes)
        .await
        .map_err(|e| format!("failed to write {path}: {e}"))?;

    Ok(EditOutput {
        success: true,
        path: path.to_string(),
        replacements: edits.len(),
        first_changed_line: first_changed_line.map(|line| line as u32),
        diff: diff_text,
        patch,
    })
}

fn not_found_message(index: usize, path: &str, total: usize) -> String {
    if total == 1 {
        format!(
            "Could not find the text in {path}.\nThe oldText must identify the intended text in the file."
        )
    } else {
        format!(
            "Could not find edits[{index}] in {path}.\nProvide a larger unique oldText block from the current file contents."
        )
    }
}

fn multiple_match_message(index: usize, count: usize, path: &str, total: usize) -> String {
    let subject = if total == 1 { "the text".to_string() } else { format!("edits[{index}]") };
    format!(
        "Found {count} occurrences of {subject} in {path}.\nEach oldText must be unique. Please provide more context to make it unique."
    )
}

// ---------------------------------------------------------------------------
// Matching: exact first, then limited normalized fallback (§16, §22)
// ---------------------------------------------------------------------------

enum MatchError {
    NotFound,
    Multiple(usize),
}

/// Locate `old_text` in `content`: an exact unique match, or a unique match
/// after Unicode normalization. Returns the byte range in the ORIGINAL text.
fn find_unique_range(content: &str, old_text: &str) -> Result<Range<usize>, MatchError> {
    // Exact pass.
    let mut exact: Option<Range<usize>> = None;
    let mut count = 0usize;
    for (byte_index, _) in content.match_indices(old_text) {
        count += 1;
        if count == 1 {
            exact = Some(byte_index..byte_index + old_text.len());
        }
    }
    match count {
        1 => return Ok(exact.expect("first match recorded")),
        n if n > 1 => return Err(MatchError::Multiple(n)),
        _ => {}
    }

    // Normalized fallback (§22): NFKC, smart quotes/dashes/spaces to ASCII,
    // CRLF/CR to LF, trailing whitespace per line removed.
    let normalized = NormalizedText::new(content);
    let needle = normalize_chars(old_text);
    if needle.is_empty() {
        return Err(MatchError::NotFound);
    }
    let positions = find_all_char_positions(&normalized.chars, &needle);
    match positions.len() {
        0 => Err(MatchError::NotFound),
        1 => {
            let start = positions[0];
            let end = start + needle.len();
            Ok(normalized.original_range(start, end))
        }
        n => Err(MatchError::Multiple(n)),
    }
}

/// One-directional normalization with a map back to original char indices.
struct NormalizedText {
    chars: Vec<char>,
    /// Per normalized char: (original char index, original chars consumed).
    map: Vec<(usize, usize)>,
    orig_chars: Vec<char>,
    orig_byte_start: Vec<usize>,
}

impl NormalizedText {
    fn new(original: &str) -> Self {
        let orig_chars: Vec<char> = original.chars().collect();
        let mut orig_byte_start = Vec::with_capacity(orig_chars.len());
        let mut offset = 0usize;
        for ch in &orig_chars {
            orig_byte_start.push(offset);
            offset += ch.len_utf8();
        }

        let mut chars: Vec<char> = Vec::with_capacity(orig_chars.len());
        let mut map: Vec<(usize, usize)> = Vec::with_capacity(orig_chars.len());
        // Trailing whitespace per line is held back and dropped when the line
        // ends; deletions are attributed to the newline that follows.
        let mut pending_ws: Vec<usize> = Vec::new();

        let mut i = 0usize;
        while i < orig_chars.len() {
            let ch = orig_chars[i];
            if ch == '\r' {
                let take = if i + 1 < orig_chars.len() && orig_chars[i + 1] == '\n' {
                    2
                } else {
                    1
                };
                push_newline(&mut chars, &mut map, &mut pending_ws, i, take);
                i += take;
                continue;
            }
            if ch == '\n' {
                push_newline(&mut chars, &mut map, &mut pending_ws, i, 1);
                i += 1;
                continue;
            }
            let mapped = map_char(ch);
            if mapped == ' ' || mapped == '\t' {
                pending_ws.push(i);
                i += 1;
                continue;
            }
            for ws_index in pending_ws.drain(..) {
                chars.push(' ');
                map.push((ws_index, 1));
            }
            push_nfkc(&mut chars, &mut map, mapped, i);
            i += 1;
        }
        // Whitespace pending at EOF is trailing whitespace: dropped.
        NormalizedText { chars, map, orig_chars, orig_byte_start }
    }

    /// Map a normalized char range [start, end) back to original bytes.
    fn original_range(&self, start: usize, end: usize) -> Range<usize> {
        let (first_char, _) = self.map[start];
        let (last_char, consumed) = self.map[end - 1];
        let last_char = last_char + consumed - 1;
        self.orig_byte_start[first_char]
            ..self.orig_byte_start[last_char] + self.orig_chars[last_char].len_utf8()
    }
}

fn push_newline(
    chars: &mut Vec<char>,
    map: &mut Vec<(usize, usize)>,
    pending_ws: &mut Vec<usize>,
    newline_index: usize,
    take: usize,
) {
    // The newline consumes any pending trailing whitespace: its map entry
    // starts at the first whitespace char and spans through the newline.
    let start = pending_ws.first().copied().unwrap_or(newline_index);
    let consumed = newline_index - start + take;
    chars.push('\n');
    map.push((start, consumed));
    pending_ws.clear();
}

fn push_nfkc(chars: &mut Vec<char>, map: &mut Vec<(usize, usize)>, ch: char, orig_index: usize) {
    if ch.is_ascii() {
        // ASCII is NFKC-invariant; skip the allocation.
        chars.push(ch);
        map.push((orig_index, 1));
        return;
    }
    let expanded: String = ch.nfkc().collect();
    for normalized_char in expanded.chars() {
        chars.push(normalized_char);
        map.push((orig_index, 1));
    }
}

/// Character-level normalization without position mapping (for needles).
/// Mirrors [`NormalizedText::new`]: CRLF/CR collapse to one LF, trailing
/// whitespace per line is dropped, internal whitespace is kept (one ' ' per
/// whitespace character), smart quotes/dashes/spaces fold to ASCII.
fn normalize_chars(input: &str) -> Vec<char> {
    let mut chars: Vec<char> = Vec::with_capacity(input.len());
    let mut pending_ws = 0usize;
    let mut prev_cr = false;
    for ch in input.chars() {
        if ch == '\r' {
            chars.push('\n');
            pending_ws = 0;
            prev_cr = true;
            continue;
        }
        if ch == '\n' {
            if prev_cr {
                // CRLF already emitted its LF.
                prev_cr = false;
                continue;
            }
            chars.push('\n');
            pending_ws = 0;
            continue;
        }
        prev_cr = false;
        let mapped = map_char(ch);
        if mapped == ' ' || mapped == '\t' {
            pending_ws += 1;
            continue;
        }
        chars.extend(std::iter::repeat_n(' ', pending_ws));
        pending_ws = 0;
        if mapped.is_ascii() {
            chars.push(mapped);
        } else {
            let expanded: String = mapped.nfkc().collect();
            chars.extend(expanded.chars());
        }
    }
    // Trailing whitespace at end of input is dropped.
    chars
}

/// Smart quotes → ASCII quotes, Unicode dashes → '-', exotic spaces → ' '.
fn map_char(ch: char) -> char {
    match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}' => '-',
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        _ => ch,
    }
}

/// All start positions where `needle` occurs in `haystack` (char windows).
fn find_all_char_positions(haystack: &[char], needle: &[char]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    'outer: for start in 0..=(haystack.len() - needle.len()) {
        for (offset, expected) in needle.iter().enumerate() {
            if haystack[start + offset] != *expected {
                continue 'outer;
            }
        }
        positions.push(start);
    }
    positions
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::tools::test_support::config;
    use crate::tools::{EditEntry, EditParams};

    fn params(path: &str, entries: &[(&str, &str)]) -> EditParams {
        EditParams {
            path: path.to_string(),
            edits: entries
                .iter()
                .map(|(old, new)| EditEntry {
                    old_text: old.to_string(),
                    new_text: new.to_string(),
                })
                .collect(),
        }
    }

    async fn setup(dir: &std::path::Path, content: &str) {
        tokio::fs::write(dir.join("f.rs"), content).await.unwrap();
    }

    async fn content_of(dir: &std::path::Path, name: &str) -> String {
        tokio::fs::read_to_string(dir.join(name)).await.unwrap()
    }

    fn structured(result: &EditOutput) -> serde_json::Value {
        serde_json::to_value(result).unwrap()
    }

    #[tokio::test]
    async fn single_replacement_with_diff() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "fn main() {\n    todo!()\n}\n").await;
        let result = run(&config(dir.path()), &params("f.rs", &[("todo!()", "unimplemented!()")]))
            .await
            .unwrap();
        assert_eq!(result.replacements, 1);
        assert!(result.diff.contains("-    todo!()"));
        assert!(result.diff.contains("+    unimplemented!()"));
        assert_eq!(content_of(dir.path(), "f.rs").await, "fn main() {\n    unimplemented!()\n}\n");

        let structured = structured(&result);
        assert_eq!(structured["success"], json!(true));
        assert_eq!(structured["replacements"], json!(1));
        assert_eq!(structured["firstChangedLine"], json!(2));
        assert!(structured["diff"].as_str().unwrap().contains("-    todo!()"));
        assert!(structured["diff"].as_str().unwrap().contains("+    unimplemented!()"));
        assert!(structured["patch"].as_str().unwrap().contains("--- a/f.rs"));
        assert!(structured["patch"].as_str().unwrap().contains("+++ b/f.rs"));
    }

    #[tokio::test]
    async fn multiple_disjoint_replacements() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            "const PORT: u16 = 3000;\n\nfn hello() {\n    println!(\"hello\");\n}\n\nfn goodbye() {\n    println!(\"bye\");\n}\n",
        )
        .await;
        let result = run(
            &config(dir.path()),
            &params(
                "f.rs",
                &[
                    ("const PORT: u16 = 3000;", "const PORT: u16 = 8080;"),
                    ("println!(\"bye\");", "println!(\"goodbye\");"),
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(structured(&result)["replacements"], json!(2));
        assert_eq!(
            content_of(dir.path(), "f.rs").await,
            "const PORT: u16 = 8080;\n\nfn hello() {\n    println!(\"hello\");\n}\n\nfn goodbye() {\n    println!(\"goodbye\");\n}\n"
        );
    }

    #[tokio::test]
    async fn all_edits_match_against_original_content() {
        // edit 1's newText introduces a second copy of edit 2's oldText; if
        // edits were applied sequentially, edit 2 would no longer be unique.
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "X1 X2\n").await;
        run(
            &config(dir.path()),
            &params("f.rs", &[("X1", "Q X2"), ("X2", "Z")]),
        )
        .await
        .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "Q X2 Z\n");
    }

    #[tokio::test]
    async fn not_found_messages() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "alpha\n").await;
        let error = run(&config(dir.path()), &params("f.rs", &[("delta", "x")]))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Could not find the text in f.rs.\nThe oldText must identify the intended text in the file."
        );

        let error = run(
            &config(dir.path()),
            &params("f.rs", &[("alpha", "ok"), ("delta", "x")]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "Could not find edits[1] in f.rs.\nProvide a larger unique oldText block from the current file contents."
        );
    }

    #[tokio::test]
    async fn duplicate_match_message() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "ha ha ha\n").await;
        let error = run(&config(dir.path()), &params("f.rs", &[("ha", "x")]))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Found 3 occurrences of the text in f.rs.\nEach oldText must be unique. Please provide more context to make it unique."
        );

        setup(dir.path(), "unique\nha ha ha ha\n").await;
        let error = run(
            &config(dir.path()),
            &params("f.rs", &[("unique", "u"), ("ha", "x")]),
        )
        .await
        .unwrap_err();
        assert!(error.starts_with("Found 4 occurrences of edits[1] in f.rs."));
    }

    #[tokio::test]
    async fn empty_old_text_rejected() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "a\n").await;
        let error = run(&config(dir.path()), &params("f.rs", &[("", "x")]))
            .await
            .unwrap_err();
        assert_eq!(error, "oldText must not be empty");
    }

    #[tokio::test]
    async fn no_op_replacement_rejected() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "same\n").await;
        let error = run(&config(dir.path()), &params("f.rs", &[("same", "same")]))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "No changes made to f.rs.\nThe replacement produced identical content."
        );
    }

    #[tokio::test]
    async fn overlapping_edits_rejected_without_modification() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "abcdef\n").await;
        let error = run(
            &config(dir.path()),
            &params("f.rs", &[("abcd", "X"), ("cdef", "Y")]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "edits[0] and edits[1] overlap in f.rs.\nMerge them into one edit or target disjoint regions."
        );
        assert_eq!(content_of(dir.path(), "f.rs").await, "abcdef\n");
    }

    #[tokio::test]
    async fn crlf_preserved_and_matched() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "a\r\nb\r\nc\r\n").await;
        // oldText uses LF while the file uses CRLF: exact match fails,
        // normalized fallback finds it, and CRLF is preserved (§20).
        run(&config(dir.path()), &params("f.rs", &[("a\nb", "X\nY")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "X\r\nY\r\nc\r\n");

        // Exact match with CRLF inside oldText also works.
        setup(dir.path(), "a\r\nb\r\n").await;
        run(&config(dir.path()), &params("f.rs", &[("a\r\nb", "Z")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "Z\r\n");
    }

    #[tokio::test]
    async fn bom_preserved() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("f.rs"), "\u{FEFF}hello world\n")
            .await
            .unwrap();
        // oldText without the BOM still matches (§21).
        run(&config(dir.path()), &params("f.rs", &[("hello", "goodbye")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "\u{FEFF}goodbye world\n");
    }

    #[tokio::test]
    async fn unicode_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "fn héllo() { 世界 }\n").await;
        run(&config(dir.path()), &params("f.rs", &[("世界", "世界 🦀")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "fn héllo() { 世界 🦀 }\n");
    }

    #[tokio::test]
    async fn smart_quote_normalization() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "print(‘hello’);\n").await;
        run(&config(dir.path()), &params("f.rs", &[("print('hello');", "print(\"hi\");")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "print(\"hi\");\n");
    }

    #[tokio::test]
    async fn unicode_dash_normalization() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "foo—bar\n").await;
        run(&config(dir.path()), &params("f.rs", &[("foo-bar", "X")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "X\n");
    }

    #[tokio::test]
    async fn trailing_whitespace_normalization() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "hello   \nworld\n").await;
        run(&config(dir.path()), &params("f.rs", &[("hello\n", "hi\n")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "hi\nworld\n");
    }

    #[tokio::test]
    async fn special_space_normalization() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "a\u{00A0}b\n").await;
        run(&config(dir.path()), &params("f.rs", &[("a b", "X")]))
            .await
            .unwrap();
        assert_eq!(content_of(dir.path(), "f.rs").await, "X\n");
    }

    #[tokio::test]
    async fn normalized_ambiguous_match_still_errors() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "‘a’ ‘a’\n").await;
        let error = run(&config(dir.path()), &params("f.rs", &[("'a'", "x")]))
            .await
            .unwrap_err();
        assert!(error.starts_with("Found 2 occurrences"));
    }

    #[tokio::test]
    async fn concurrent_edits_same_file_no_lost_update() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=5).map(|i| format!("line{i}\n")).collect();
        setup(dir.path(), &content).await;

        let mut tasks = Vec::new();
        for i in 1..=5 {
            let config = config(dir.path());
            let entry = (format!("line{i}"), format!("line{i}-edited"));
            tasks.push(tokio::spawn(async move {
                run(&config, &params("f.rs", &[(&entry.0, &entry.1)])).await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let final_content = content_of(dir.path(), "f.rs").await;
        for i in 1..=5 {
            assert!(final_content.contains(&format!("line{i}-edited")), "{final_content}");
        }
    }

    #[tokio::test]
    async fn edit_and_write_race_on_same_file() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), "a\nb\nc\n").await;
        let config = config(dir.path());

        let edit_config = config.clone();
        let edit_task = tokio::spawn(async move {
            run(&edit_config, &params("f.rs", &[("b", "B")])).await
        });
        let write_task = tokio::spawn(async move {
            crate::tools::write::run(
                &config,
                &crate::tools::WriteParams {
                    path: "f.rs".into(),
                    content: "x\ny\nz\n".into(),
                },
            )
            .await
        });
        let edit_result = edit_task.await.unwrap();
        let write_result = write_task.await.unwrap();
        // Both operations are serialized by the mutation queue: the write
        // always succeeds, and the file is never corrupted regardless of
        // which order the two transactions ran in.
        assert!(write_result.is_ok());
        let final_content = content_of(dir.path(), "f.rs").await;
        assert_eq!(final_content, "x\ny\nz\n", "write content must win or edit applies on top");
        // The edit either applied before the write (then overwritten) or ran
        // after it and failed cleanly ("b" no longer exists) — never partial.
        let _ = edit_result;
    }

    #[tokio::test]
    async fn file_not_found_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let error = run(&config(dir.path()), &params("nope.txt", &[("a", "b")]))
            .await
            .unwrap_err();
        assert_eq!(error, "File not found: nope.txt");

        tokio::fs::write(dir.path().join("blob.bin"), [0u8, 1, 0, 2])
            .await
            .unwrap();
        let error = run(&config(dir.path()), &params("blob.bin", &[("a", "b")]))
            .await
            .unwrap_err();
        assert!(error.contains("binary"));
    }
}
