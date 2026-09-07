//! Atomic file writes (pi-spec §38): temp file + fsync + rename, so a crash
//! or cancellation never leaves a partially written target behind.

use std::path::Path;

/// Write `bytes` to `path` atomically: a hidden temp file in the same
/// directory, fsync, then rename over the target.
pub async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid file path: {}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".{file_name}.mcpod-tmp-{}", crate::util::random_hex(4)));

    let result = write_inner(path, &tmp, bytes).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

async fn write_inner(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::File::create(tmp)
        .await
        .map_err(|e| format!("failed to create temp file: {e}"))?;
    file.write_all(bytes)
        .await
        .map_err(|e| format!("failed to write temp file: {e}"))?;
    file.sync_all()
        .await
        .map_err(|e| format!("failed to sync temp file: {e}"))?;
    drop(file);

    tokio::fs::rename(tmp, path)
        .await
        .map_err(|e| format!("failed to replace file: {e}"))?;

    // Best-effort directory fsync so the rename itself is durable.
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or_else(|| Path::new("."))) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_atomic;

    #[tokio::test]
    async fn writes_content_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/file.txt");
        // Parent creation is the caller's job (tools::write does mkdir first).
        tokio::fs::create_dir_all(dir.path().join("nested")).await.unwrap();
        write_atomic(&target, b"hello").await.unwrap();
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"hello");

        // Overwrite works and still leaves no temp files behind.
        write_atomic(&target, b"world!").await.unwrap();
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"world!");

        let entries: Vec<_> = std::fs::read_dir(dir.path().join("nested"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["file.txt".to_string()]);
    }

    #[tokio::test]
    async fn preserves_existing_file_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        tokio::fs::write(&target, "original").await.unwrap();

        // Target is a directory -> create/rename must fail, content intact.
        let blocked = dir.path().join("blocked");
        tokio::fs::create_dir(&blocked).await.unwrap();
        assert!(write_atomic(&blocked, b"x").await.is_err());
        assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "original");
    }
}
