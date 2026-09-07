//! Environment-variable configuration (§31, transport-spec §16/§21/§29).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Bearer token required on all MCP transport endpoints. `None` disables
    /// authentication entirely (logged loudly at startup).
    pub token: Option<String>,
    /// Bind address. Defaults to loopback (§37); the Docker image overrides
    /// it to 0.0.0.0 because host-side localhost binding is provided by the
    /// compose port mapping `127.0.0.1:3000:3000` (§23).
    pub host: IpAddr,
    pub port: u16,
    /// Canonicalized workspace root; the file tools are jailed inside it (§17).
    pub workspace: PathBuf,
    /// Hosts accepted in the MCP endpoint's `Host` header. Empty keeps the
    /// transport default (loopback only), which guards against DNS rebinding.
    pub allowed_hosts: Vec<String>,
    /// Exact `Origin` values allowed on MCP endpoints; empty enables the
    /// localhost default (any port on localhost / 127.0.0.1 / [::1]).
    pub allowed_origins: Vec<String>,
    /// How long a disconnected legacy SSE session lingers before removal
    /// (transport-spec §16). Default 30m.
    pub sse_session_ttl: Duration,
    /// Interval between legacy SSE `: keepalive` comments (§29). Default 15s.
    pub sse_keepalive: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Empty or unset token: authentication disabled (with a loud warning
        // printed by main).
        let token = std::env::var("MCPOD_TOKEN")
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());

        let host: IpAddr = std::env::var("MCPOD_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string())
            .parse()
            .context("invalid MCPOD_HOST")?;
        let port: u16 = match std::env::var("MCPOD_PORT") {
            Ok(value) => value.parse().context("invalid MCPOD_PORT")?,
            Err(_) => 3000,
        };
        let workspace = std::env::var("MCPOD_WORKSPACE").unwrap_or_else(|_| "/workspace".into());
        let workspace =
            canonicalize_root(Path::new(&workspace)).context("invalid MCPOD_WORKSPACE")?;

        let allowed_hosts = csv_env("MCPOD_ALLOWED_HOSTS");
        let allowed_origins = csv_env("MCPOD_ALLOWED_ORIGINS");
        let sse_session_ttl = duration_env("MCPOD_SSE_SESSION_TTL", 30 * 60)?;
        let sse_keepalive = duration_env("MCPOD_SSE_KEEPALIVE", 15)?;

        Ok(Self {
            token,
            host,
            port,
            workspace,
            allowed_hosts,
            allowed_origins,
            sse_session_ttl,
            sse_keepalive,
        })
    }

    /// True when `origin` passes the Origin allowlist (§21). Empty allowlist
    /// defaults to loopback hosts with any port, http or https.
    pub fn origin_allowed(&self, origin: &str) -> bool {
        if !self.allowed_origins.is_empty() {
            return self.allowed_origins.iter().any(|allowed| allowed == origin);
        }
        let (scheme, rest) = match origin.split_once("://") {
            Some(parts) => parts,
            None => return false,
        };
        if scheme != "http" && scheme != "https" {
            return false;
        }
        // Strip the port (and brackets around IPv6 literals) before matching
        // well-known loopback hostnames.
        let host = rest.rsplit_once(':').map_or(rest, |(h, _)| h);
        let host = host.trim_start_matches('[').trim_end_matches(']');
        matches!(host, "localhost" | "127.0.0.1" | "::1")
    }
}

fn csv_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `Ns` / `Nm` / `Nh` (or bare seconds) into a Duration.
fn duration_env(name: &str, default_secs: u64) -> Result<Duration> {
    let raw = match std::env::var(name) {
        Ok(value) => value.trim().to_string(),
        Err(_) => return Ok(Duration::from_secs(default_secs)),
    };
    if raw.is_empty() {
        return Ok(Duration::from_secs(default_secs));
    }
    let (number, unit) = raw.split_at(raw.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(raw.len()));
    let value: f64 = number
        .trim()
        .parse()
        .with_context(|| format!("invalid {name}: {raw}"))?;
    if !value.is_finite() || value <= 0.0 {
        anyhow::bail!("invalid {name}: {raw} (must be positive)");
    }
    let secs = match unit.trim() {
        "" | "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        other => anyhow::bail!("invalid {name} unit: {other} (use s, m, or h)"),
    };
    Ok(Duration::from_secs_f64(secs))
}

/// Create the workspace if missing, then canonicalize it once so every later
/// sandbox check compares against a fully resolved absolute path (§17).
fn canonicalize_root(path: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create workspace directory {}", path.display()))?;
    path.canonicalize()
        .with_context(|| format!("failed to resolve workspace directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_default_allows_loopback_any_port() {
        let config = Config {
            token: None,
            host: "127.0.0.1".parse().unwrap(),
            port: 3000,
            workspace: PathBuf::from("/workspace"),
            allowed_hosts: vec![],
            allowed_origins: vec![],
            sse_session_ttl: Duration::from_secs(1800),
            sse_keepalive: Duration::from_secs(15),
        };
        for origin in [
            "http://localhost",
            "http://localhost:3000",
            "http://localhost:5173",
            "https://localhost:8443",
            "http://127.0.0.1",
            "http://127.0.0.1:8080",
            "http://[::1]:3000",
        ] {
            assert!(config.origin_allowed(origin), "{origin} should be allowed");
        }
        for origin in [
            "http://evil.example.com",
            "https://attacker.io:3000",
            "http://192.168.1.5:3000",
            "chrome-extension://abc",
            "null",
        ] {
            assert!(!config.origin_allowed(origin), "{origin} should be rejected");
        }
    }

    #[test]
    fn origin_allowlist_exact_match() {
        let config = Config {
            token: None,
            host: "127.0.0.1".parse().unwrap(),
            port: 3000,
            workspace: PathBuf::from("/workspace"),
            allowed_hosts: vec![],
            allowed_origins: vec!["https://app.example.com".to_string()],
            sse_session_ttl: Duration::from_secs(1800),
            sse_keepalive: Duration::from_secs(15),
        };
        assert!(config.origin_allowed("https://app.example.com"));
        assert!(!config.origin_allowed("http://localhost:3000"));
    }
}
