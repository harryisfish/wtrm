use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::scan::Worktree;

const DEP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".next",
    ".turbo",
    ".venv",
    "venv",
    "__pycache__",
    "Pods",
    ".gradle",
];

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
        return Err("refusing to delete the main checkout".to_string());
    }
    if !force {
        if wt.locked {
            return Err("locked; press f to force".to_string());
        }
        if !wt.missing && wt.dirty != Some(false) {
            return Err("not a clean worktree; press f to force".to_string());
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
    let out = cmd
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        "git worktree remove failed".to_string()
    } else {
        err
    })
}

pub struct CleanOutcome {
    pub removed: Vec<PathBuf>,
    pub errors: Vec<String>,
}

pub fn clean_many(items: &[Worktree], paths: &[PathBuf]) -> CleanOutcome {
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        let Some(wt) = items.iter().find(|wt| &wt.path == path) else {
            continue;
        };
        if !wt.deletable() || wt.missing {
            errors.push(format!(
                "{}: skipped main checkout or missing directory",
                path.display()
            ));
            continue;
        }
        match clean_deps(&wt.path) {
            Ok(found) => removed.extend(found),
            Err(err) => errors.push(err),
        }
    }
    CleanOutcome { removed, errors }
}

pub fn clean_deps(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.is_dir() {
        return Err(format!("directory does not exist: {}", root.display()));
    }
    let mut removed = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                return Err(format!("{}：{err}", dir.display()));
            }
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" {
                continue;
            }
            if DEP_DIRS.contains(&name.as_ref()) {
                let path = entry.path();
                fs::remove_dir_all(&path).map_err(|err| format!("{}：{err}", path.display()))?;
                removed.push(path);
            } else {
                stack.push(entry.path());
            }
        }
    }
    Ok(removed)
}

pub fn summarize_clean(outcome: &CleanOutcome) -> String {
    if outcome.errors.is_empty() {
        format!("cleaned {} dependency directories", outcome.removed.len())
    } else {
        format!(
            "cleaned {} dependency directories, {} failed: {}",
            outcome.removed.len(),
            outcome.errors.len(),
            outcome.errors[0]
        )
    }
}

pub fn summarize(outcomes: &[DeleteOutcome]) -> String {
    let ok = outcomes.iter().filter(|item| item.error.is_none()).count();
    let failed: Vec<_> = outcomes
        .iter()
        .filter(|item| item.error.is_some())
        .collect();
    if failed.is_empty() {
        format!("deleted {ok} worktrees")
    } else {
        let first = failed[0].error.as_deref().unwrap_or("");
        format!(
            "deleted {ok}, {} failed: {} ({first})",
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
    fn removes_dependency_dirs_and_keeps_source() {
        let root = std::env::temp_dir().join(format!("wtrm-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.js"), "ok\n").unwrap();
        fs::create_dir_all(root.join("node_modules/left")).unwrap();
        fs::write(root.join("node_modules/left/index.js"), "x\n").unwrap();
        fs::create_dir_all(root.join("apps/web/node_modules/pkg")).unwrap();
        fs::write(root.join("apps/web/node_modules/pkg/a.js"), "x\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("keep-target.txt"), "no\n").unwrap();

        let removed = clean_deps(&root).unwrap();
        assert_eq!(removed.len(), 3);
        assert!(!root.join("node_modules").exists());
        assert!(!root.join("apps/web/node_modules").exists());
        assert!(!root.join("target").exists());
        assert_eq!(
            fs::read_to_string(root.join("src/main.js")).unwrap(),
            "ok\n"
        );
        assert!(root.join("apps/web").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_main_checkout() {
        let wt = fixture("/repos/demo", "demo", true, Some(10));
        let err = delete_one(&wt, true).unwrap_err();
        assert!(err.contains("main checkout"));
    }
}
