//! `write` tool (pi-spec §10–§12): create or fully replace a file, creating
//! parent directories, through the per-file mutation queue and an atomic
//! temp+rename commit.

use crate::config::Config;
use crate::fs::atomic_write::write_atomic;
use crate::fs::mutation_queue;
use crate::fs::path::resolve_in_workspace;
use crate::tools::{text_result, WriteParams};
use rmcp::model::CallToolResult;

pub async fn run(config: &Config, params: &WriteParams) -> Result<CallToolResult, String> {
    let resolved = resolve_in_workspace(&config.workspace, &params.path)?;
    // Whole mutation transaction under the per-file lock (§24): nothing can
    // interleave between resolve and the atomic rename.
    let _guard = mutation_queue::global().lock(&resolved).await;

    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create parent directory for {}: {e}", params.path))?;
    }
    write_atomic(&resolved, params.content.as_bytes())
        .await
        .map_err(|e| format!("failed to write {}: {e}", params.path))?;

    Ok(text_result(format!(
        "Successfully wrote {} bytes to {}",
        params.content.len(),
        params.path
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::config;
    use crate::tools::WriteParams;

    fn params(path: &str, content: &str) -> WriteParams {
        WriteParams { path: path.to_string(), content: content.to_string() }
    }

    async fn read_all(dir: &std::path::Path, name: &str) -> String {
        tokio::fs::read_to_string(dir.join(name)).await.unwrap()
    }

    #[tokio::test]
    async fn creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let result = run(&config, &params("src/deep/nested/new.txt", "first"))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&result.content).unwrap()[0]["text"],
            "Successfully wrote 5 bytes to src/deep/nested/new.txt"
        );
        assert_eq!(read_all(dir.path(), "src/deep/nested/new.txt").await, "first");

        run(&config, &params("src/deep/nested/new.txt", "second edition"))
            .await
            .unwrap();
        assert_eq!(
            read_all(dir.path(), "src/deep/nested/new.txt").await,
            "second edition"
        );
    }

    #[tokio::test]
    async fn empty_and_unicode_content() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        run(&config, &params("empty.txt", "")).await.unwrap();
        assert_eq!(read_all(dir.path(), "empty.txt").await, "");

        run(&config, &params("uni.txt", "héllo → 世界 🦀")).await.unwrap();
        assert_eq!(read_all(dir.path(), "uni.txt").await, "héllo → 世界 🦀");
    }

    #[tokio::test]
    async fn rejects_traversal_and_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        assert!(run(&config, &params("../evil.txt", "x")).await.is_err());
        assert!(run(&config, &params("/etc/passwd", "x")).await.is_err());

        let root = dir.path().canonicalize().unwrap();
        let link = root.join("dir-link");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        assert!(
            run(&config, &params("dir-link/newfile", "x"))
                .await
                .unwrap_err()
                .contains("escapes the workspace")
        );
        assert!(!std::path::Path::new("/etc/mcpod-escape-marker").exists());
        std::fs::remove_file(&link).unwrap();
    }

    #[tokio::test]
    async fn concurrent_writes_same_file_stay_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut tasks = Vec::new();
        for i in 0..10 {
            let config = config.clone();
            tasks.push(tokio::spawn(async move {
                run(
                    &config,
                    &params("shared.txt", &format!("writer-{i} payload {}", "x".repeat(1000))),
                )
                .await
                .unwrap();
                i
            }));
        }
        let finished: Vec<i32> = futures_all(tasks).await;
        assert_eq!(finished.len(), 10);

        let final_content = read_all(dir.path(), "shared.txt").await;
        assert!(
            (0..10).any(|i| final_content == format!("writer-{i} payload {}", "x".repeat(1000))),
            "final content must be exactly one writer's full output"
        );
    }

    #[tokio::test]
    async fn concurrent_writes_different_files_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut tasks = Vec::new();
        for i in 0..8 {
            let config = config.clone();
            tasks.push(tokio::spawn(async move {
                run(&config, &params(&format!("file-{i}.txt"), &format!("content-{i}")))
                    .await
                    .unwrap()
            }));
        }
        for (i, task) in tasks.into_iter().enumerate() {
            task.await.unwrap();
            assert_eq!(
                read_all(dir.path(), &format!("file-{i}.txt")).await,
                format!("content-{i}")
            );
        }
    }

    async fn futures_all(tasks: Vec<tokio::task::JoinHandle<i32>>) -> Vec<i32> {
        let mut out = Vec::new();
        for task in tasks {
            out.push(task.await.unwrap());
        }
        out
    }
}
