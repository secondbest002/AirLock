use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
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

/// Builds a non-interactive `git` command with cleaned cwd, optional read-only lock avoidance,
/// non-interactive environment variables, and Windows window suppression.
pub fn build_git_command(root: &Path, args: &[&str], read_only: bool) -> Command {
    let mut cmd = Command::new("git");
    if read_only {
        cmd.arg("--no-optional-locks");
    }
    let clean_cwd = crate::workspace::clean_path_string(root);
    cmd.args(args)
        .current_dir(&clean_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1");

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    cmd
}

/// Runs a `git` command with bounded execution time, concurrent pipe draining,
/// and deterministic process-tree cleanup on timeout or failure.
pub async fn run_git_command_bounded(
    root: &Path,
    args: &[&str],
    read_only: bool,
    timeout_secs: u64,
) -> Result<(i32, String, String), String> {
    let mut cmd = build_git_command(root, args, read_only);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Gagal menjalankan git: {e}"))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let mut guard = ChildTreeGuard::new(child);

    let wait_res = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        match guard.child_mut() {
            Some(c) => c.wait().await,
            None => Err(std::io::Error::other("child process missing")),
        }
    })
    .await;

    match wait_res {
        Ok(Ok(status)) => {
            guard.disarm();
            let stdout_bytes = stdout_task.await.unwrap_or_default();
            let stderr_bytes = stderr_task.await.unwrap_or_default();
            let code = status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
            let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
            Ok((code, stdout, stderr))
        }
        Ok(Err(e)) => {
            guard.kill_tree_now();
            stdout_task.abort();
            stderr_task.abort();
            Err(format!("Gagal menjalankan git: {e}"))
        }
        Err(_) => {
            guard.kill_tree_now();
            stdout_task.abort();
            stderr_task.abort();
            Err(format!("Perintah git timeout setelah {timeout_secs} detik."))
        }
    }
}

/// Suppresses blocking Windows critical-error and GPF modal popups inherited by child processes.
pub fn suppress_windows_error_dialogs() {
    #[cfg(target_os = "windows")]
    {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(u_mode: u32) -> u32;
        }
        unsafe {
            SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as StdCommand;

    #[test]
    fn test_build_git_command_strips_verbatim_prefix_and_toggles_no_optional_locks() {
        let raw_root = if cfg!(target_os = "windows") {
            Path::new(r"\\?\C:\Temp\demo")
        } else {
            Path::new("/tmp/demo")
        };

        let ro_cmd = build_git_command(raw_root, &["status", "--short"], true);
        let ro_std = ro_cmd.as_std();
        let ro_args: Vec<String> = ro_std
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(ro_args, vec!["--no-optional-locks", "status", "--short"]);
        if cfg!(target_os = "windows") {
            let cwd = ro_std.get_current_dir().expect("cwd set");
            assert!(
                !cwd.to_string_lossy().starts_with(r"\\?\"),
                "verbatim prefix should be stripped from cwd: {}",
                cwd.display()
            );
        }

        let rw_cmd = build_git_command(raw_root, &["commit", "-m", "hi"], false);
        let rw_args: Vec<String> = rw_cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(rw_args, vec!["commit", "-m", "hi"]);
    }

    #[tokio::test]
    async fn test_run_git_command_bounded_read_only_leaves_no_index_lock() {
        let repo_dir = std::env::temp_dir().join("smart_mcp_process_util_git_test");
        let _ = fs::remove_dir_all(&repo_dir);
        fs::create_dir_all(&repo_dir).unwrap();

        let run_init = |args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(&repo_dir)
                .output()
                .expect("git init helper should run");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run_init(&["init"]);
        run_init(&["config", "user.email", "test@airlock.local"]);
        run_init(&["config", "user.name", "Airlock Test"]);
        fs::write(repo_dir.join("a.txt"), "v1\n").unwrap();
        run_init(&["add", "."]);
        run_init(&["commit", "-m", "init"]);
        fs::write(repo_dir.join("a.txt"), "v2\n").unwrap();

        let canon_repo = fs::canonicalize(&repo_dir).unwrap();
        let (code, stdout, stderr) =
            run_git_command_bounded(&canon_repo, &["status", "--porcelain"], true, 6)
                .await
                .expect("bounded read-only git status should succeed");

        assert_eq!(code, 0, "stderr: {stderr}");
        assert!(stdout.contains("a.txt"), "stdout: {stdout}");
        assert!(
            !canon_repo.join(".git").join("index.lock").exists(),
            "read-only git status must not leave .git/index.lock"
        );

        let _ = fs::remove_dir_all(&repo_dir);
    }
}
