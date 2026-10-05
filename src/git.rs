use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub const QUICK: Duration = Duration::from_secs(20);
pub const REMOVE: Duration = Duration::from_secs(120);

pub fn command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_PAGER", "cat");
    cmd.stdin(Stdio::null());
    cmd
}

pub fn output(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|err| format!("cannot run git: {err}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(err)) => Err(format!("cannot run git: {err}")),
        Err(_) => {
            kill_pid(pid);
            let _ = rx.recv();
            Err(format!("git timed out after {}s", timeout.as_secs()))
        }
    }
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn stdout_text(cmd: Command, timeout: Duration) -> Result<String, String> {
    let out = output(cmd, timeout)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "git failed".to_string()
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

pub fn leave_tree(path: &std::path::Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    if cwd == path || cwd.starts_with(path) {
        if let Some(parent) = path.parent() {
            let _ = std::env::set_current_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_out_a_stuck_process() {
        let mut cmd = Command::new("sleep");
        cmd.arg("8");
        let err = output(cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn git_version_returns_without_polling_delay() {
        let start = std::time::Instant::now();
        let mut cmd = command();
        cmd.arg("--version");
        let text = stdout_text(cmd, QUICK).unwrap();
        assert!(text.contains("git version"), "{text}");
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "git --version took {:?}",
            start.elapsed()
        );
    }
}
