//! `initialize` instructions (PRD §12, pi-spec §2, §41): everything an agent
//! must know to use MCPod, delivered before any resource read (§26).

use std::path::Path;

pub fn text(workspace: &Path) -> String {
    let workspace = workspace.display();
    format!(
        r#"You are connected to an MCPod development container.

Workspace:

- The project workspace is {workspace}.
- Perform project work inside {workspace}.
- Shell commands run with {workspace} as the default working directory.

Available tools:

- read: Read file contents.
- write: Create new files or completely replace file contents.
- edit: Make precise changes to existing files.
- bash: Execute shell commands, including git, search, build, test, package management, and development tools.

Tool usage:

- Use read to inspect source files instead of `cat` or `sed`.
- Use edit for targeted changes to existing files.
- Use write for new files or complete rewrites.
- Use bash for `ls`, `rg`, `find`, git, package managers, builds, tests, and other shell operations.
- Prefer the dedicated file tools over shell-based file modification.
- Do not rely on temporary shell state such as `cd`, aliases, or exported variables persisting across separate bash calls.

Runtime:

- mise is installed.
- Use mise to manage development runtimes such as Node.js, Python, Rust, Go, and Java.
- Respect project-defined mise configuration when present.
- Run `mise install` when the project declares runtimes that are not yet installed.

Privileges:

- You run as the `mcpod` user mapped onto the workspace owner's UID/GID, so files you create keep the host workspace ownership.
- You have passwordless sudo inside the container for system-level operations.
- Do not prefix ordinary project commands with sudo: files created via sudo are owned by root.

Package installation:

- The container is a disposable development environment.
- You may install missing system packages with sudo apt-get when required.
- Prefer `sudo apt-get install -y --no-install-recommends <package>`.
- Run `sudo apt-get update` when package metadata is unavailable or stale.
- Prefer mise for development runtimes and runtime-managed tools.
- Install project-specific system libraries only when required.

Project instructions:

- If {workspace}/AGENTS.md exists, read it before modifying the project.
- Follow project-specific instructions found there.

Persistence:

- Only mounted directories are guaranteed to persist.
- Changes outside mounted directories may be lost when the container is recreated."#
    )
}

#[cfg(test)]
mod tests {
    use super::text;

    #[test]
    fn contains_core_guidance() {
        let instructions = text(std::path::Path::new("/workspace"));
        assert!(instructions.contains("You are connected to an MCPod development container"));
        assert!(instructions.contains("The project workspace is /workspace"));
        assert!(instructions.contains("read: Read file contents."));
        assert!(instructions.contains("write: Create new files or completely replace file contents."));
        assert!(instructions.contains("edit: Make precise changes to existing files."));
        assert!(instructions.contains("Execute shell commands, including git, search, build, test"));
        assert!(instructions.contains("Use read to inspect source files instead of `cat` or `sed`."));
        assert!(instructions.contains("Use bash for `ls`, `rg`, `find`, git, package managers, builds, tests"));
        assert!(instructions.contains("Do not rely on temporary shell state such as `cd`, aliases"));
        assert!(instructions.contains("mise install"));
        assert!(instructions.contains("mapped onto the workspace owner's UID/GID"));
        assert!(instructions.contains("passwordless sudo inside the container"));
        assert!(instructions.contains("files created via sudo are owned by root"));
        assert!(instructions.contains("sudo apt-get install -y --no-install-recommends"));
        assert!(instructions.contains("AGENTS.md exists, read it before modifying"));
        assert!(instructions.contains("Only mounted directories are guaranteed to persist"));
    }
}
