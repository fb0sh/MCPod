//! Per-file mutation queue (pi-spec §24).
//!
//! `write` and `edit` serialize on a per-canonical-path async mutex so
//! concurrent tool calls on the same file cannot lose updates or interleave.
//! Different files proceed in parallel. The lock covers the whole mutation
//! transaction: read current content → validate → modify → write final.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Default)]
pub struct MutationQueue {
    locks: Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
}

static QUEUE: OnceLock<MutationQueue> = OnceLock::new();

pub fn global() -> &'static MutationQueue {
    QUEUE.get_or_init(MutationQueue::default)
}

impl MutationQueue {
    /// Acquire the mutation lock for one canonical file path.
    pub async fn lock(&self, path: &Path) -> OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.locks.lock().expect("mutation queue lock poisoned");
            map.entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::MutationQueue;
    use std::path::PathBuf;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mcpod-mq-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn same_file_serializes_different_files_parallel() {
        // One shared queue: per-file locks live in its map.
        let queue = std::sync::Arc::new(MutationQueue::default());
        let a = unique_path("a");
        let b = unique_path("b");

        let guard = queue.lock(&a).await;
        let queue_a = queue.clone();
        let same_file = tokio::spawn(async move {
            let _held = queue_a.lock(&a).await;
            true
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !same_file.is_finished(),
            "second lock on the same file must wait"
        );
        drop(guard);
        assert!(same_file.await.unwrap(), "lock released after guard drops");

        // A different file is not blocked by `a`'s lock being held elsewhere.
        let queue_b = queue.clone();
        let other_file = tokio::spawn(async move {
            let _held = queue_b.lock(&b).await;
            true
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), other_file)
                .await
                .is_ok(),
            "different files must not serialize"
        );
    }
}
