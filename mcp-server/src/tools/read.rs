//! `read` tool (pi-spec §4–§9): text files with offset/limit pagination,
//! head truncation (2000 lines / 50 KiB), giant-line notices, and image
//! support (jpg/jpeg/png/gif/webp/bmp, downscaled to ≤2000×2000).

use crate::tools::ReadOutput;

use crate::config::Config;
use crate::fs::path::resolve_in_workspace;
use crate::output::truncate::{self, MAX_BYTES, MAX_LINES};
use crate::tools::append_note;

/// Hard ceiling for a single read; larger files must go through bash.
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;
/// Images larger than this are downscaled (aspect ratio preserved).
const IMAGE_MAX_DIM: u32 = 2000;
const BINARY_SNIFF_LEN: usize = 8192;

pub async fn run(
    config: &Config,
    path: &str,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<ReadOutput, String> {
    let offset = offset.unwrap_or(1);
    if offset == 0 {
        return Err("Invalid offset: must be at least 1 (lines are 1-based)".to_string());
    }
    if limit == Some(0) {
        return Err("Invalid limit: must be at least 1".to_string());
    }

    let resolved = resolve_in_workspace(&config.workspace, path)?;
    let metadata = tokio::fs::metadata(&resolved)
        .await
        .map_err(|_| format!("File not found: {path}"))?;
    if metadata.is_dir() {
        return Err(format!(
            "{path} is a directory; use bash (e.g. `ls -la {path}`) to inspect it"
        ));
    }
    if metadata.len() > MAX_READ_BYTES {
        return Err(format!(
            "{path} is {} bytes, above the {} byte read limit; use bash to read it in chunks",
            metadata.len(),
            MAX_READ_BYTES
        ));
    }
    let bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|e| format!("failed to read {path}: {e}"))?;

    // Images (pi-spec §9): detected by content; the caller attaches the MCP
    // image block, the structured output describes it.
    if let Some(mime) = detect_image_mime(&bytes)? {
        return Ok(ReadOutput {
            path: path.to_string(),
            content: format!("Read image file [{mime}]"),
            start_line: None,
            end_line: None,
            total_lines: None,
            image_mime_type: Some(mime.to_string()),
        });
    }

    if bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0) {
        return Err(format!("{path} appears to be a binary file; use bash to inspect it"));
    }
    let content =
        String::from_utf8(bytes).map_err(|_| format!("{path} is not valid UTF-8 text"))?;
    if content.is_empty() {
        return Ok(ReadOutput {
            path: path.to_string(),
            content: "(file is empty)".to_string(),
            start_line: None,
            end_line: None,
            total_lines: Some(0),
            image_mime_type: None,
        });
    }

    // Line window (pi-spec §6).
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    let start = offset as usize - 1;
    if start >= total {
        return Err(format!("Offset {offset} is beyond end of file ({total} lines total)"));
    }

    let line_budget = limit.map(|l| l as usize).unwrap_or(MAX_LINES).min(MAX_LINES);
    let user_limited = limit.is_some_and(|l| (l as usize) < MAX_LINES);

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes_used = 0usize;
    let mut byte_capped = false;
    let mut end = start; // exclusive index: lines[start..end] shown
    while end < total && kept.len() < line_budget {
        let line = lines[end];
        if bytes_used + line.len() > MAX_BYTES {
            byte_capped = true;
            break;
        }
        bytes_used += line.len();
        kept.push(line);
        end += 1;
    }

    if kept.is_empty() {
        // The very first line of the window exceeds the byte budget: never
        // return half a line (pi-spec §7).
        let size = lines[start].len();
        return Ok(ReadOutput {
            path: path.to_string(),
            content: format!(
                "[Line {} is {}, exceeds {} limit.\nUse bash to inspect the line in chunks.]",
                start + 1,
                truncate::format_kb(size),
                truncate::format_kb(MAX_BYTES)
            ),
            start_line: Some(start as u32 + 1),
            end_line: Some(start as u32 + 1),
            total_lines: Some(total as u32),
            image_mime_type: None,
        });
    }

    let mut text: String = kept.concat();
    let first = start + 1; // 1-based first shown line
    let last = end; // 1-based last shown line (index end-1)
    let more_remain = end < total;
    let budget_hit = kept.len() == line_budget && more_remain;

    if byte_capped || budget_hit {
        let note = if byte_capped {
            format!(
                "[Showing lines {first}-{last} of {total} ({} limit). Use offset={} to continue.]",
                truncate::format_kb(MAX_BYTES),
                last + 1
            )
        } else if user_limited {
            format!(
                "[{} more lines in file. Use offset={} to continue.]",
                total - last,
                last + 1
            )
        } else {
            format!("[Showing lines {first}-{last} of {total}. Use offset={} to continue.]", last + 1)
        };
        append_note(&mut text, note);
    }
    Ok(ReadOutput {
        path: path.to_string(),
        content: text,
        start_line: Some(first as u32),
        end_line: Some(last as u32),
        total_lines: Some(total as u32),
        image_mime_type: None,
    })
}

/// Detect whether the bytes are a supported image; returns the MIME type
/// (post-downscale it is always re-encoded as PNG, pi-spec §9).
fn detect_image_mime(bytes: &[u8]) -> Result<Option<&'static str>, String> {
    use image::ImageFormat;
    let Ok(format) = image::guess_format(bytes) else {
        return Ok(None);
    };
    Ok(match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Bmp => Some("image/bmp"),
        _ => None,
    })
}

/// Load, downscale (≤2000×2000, aspect preserved), and base64-encode an
/// image for the MCP image content block. Re-encodes as PNG when resized.
pub fn encode_image_for_content(path: &str, bytes: &[u8]) -> Result<(String, &'static str), String> {
    use image::ImageFormat;
    let format = image::guess_format(bytes)
        .map_err(|e| format!("failed to detect image {path}: {e}"))?;
    let img = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| format!("failed to decode image {path}: {e}"))?;
    let (data, out_mime) = if img.width() > IMAGE_MAX_DIM || img.height() > IMAGE_MAX_DIM {
        let resized =
            img.resize(IMAGE_MAX_DIM, IMAGE_MAX_DIM, image::imageops::FilterType::Lanczos3);
        let mut buffer = Vec::new();
        resized
            .write_to(&mut std::io::Cursor::new(&mut buffer), ImageFormat::Png)
            .map_err(|e| format!("failed to re-encode image {path}: {e}"))?;
        (buffer, "image/png")
    } else {
        let mime = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Bmp => "image/bmp",
            _ => "image/png",
        };
        (bytes.to_vec(), mime)
    };
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok((encoded, out_mime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::config;

    async fn write_file(dir: &std::path::Path, name: &str, content: &str) {
        tokio::fs::write(dir.join(name), content).await.unwrap();
    }

    async fn read_text(dir: &std::path::Path, args: &str) -> Result<String, String> {
        let params: crate::tools::ReadParams = serde_json::from_str(args).unwrap();
        let output = run(&config(dir), &params.path, params.offset, params.limit).await?;
        Ok(output.content)
    }

    #[tokio::test]
    async fn small_file_returns_everything() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "line1\nline2\nline3\n").await;
        let text = read_text(dir.path(), r#"{"path":"hello.txt"}"#).await.unwrap();
        assert_eq!(text, "line1\nline2\nline3\n");
    }

    #[tokio::test]
    async fn paginates_large_file_by_lines() {
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=5000).map(|i| format!("line {i}\n")).collect::<String>();
        write_file(dir.path(), "big.txt", &content).await;

        let first = read_text(dir.path(), r#"{"path":"big.txt"}"#).await.unwrap();
        assert!(first.starts_with("line 1\n"));
        assert!(first.contains("line 2000\n"));
        assert!(!first.contains("line 2001\n"));
        assert!(first.ends_with(
            "[Showing lines 1-2000 of 5000. Use offset=2001 to continue.]"
        ));

        let second =
            read_text(dir.path(), r#"{"path":"big.txt","offset":2001}"#).await.unwrap();
        assert!(second.starts_with("line 2001\n"));
        assert!(second.ends_with(
            "[Showing lines 2001-4000 of 5000. Use offset=4001 to continue.]"
        ));

        // Final page reaches EOF: no continuation note.
        let last = read_text(dir.path(), r#"{"path":"big.txt","offset":4001}"#).await.unwrap();
        assert!(last.starts_with("line 4001\n"));
        assert!(last.ends_with("line 5000\n"));
        assert!(!last.contains("Use offset="));
    }

    #[tokio::test]
    async fn offset_and_limit_window() {
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=5000).map(|i| format!("line {i}\n")).collect::<String>();
        write_file(dir.path(), "big.txt", &content).await;

        let args = r#"{"path":"big.txt","offset":100,"limit":50}"#;
        let page = read_text(dir.path(), args).await.unwrap();
        assert!(page.starts_with("line 100\n"));
        assert!(page.contains("line 149\n"));
        assert!(!page.contains("line 150\n"));
        assert!(page.ends_with("[4851 more lines in file. Use offset=150 to continue.]"));
    }

    #[tokio::test]
    async fn byte_cap_shows_kb_limit() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}\n", "a".repeat(300)); // 301 bytes per line
        let content = line.repeat(300); // ~90KB total
        write_file(dir.path(), "wide.txt", &content).await;

        let text = read_text(dir.path(), r#"{"path":"wide.txt"}"#).await.unwrap();
        assert!(text.contains("[Showing lines 1-"));
        assert!(text.contains("(50.0KB limit). Use offset="));
        assert!(text.contains("to continue.]"));
        assert!(text.len() < MAX_BYTES + 500);
    }

    #[tokio::test]
    async fn offset_beyond_eof_is_error() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "n.txt", "1\n2\n").await;
        let error = read_text(dir.path(), r#"{"path":"n.txt","offset":5000}"#)
            .await
            .unwrap_err();
        assert_eq!(error, "Offset 5000 is beyond end of file (2 lines total)");
    }

    #[tokio::test]
    async fn empty_file_notice() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "empty.txt", "").await;
        let text = read_text(dir.path(), r#"{"path":"empty.txt"}"#).await.unwrap();
        assert_eq!(text, "(file is empty)");
    }

    #[tokio::test]
    async fn unicode_content_preserved() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "uni.txt", "héllo → 世界 🦀\n").await;
        let text = read_text(dir.path(), r#"{"path":"uni.txt"}"#).await.unwrap();
        assert_eq!(text, "héllo → 世界 🦀\n");
    }

    #[tokio::test]
    async fn crlf_content_preserved() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "crlf.txt", "a\r\nb\r\n").await;
        let text = read_text(dir.path(), r#"{"path":"crlf.txt"}"#).await.unwrap();
        assert_eq!(text, "a\r\nb\r\n");
    }

    #[tokio::test]
    async fn giant_single_line_notice() {
        let dir = tempfile::tempdir().unwrap();
        let giant = format!("x{}\nb\n", "y".repeat(60 * 1024));
        write_file(dir.path(), "giant.txt", &giant).await;

        // Reading from the giant line yields the §7 notice, never half a line.
        let text = read_text(dir.path(), r#"{"path":"giant.txt","offset":1}"#)
            .await
            .unwrap();
        assert!(text.contains("[Line 1 is 60.0KB, exceeds 50.0KB limit."));
        assert!(text.contains("Use bash to inspect the line in chunks.]"));
        assert!(!text.contains("yyyy"));

        // Reading from line 2 works normally.
        let tail = read_text(dir.path(), r#"{"path":"giant.txt","offset":2}"#).await.unwrap();
        assert_eq!(tail, "b\n");
    }

    fn encode_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    async fn read_output(dir: &std::path::Path, name: &str) -> crate::tools::ReadOutput {
        run(&config(dir), name, None, None).await.unwrap()
    }

    #[tokio::test]
    async fn reads_png_image() {
        let dir = tempfile::tempdir().unwrap();
        let png = encode_png(10, 12);
        tokio::fs::write(dir.path().join("pic.png"), &png).await.unwrap();

        // Structured output marks the file as an image.
        let output = read_output(dir.path(), "pic.png").await;
        assert_eq!(output.image_mime_type.as_deref(), Some("image/png"));
        assert_eq!(output.content, "Read image file [image/png]");

        // The image encoder round-trips the pixels.
        let (encoded, mime) = encode_image_for_content("pic.png", &png).unwrap();
        assert_eq!(mime, "image/png");
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (10, 12));
    }

    #[tokio::test]
    async fn downscales_oversized_image() {
        let png = encode_png(3000, 1000);
        let (encoded, mime) = encode_image_for_content("big.png", &png).unwrap();
        assert_eq!(mime, "image/png"); // resized images re-encode as PNG
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert!(img.width() <= 2000 && img.height() <= 2000);
        // aspect ratio roughly preserved (3000:1000 -> 2000:667)
        assert_eq!(img.height(), 667);
    }

    #[tokio::test]
    async fn reads_jpeg_and_gif_images() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg_img = image::DynamicImage::new_rgb8(4, 4);
        let mut jpeg = Vec::new();
        jpeg_img
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        tokio::fs::write(dir.path().join("pic.jpg"), &jpeg).await.unwrap();
        let output = read_output(dir.path(), "pic.jpg").await;
        assert_eq!(output.image_mime_type.as_deref(), Some("image/jpeg"));

        let gif_img = image::DynamicImage::new_rgba8(4, 4);
        let mut gif = Vec::new();
        gif_img
            .write_to(&mut std::io::Cursor::new(&mut gif), image::ImageFormat::Gif)
            .unwrap();
        tokio::fs::write(dir.path().join("anim.gif"), &gif).await.unwrap();
        let output = read_output(dir.path(), "anim.gif").await;
        assert_eq!(output.image_mime_type.as_deref(), Some("image/gif"));
    }

    #[tokio::test]
    async fn binary_non_image_rejected() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("blob.bin"), [0u8, 1, 0, 2])
            .await
            .unwrap();
        let error = read_text(dir.path(), r#"{"path":"blob.bin"}"#)
            .await
            .unwrap_err();
        assert!(error.contains("binary"));
    }

    #[tokio::test]
    async fn relative_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("a")).await.unwrap();
        write_file(dir.path(), "a/b.txt", "content\n").await;
        let config = config(dir.path());
        let via_relative = run(&config, "a/b.txt", None, None).await.unwrap();
        let absolute = dir.path().canonicalize().unwrap().join("a/b.txt");
        let via_absolute =
            run(&config, absolute.to_str().unwrap(), None, None).await.unwrap();
        assert_eq!(
            serde_json::to_value(&via_relative.content).unwrap(),
            serde_json::to_value(&via_absolute.content).unwrap()
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal_and_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        for path in [
            "../../etc/passwd",
            "/etc/passwd",
            "nested/../../escape.txt",
        ] {
            assert!(
                run(&config, path, None, None).await.unwrap_err().contains("escapes the workspace"),
                "{path} should be rejected"
            );
        }

        let root = dir.path().canonicalize().unwrap();
        let sibling = root.parent().unwrap().join(format!(
            "{}-evil",
            root.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(run(&config, sibling.to_str().unwrap(), None, None).await.is_err());
        let _ = std::fs::remove_dir_all(&sibling);

        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        assert!(run(&config, "escape/passwd", None, None).await.is_err());
        std::fs::remove_file(root.join("escape")).unwrap();
    }

    #[tokio::test]
    async fn file_not_found_message() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_text(dir.path(), r#"{"path":"missing.txt"}"#)
            .await
            .unwrap_err();
        assert_eq!(error, "File not found: missing.txt");
    }

    #[tokio::test]
    async fn zero_offset_and_limit_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "n.txt", "1\n").await;
        assert!(read_text(dir.path(), r#"{"path":"n.txt","offset":0}"#)
            .await
            .unwrap_err()
            .contains("Invalid offset"));
        assert!(read_text(dir.path(), r#"{"path":"n.txt","limit":0}"#)
            .await
            .unwrap_err()
            .contains("Invalid limit"));
    }
}
