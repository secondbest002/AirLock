use std::process::Stdio;

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
