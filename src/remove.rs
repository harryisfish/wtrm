use std::process::Command;

use crate::scan::Worktree;

pub struct DeleteOutcome {
    pub path: std::path::PathBuf,
    pub error: Option<String>,
}

pub fn delete_many(
    items: &[Worktree],
    paths: &[std::path::PathBuf],
    force: bool,
) -> Vec<DeleteOutcome> {
    paths
        .iter()
        .filter_map(|path| {
            let wt = items.iter().find(|wt| &wt.path == path)?;
            let error = delete_one(wt, force).err();
            Some(DeleteOutcome {
                path: path.clone(),
                error,
            })
        })
        .collect()
}

pub fn delete_one(wt: &Worktree, force: bool) -> Result<(), String> {
    if wt.main || wt.bare || wt.path == wt.repo_root {
        return Err("不会删除主检出".to_string());
    }
    if !force {
        if wt.locked {
            return Err("已锁定，按 f 强制删除".to_string());
        }
        if !wt.missing && wt.dirty != Some(false) {
            return Err("不是干净工作区，按 f 强制删除".to_string());
        }
    }

    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    if wt.repo_root.exists() {
        cmd.arg("-C").arg(&wt.repo_root);
    } else {
        cmd.arg("--git-dir").arg(&wt.git_common_dir);
    }
    cmd.args(["worktree", "remove"]);
    if force {
        cmd.args(["--force", "--force"]);
    }
    cmd.arg(&wt.path);
    let out = cmd.output().map_err(|err| format!("无法运行 git: {err}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        "git worktree remove 失败".to_string()
    } else {
        err
    })
}

pub fn summarize(outcomes: &[DeleteOutcome]) -> String {
    let ok = outcomes.iter().filter(|item| item.error.is_none()).count();
    let failed: Vec<_> = outcomes
        .iter()
        .filter(|item| item.error.is_some())
        .collect();
    if failed.is_empty() {
        format!("已删除 {ok} 个 worktree")
    } else {
        let first = failed[0].error.as_deref().unwrap_or("");
        format!(
            "已删除 {ok} 个，失败 {} 个：{}（{first}）",
            failed.len(),
            failed[0].path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::fixture;

    #[test]
    fn refuses_main_checkout() {
        let wt = fixture("/repos/demo", "demo", true, Some(10));
        let err = delete_one(&wt, true).unwrap_err();
        assert!(err.contains("主检出"));
    }
}
