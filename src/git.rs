use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("cannot run git: {err}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing git stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing git stderr".to_string())?;
    let stdout_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("git timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(err) => return Err(format!("cannot run git: {err}")),
        }
    };
    let stdout = stdout_h.join().unwrap_or_default();
    let stderr = stderr_h.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
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
}
