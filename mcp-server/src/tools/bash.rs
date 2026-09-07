//! `bash` tool (pi-spec §25–§35): `/bin/bash -c` in the workspace, no
//! default timeout, stdout/stderr merged in arrival order, tail truncation
//! with the full output saved to /tmp, process-tree kill on timeout or
//! cancellation.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::tools::BashOutput;

use crate::output::truncate::{self, MAX_BYTES, MAX_LINES};
use crate::process::process_group;
use crate::tools::append_note;
use crate::util::random_hex;

/// Validate the optional timeout (§25): finite and > 0, or absent.
pub fn validate_timeout(timeout: Option<f64>) -> Result<Option<Duration>, String> {
    match timeout {
        None => Ok(None),
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Ok(Some(Duration::from_secs_f64(seconds)))
        }
        Some(_) => Err("Invalid timeout: must be a finite number of seconds".to_string()),
    }
}

struct Chunk {
    seq: u64,
    stderr: bool,
    bytes: Vec<u8>,
}

static CHUNK_SEQ: AtomicU64 = AtomicU64::new(0);

pub async fn run(
    workspace: &Path,
    command: &str,
    timeout: Option<Duration>,
) -> Result<BashOutput, String> {
    let mut child = process_group::spawn_bash(command, workspace)
        .map_err(|e| format!("failed to spawn /bin/bash: {e}"))?;
    let (stdout_pipe, stderr_pipe) = child.take_pipes();
    let chunks: Arc<Mutex<Vec<Chunk>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_task = tokio::spawn(read_stream(stdout_pipe, chunks.clone(), false));
    let stderr_task = tokio::spawn(read_stream(stderr_pipe, chunks.clone(), true));
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();

    // No default timeout (§25): without one, the command may run indefinitely.
    let mut timed_out = false;
    let exit_code = match timeout {
        Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
            Ok(status) => process_group::exit_code_of(
                &status.map_err(|e| format!("failed to wait for bash: {e}"))?,
            ),
            Err(_) => {
                timed_out = true;
                // Kill the entire process tree (§33), then reap the child.
                let status = child
                    .kill_group()
                    .await
                    .map_err(|e| format!("failed to kill process group: {e}"))?;
                let _ = status;
                124 // GNU-timeout convention
            }
        },
        None => process_group::exit_code_of(
            &child
                .wait()
                .await
                .map_err(|e| format!("failed to wait for bash: {e}"))?,
        ),
    };

    // Drain the readers; abort if a daemonized descendant keeps a pipe open.
    {
        let drain = async move {
            let _ = stdout_task.await;
            let _ = stderr_task.await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
    }
    stdout_abort.abort();
    stderr_abort.abort();

    // Merge output in arrival order (§27).
    let mut all = std::mem::take(&mut *chunks.lock().expect("chunk list poisoned"));
    all.sort_by_key(|chunk| chunk.seq);
    let combined_bytes: Vec<u8> = all.iter().flat_map(|c| c.bytes.iter().copied()).collect();
    let stdout_bytes: Vec<u8> = all
        .iter()
        .filter(|c| !c.stderr)
        .flat_map(|c| c.bytes.iter().copied())
        .collect();
    let stderr_bytes: Vec<u8> = all
        .iter()
        .filter(|c| c.stderr)
        .flat_map(|c| c.bytes.iter().copied())
        .collect();
    let combined = String::from_utf8_lossy(&combined_bytes).into_owned();

    // Tail truncation (§30): keep the last 2000 lines / 50 KiB and, when
    // anything was cut, save the full output for follow-up bash calls (§31).
    let total_lines = truncate::count_lines(&combined);
    let tail = truncate::take_tail(&combined, MAX_LINES, MAX_BYTES);
    let log_path: Option<String> = if tail.truncated {
        let path = format!("/tmp/mcpod-bash-{}.log", random_hex(4));
        tokio::fs::write(&path, &combined_bytes)
            .await
            .map_err(|e| format!("failed to save full output to {path}: {e}"))?;
        Some(path)
    } else {
        None
    };

    let mut text = tail.text;
    if tail.truncated {
        let limit_note = if tail.byte_limited {
            format!(" ({} limit)", truncate::format_kb(MAX_BYTES))
        } else {
            String::new()
        };
        let note = match &log_path {
            Some(path) => format!(
                "[Showing lines {}-{} of {}{limit_note}. Full output: {path}]",
                tail.first_line, total_lines, total_lines
            ),
            None => format!(
                "[Showing lines {}-{} of {}{limit_note}.]",
                tail.first_line, total_lines, total_lines
            ),
        };
        append_note(&mut text, note);
    }
    if timed_out {
        let seconds = timeout.map(|d| d.as_secs_f64()).unwrap_or_default();
        append_note(
            &mut text,
            format!("Command timed out after {} seconds", format_seconds(seconds)),
        );
    } else if exit_code != 0 {
        append_note(&mut text, format!("Command exited with code {exit_code}"));
    }
    if text.is_empty() {
        // Success with no output at all (§28).
        text = "(no output)".to_string();
    }

    let stdout_shown = stream_view(&String::from_utf8_lossy(&stdout_bytes));
    let stderr_shown = stream_view(&String::from_utf8_lossy(&stderr_bytes));

    Ok(BashOutput {
        exit_code,
        output: text,
        stdout: stdout_shown,
        stderr: stderr_shown,
        full_output_log: log_path,
        timed_out: timed_out.then_some(true),
    })
}

fn stream_view(full: &str) -> String {
    let tail = truncate::take_tail(full, MAX_LINES, MAX_BYTES);
    if tail.truncated {
        let mut text = tail.text;
        text.push_str("\n[...truncated]");
        text
    } else {
        tail.text
    }
}

pub fn format_seconds(seconds: f64) -> String {
    if seconds.fract() == 0.0 {
        format!("{}", seconds as u64)
    } else {
        trim_float(seconds)
    }
}

fn trim_float(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

async fn read_stream<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    chunks: Arc<Mutex<Vec<Chunk>>>,
    stderr: bool,
) {
    let Some(mut pipe) = pipe else { return };
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let seq = CHUNK_SEQ.fetch_add(1, Ordering::Relaxed);
                chunks
                    .lock()
                    .expect("chunk list poisoned")
                    .push(Chunk { seq, stderr, bytes: buffer[..n].to_vec() });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> std::path::PathBuf {
        // `keep()` prevents the TempDir destructor from deleting the
        // directory: the spawned bash needs the cwd to stay alive. Resolve
        // symlinks (macOS /var -> /private/var) so `pwd` matches.
        tempfile::tempdir().unwrap().keep().canonicalize().unwrap()
    }

    fn text_of(result: &BashOutput) -> String {
        result.output.clone()
    }

    fn structured(result: &BashOutput) -> serde_json::Value {
        serde_json::to_value(result).unwrap()
    }

    #[test]
    fn timeout_validation() {
        assert!(validate_timeout(None).unwrap().is_none());
        assert_eq!(validate_timeout(Some(30.0)).unwrap(), Some(Duration::from_secs(30)));
        assert_eq!(
            validate_timeout(Some(1.5)).unwrap(),
            Some(Duration::from_millis(1500))
        );
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                validate_timeout(Some(bad)).unwrap_err(),
                "Invalid timeout: must be a finite number of seconds"
            );
        }
    }

    #[tokio::test]
    async fn echo_hello_exit_code_stdout_stderr() {
        let result = run(&ws(), "echo hello; echo oops >&2; exit 3", Some(Duration::from_secs(30)))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 3);
        let structured = structured(&result);
        assert_eq!(structured["exit_code"], 3);
        assert_eq!(structured["stdout"].as_str().unwrap().trim(), "hello");
        assert_eq!(structured["stderr"].as_str().unwrap().trim(), "oops");
        let text = text_of(&result);
        assert!(text.contains("hello"));
        assert!(text.contains("oops"));
        assert!(text.contains("Command exited with code 3"));
    }

    #[tokio::test]
    async fn pwd_is_workspace() {
        let workspace = ws();
        let result = run(&workspace, "pwd", Some(Duration::from_secs(30))).await.unwrap();
        let structured = structured(&result);
        assert_eq!(
            structured["stdout"].as_str().unwrap().trim(),
            workspace.display().to_string()
        );
    }

    #[tokio::test]
    async fn no_output_success() {
        let result = run(&ws(), "true", Some(Duration::from_secs(10))).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(text_of(&result), "(no output)");
    }

    #[tokio::test]
    async fn no_output_failure_keeps_message() {
        let result = run(&ws(), "exit 7", Some(Duration::from_secs(10))).await.unwrap();
        assert_eq!(text_of(&result), "Command exited with code 7");
        assert_eq!(result.exit_code, 7);
    }

    #[tokio::test]
    async fn combined_output_in_arrival_order() {
        let result = run(
            &ws(),
            "echo out1; echo err1 >&2; echo out2; echo err2 >&2",
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();
        let text = text_of(&result);
        let pos = |needle: &str| text.find(needle).expect(needle);
        assert!(pos("out1") < pos("out2"));
        assert!(pos("err1") < pos("err2"));
        assert!(text.contains("out1") && text.contains("err2"));
    }

    #[tokio::test]
    async fn line_tail_truncation_saves_full_output() {
        let result = run(&ws(), "seq 1 30000", Some(Duration::from_secs(120))).await.unwrap();
        let text = text_of(&result);
        assert!(text.contains("[Showing lines 28001-30000 of 30000. Full output: /tmp/mcpod-bash-"));
        assert!(text.contains(".log]"));
        assert!(text.contains("30000\n"));
        assert!(!text.contains("\n1\n2\n3\n"));

        let log_path = text
            .split("Full output: ")
            .nth(1)
            .and_then(|rest| rest.trim_end_matches(']').split_whitespace().next())
            .unwrap()
            .to_string();
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(log.lines().count(), 30000);
        std::fs::remove_file(&log_path).unwrap();
    }

    #[tokio::test]
    async fn byte_tail_truncation_on_giant_last_line() {
        // One 60000-byte line of 'é' (2 bytes each), no trailing newline.
        let result = run(&ws(), "printf 'é%.0s' {1..30000}", Some(Duration::from_secs(120)))
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("[Showing lines 1-1 of 1 (50.0KB limit). Full output: /tmp/mcpod-bash-"));
        let kept = text.chars().filter(|c| *c == 'é').count();
        assert!((25590..=25610).contains(&kept), "kept {kept} é chars");

        let log_path = text
            .split("Full output: ")
            .nth(1)
            .and_then(|rest| rest.trim_end_matches(']').split_whitespace().next())
            .unwrap()
            .to_string();
        assert_eq!(std::fs::read(&log_path).unwrap().len(), 60000);
        std::fs::remove_file(&log_path).unwrap();
    }

    #[tokio::test]
    async fn timeout_kills_whole_process_tree() {
        let started = std::time::Instant::now();
        let result = run(
            &ws(),
            "sleep 291 & bash -c 'sleep 292 & wait' & sleep 293 & wait",
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert!(started.elapsed().as_secs() < 30);
        assert!(result.timed_out == Some(true));
        let text = text_of(&result);
        assert!(text.contains("Command timed out after 1 seconds"));
        assert_eq!(structured(&result)["timed_out"], serde_json::json!(true));
        assert_eq!(structured(&result)["exit_code"], 124);

        // The whole tree — children and grandchildren — must be gone (§33).
        tokio::time::sleep(Duration::from_millis(300)).await;
        let check = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("sleep 29[0-9]")
            .output()
            .unwrap();
        assert!(
            !check.status.success(),
            "child processes still alive: {}",
            String::from_utf8_lossy(&check.stdout)
        );
    }

    #[tokio::test]
    async fn timeout_preserves_partial_output() {
        let result = run(
            &ws(),
            "echo starting; echo test1; sleep 60; echo done",
            Some(Duration::from_secs(2)),
        )
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(text.contains("starting"));
        assert!(text.contains("test1"));
        assert!(text.contains("Command timed out after 2 seconds"));
        assert!(!text.contains("done"));
    }

    #[tokio::test]
    async fn fractional_timeout_message() {
        let result = run(&ws(), "sleep 30", Some(Duration::from_millis(500)))
            .await
            .unwrap();
        assert!(text_of(&result).contains("Command timed out after 0.5 seconds"));
    }

    #[tokio::test]
    async fn aborted_request_kills_process_tree() {
        let workspace = ws();
        let handle =
            tokio::spawn(async move { run(&workspace, "sleep 300; echo never", None).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!handle.is_finished());
        handle.abort(); // request cancelled (§35): future dropped, guard fires
        tokio::time::sleep(Duration::from_millis(400)).await;

        let check = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("sleep 300")
            .output()
            .unwrap();
        assert!(
            !check.status.success(),
            "process should have been killed on cancellation"
        );
    }

    #[tokio::test]
    async fn no_timeout_runs_to_completion() {
        let result = run(&ws(), "echo done", None).await.unwrap();
        assert_eq!(structured(&result)["exit_code"], 0);
        assert!(text_of(&result).contains("done"));
    }

    #[tokio::test]
    async fn child_processes_complete_normally() {
        let result = run(
            &ws(),
            "bash -c 'echo grandchild' & echo child; wait",
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(text.contains("grandchild"));
        assert!(text.contains("child"));
    }

    #[tokio::test]
    async fn git_and_mise_versions() {
        let result = run(&ws(), "git --version", Some(Duration::from_secs(30)))
            .await
            .unwrap();
        assert_eq!(structured(&result)["exit_code"], 0);
        // mise exists on the dev host via the user's toolchain, and in the
        // container by design; tolerate absence on bare CI hosts.
        let result = run(&ws(), "mise --version", Some(Duration::from_secs(30))).await;
        match result {
            Ok(result) => assert_eq!(structured(&result)["exit_code"], 0),
            Err(message) => assert!(message.contains("failed to spawn"), "{message}"),
        }
    }
}
