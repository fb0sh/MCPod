//! Process-group execution for bash (pi-spec §26, §33, §35).
//!
//! Every command runs in its own process group (`process_group(0)` + setpgid
//! semantics), so a timeout or a cancelled request can kill the entire tree —
//! bash, npm, node workers, test runners — with one `killpg`. A drop guard
//! disarms only after the child is reaped, which makes dropping the tool
//! future (request cancellation) take the whole tree down too.

use std::path::Path;
use std::process::Stdio;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

pub struct GroupChild {
    child: Child,
    pid: Option<u32>,
    defused: bool,
}

/// Spawn `/bin/bash -c <command>` in its own process group with `cwd`.
pub fn spawn_bash(command: &str, cwd: &Path) -> std::io::Result<GroupChild> {
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // New process group: the child leads it, so killpg(child) reaches
        // every descendant that did not escape into its own session.
        cmd.process_group(0);
    }
    let child = cmd.spawn()?;
    let pid = child.id();
    Ok(GroupChild { child, pid, defused: false })
}

impl GroupChild {
    /// Take the piped stdout/stderr handles (once).
    pub fn take_pipes(&mut self) -> (Option<ChildStdout>, Option<ChildStderr>) {
        (self.child.stdout.take(), self.child.stderr.take())
    }

    /// Wait for the direct child to exit; disarms the kill guard once reaped.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        self.defused = true;
        Ok(status)
    }

    /// SIGKILL the whole process group, then reap the direct child.
    pub async fn kill_group(&mut self) -> std::io::Result<std::process::ExitStatus> {
        kill_group_raw(self.pid);
        let status = self.child.wait().await?;
        self.defused = true;
        Ok(status)
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        // Request cancelled / future dropped before completion: take the
        // whole tree with us (pi-spec §35).
        if !self.defused {
            kill_group_raw(self.pid);
        }
    }
}

fn kill_group_raw(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // The child is its own group leader; killpg reaches every descendant.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Exit code, mapping signal deaths to the conventional 128+signal.
pub fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> std::path::PathBuf {
        std::env::temp_dir().join("mcpod-pg-ws")
    }

    #[tokio::test]
    async fn waits_and_disarms() {
        std::fs::create_dir_all(ws()).unwrap();
        let mut child = spawn_bash("echo hi", &ws()).unwrap();
        // Keep the pipe handles alive: dropping them closes the read end and
        // the child's `echo` would die to SIGPIPE.
        let _pipes = child.take_pipes();
        let status = child.wait().await.unwrap();
        assert!(status.success());
        assert!(child.defused);
    }

    #[tokio::test]
    async fn kill_group_reaps_child() {
        std::fs::create_dir_all(ws()).unwrap();
        let mut child = spawn_bash("sleep 60", &ws()).unwrap();
        let _ = child.take_pipes();
        let status = child.kill_group().await.unwrap();
        assert!(!status.success());
        assert!(child.defused);
    }

    #[tokio::test]
    async fn dropping_without_wait_kills_group() {
        std::fs::create_dir_all(ws()).unwrap();
        let mut child = spawn_bash("sleep 60", &ws()).unwrap();
        let _ = child.take_pipes();
        let pid = child.pid;
        drop(child);
        // The group must be gone shortly.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let alive = unsafe { libc::killpg(pid.unwrap() as libc::pid_t, 0) } == 0;
        assert!(!alive, "process group should have been killed on drop");
    }
}
