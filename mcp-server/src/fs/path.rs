//! Workspace path sandbox (PRD §17, pi-spec §36, §37).
//!
//! Every file tool funnels through [`resolve_in_workspace`]: lexical
//! normalization rejects `../` escapes, and canonicalization of the deepest
//! existing ancestor rejects symlink redirections out of the workspace
//! (including write targets that do not exist yet). `Path::starts_with`
//! compares whole components, so `/workspace-evil` never passes a
//! `/workspace` check.

use std::path::{Component, Path, PathBuf};

/// Resolve a user-supplied path against the workspace root and verify the
/// result stays inside it. `root` must already be canonicalized.
pub fn resolve_in_workspace(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let requested_path = Path::new(requested);
    let joined = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };

    // Lexical pass: reject `..` traversal out of the workspace before any
    // filesystem access.
    let normalized = lexical_normalize(&joined);
    if !normalized.starts_with(root) {
        return Err(format!("path escapes the workspace: {requested}"));
    }

    // Symlink pass: canonicalize the deepest existing ancestor, then re-append
    // the not-yet-existing tail. A symlink inside the workspace pointing
    // outside (e.g. /workspace/link -> /etc) resolves outside and is rejected.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = normalized.clone();
    let real_ancestor = loop {
        match current.canonicalize() {
            Ok(real) => break real,
            Err(_) => match current.file_name() {
                Some(name) => {
                    tail.push(name.to_os_string());
                    if !current.pop() {
                        return Err(format!("cannot resolve path: {requested}"));
                    }
                }
                None => return Err(format!("cannot resolve path: {requested}")),
            },
        }
    };
    let mut target = real_ancestor;
    for component in tail.iter().rev() {
        target.push(component);
    }
    if !target.starts_with(root) {
        return Err(format!("path escapes the workspace: {requested}"));
    }
    Ok(target)
}

/// Remove `.` components and resolve `..` lexically (no filesystem access).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::resolve_in_workspace;
    use std::path::Path;

    fn root() -> std::path::PathBuf {
        // /tmp is a symlink on macOS; canonicalize like Config does.
        std::fs::create_dir_all("/tmp/mcpod-paths-test").unwrap();
        Path::new("/tmp/mcpod-paths-test").canonicalize().unwrap()
    }

    #[test]
    fn accepts_relative_and_absolute_inside() {
        let root = root();
        assert!(resolve_in_workspace(&root, "src/main.rs").is_ok());
        assert_eq!(
            resolve_in_workspace(&root, "src/../a.txt").unwrap(),
            root.join("a.txt")
        );
        assert_eq!(
            resolve_in_workspace(&root, root.join("x").to_str().unwrap()).unwrap(),
            root.join("x")
        );
        assert_eq!(resolve_in_workspace(&root, ".").unwrap(), root);
    }

    #[test]
    fn rejects_escape_attempts() {
        let root = root();
        for path in [
            "../etc/passwd",
            "../../etc/passwd",
            "a/../../escape.txt",
            "/etc/passwd",
            "/etc",
            "..",
            "/tmp",
        ] {
            assert!(resolve_in_workspace(&root, path).is_err(), "{path} should be rejected");
        }
    }

    #[test]
    fn rejects_sibling_prefix_directory() {
        let root = root();
        let sibling = root.parent().unwrap().join("mcpod-paths-test-evil");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(resolve_in_workspace(&root, sibling.to_str().unwrap()).is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let root = root();
        let link = root.join("escape");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        assert!(resolve_in_workspace(&root, "escape/passwd").is_err());
        assert!(resolve_in_workspace(&root, "escape").is_err());
        // A symlink pointing back inside the workspace is allowed.
        std::os::unix::fs::symlink(&root, root.join("self")).unwrap();
        assert!(resolve_in_workspace(&root, "self/a.txt").is_ok());
        std::fs::remove_file(root.join("self")).unwrap();
        std::fs::remove_file(&link).unwrap();
    }

    #[test]
    fn rejects_write_target_under_escaped_symlink_dir() {
        // /workspace/link -> /etc ; write to /workspace/link/newfile must fail
        let root = root();
        let link = root.join("dir-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        assert!(resolve_in_workspace(&root, "dir-link/newfile.txt").is_err());
        std::fs::remove_file(&link).unwrap();
    }

    #[test]
    fn resolves_through_symlinked_temp_dir() {
        let root = root();
        assert!(resolve_in_workspace(&root, "deep/nested/new.txt").is_ok());
    }
}
