use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Builds a non-interactive shell command configured to avoid hidden stdin prompts,
/// interactive pagers, buffered Python output, and Windows `\\?\` path quirks.
pub fn build_shell_command(root: &Path, command: &str) -> Command {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };

    let clean_cwd = crate::workspace::clean_path_string(root);
    cmd.current_dir(&clean_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CI", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("NPM_CONFIG_YES", "true")
        .env("npm_config_yes", "true")
        .env("PIP_NO_INPUT", "1")
        .env("PYTHONUNBUFFERED", "1")
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never");

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    cmd
}

pub fn kill_pid_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = pid;
    }
}

pub struct ChildTreeGuard {
    child: Option<tokio::process::Child>,
}

impl ChildTreeGuard {
    pub fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    pub fn child_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    pub fn disarm(&mut self) {
        self.child = None;
    }

    pub fn kill_tree_now(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = child.id() {
                kill_pid_tree(pid);
            }
            let _ = child.start_kill();
        }
    }
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        self.kill_tree_now();
    }
}
