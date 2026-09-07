//! `resource://environment` (§19, §20): dynamic container environment info.
//!
//! Mounts are discovered from `/proc/self/mountinfo` at read time instead of
//! hard-coding a compose layout. Only user-relevant mounts are reported and no
//! host-side source paths leak (§20).

use std::sync::OnceLock;

use serde::Serialize;

use crate::config::Config;

pub const RESOURCE_URI: &str = "resource://environment";

/// The `resources/list` entry.
pub fn resource() -> rmcp::model::Resource {
    rmcp::model::Resource::new(RESOURCE_URI, "environment")
        .with_description(
            "Dynamic environment information about this MCPod container: OS, architecture, \
             workspace, mounts, and runtime manager.",
        )
        .with_mime_type("application/json")
}

#[derive(Debug, Serialize)]
pub struct EnvironmentInfo {
    pub os: String,
    pub architecture: String,
    pub workspace: String,
    pub mounts: Vec<MountInfo>,
    pub runtime_manager: RuntimeManager,
}

#[derive(Debug, Serialize)]
pub struct MountInfo {
    pub path: String,
    pub mode: String,
    pub persistent: bool,
}

#[derive(Debug, Serialize)]
pub struct RuntimeManager {
    pub name: &'static str,
    pub installed: bool,
}

pub async fn info(config: &Config) -> EnvironmentInfo {
    EnvironmentInfo {
        os: os_name(),
        architecture: std::env::consts::ARCH.to_string(),
        workspace: config.workspace.display().to_string(),
        mounts: mounts(),
        runtime_manager: RuntimeManager { name: "mise", installed: mise_installed().await },
    }
}

/// `PRETTY_NAME` from /etc/os-release, e.g. "Debian GNU/Linux 13 (trixie)".
fn os_name() -> String {
    if let Ok(release) = std::fs::read_to_string("/etc/os-release") {
        for line in release.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                let value = value.trim();
                return value.trim_matches('"').trim_matches('\'').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

static MISE_INSTALLED: OnceLock<bool> = OnceLock::new();

async fn mise_installed() -> bool {
    if let Some(cached) = MISE_INSTALLED.get() {
        return *cached;
    }
    let installed = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::process::Command::new("mise").arg("--version").output(),
    )
    .await
    {
        Ok(Ok(output)) => output.status.success(),
        _ => false,
    };
    let _ = MISE_INSTALLED.set(installed);
    installed
}

/// Mounts the agent should know about: everything Docker layered on top of
/// the image (bind mounts, volumes, user tmpfs), excluding kernel/system
/// mounts and the image root itself.
fn mounts() -> Vec<MountInfo> {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new(); // not Linux (e.g. host-side dev/test runs)
    };

    let mut by_path = std::collections::BTreeMap::new();
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // id parent dev root mount_point mount_options [optional...] - fstype source super_options
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if fields.len() < separator + 2 || fields.len() < 6 {
            continue;
        }
        let mount_point = unescape_mount_point(fields[4]);
        let options = fields[5];
        let fstype = fields[separator + 1];
        if is_system_mount(&mount_point) {
            continue;
        }
        let mode = if options.split(',').any(|option| option == "ro") {
            "ro"
        } else {
            "rw"
        };
        // The root mount is the container image itself: writable during a
        // session but not persistent. tmpfs lives in memory only.
        let persistent = mount_point != "/" && fstype != "tmpfs";
        by_path.insert(
            mount_point.clone(),
            MountInfo { path: mount_point, mode: mode.to_string(), persistent },
        );
    }
    by_path.into_values().collect()
}

fn is_system_mount(mount_point: &str) -> bool {
    const SYSTEM_EXACT: [&str; 7] = [
        "/",
        "/etc/hosts",
        "/etc/hostname",
        "/etc/resolv.conf",
        "/etc/localtime",
        "/etc/machine-id",
        "/etc/platform-id",
    ];
    const SYSTEM_PREFIXES: [&str; 4] = ["/proc", "/sys", "/dev", "/run"];
    SYSTEM_EXACT.contains(&mount_point)
        || SYSTEM_PREFIXES
            .iter()
            .any(|prefix| mount_point == *prefix || mount_point.starts_with(&format!("{prefix}/")))
}

/// mountinfo escapes space, tab, newline, and backslash as \040 \011 \012 \134.
fn unescape_mount_point(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(position) = rest.find('\\') {
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        let replacement = match tail.get(..4) {
            Some("\\040") => Some(' '),
            Some("\\011") => Some('\t'),
            Some("\\012") => Some('\n'),
            Some("\\134") => Some('\\'),
            _ => None,
        };
        match replacement {
            Some(character) => {
                out.push(character);
                rest = &tail[4..];
            }
            None => {
                out.push('\\');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_handles_octal_sequences() {
        assert_eq!(unescape_mount_point("/data"), "/data");
        assert_eq!(unescape_mount_point("/my\\040data"), "/my data");
        assert_eq!(unescape_mount_point("a\\134b"), "a\\b");
        assert_eq!(unescape_mount_point("trailing\\"), "trailing\\");
    }

    #[test]
    fn system_mounts_are_filtered() {
        assert!(is_system_mount("/"));
        assert!(is_system_mount("/proc"));
        assert!(is_system_mount("/proc/cpuinfo"));
        assert!(is_system_mount("/dev/shm"));
        assert!(is_system_mount("/etc/hosts"));
        assert!(is_system_mount("/run/secrets"));
        assert!(!is_system_mount("/workspace"));
        assert!(!is_system_mount("/data"));
    }
}
