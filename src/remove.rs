use std::collections::BTreeSet;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BranchScope {
    #[default]
    Keep,
    Local,
    GitHub,
}

impl BranchScope {
    pub fn label(self) -> &'static str {
        match self {
            BranchScope::Keep => "keep",
            BranchScope::Local => "local",
            BranchScope::GitHub => "local+GitHub",
        }
    }

    pub fn next(self) -> Self {
        match self {
            BranchScope::Keep => BranchScope::Local,
            BranchScope::Local => BranchScope::GitHub,
            BranchScope::GitHub => BranchScope::Keep,
        }
    }
}

pub struct DeleteOutcome {
    pub path: std::path::PathBuf,
    pub error: Option<String>,
}

pub struct DeleteReport {
    pub worktrees: Vec<DeleteOutcome>,
    pub branch_notes: Vec<String>,
}

pub fn delete_many(
    items: &[Worktree],
    paths: &[std::path::PathBuf],
    force: bool,
    branches: BranchScope,
) -> DeleteReport {
    let mut worktrees = Vec::new();
    let mut removed = Vec::new();
    for path in paths {
        let Some(wt) = items.iter().find(|wt| &wt.path == path) else {
            continue;
        };
        let error = delete_one(wt, force).err();
        if error.is_none() {
            removed.push(wt);
        }
        worktrees.push(DeleteOutcome {
            path: path.clone(),
            error,
        });
    }
    let branch_notes = if branches == BranchScope::Keep {
        Vec::new()
    } else {
        delete_branches(items, &removed, branches)
    };
    DeleteReport {
        worktrees,
        branch_notes,
    }
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

pub fn summarize(report: &DeleteReport) -> String {
    let ok = report
        .worktrees
        .iter()
        .filter(|item| item.error.is_none())
        .count();
    let failed: Vec<_> = report
        .worktrees
        .iter()
        .filter(|item| item.error.is_some())
        .collect();
    let mut message = if failed.is_empty() {
        format!("deleted {ok} worktrees")
    } else {
        let first = failed[0].error.as_deref().unwrap_or("");
        format!(
            "deleted {ok}, {} failed: {} ({first})",
            failed.len(),
            failed[0].path.display()
        )
    };
    if !report.branch_notes.is_empty() {
        message.push_str(". ");
        message.push_str(&report.branch_notes.join("; "));
    }
    message
}

fn delete_branches(items: &[Worktree], removed: &[&Worktree], scope: BranchScope) -> Vec<String> {
    let mut notes = Vec::new();
    let mut seen = BTreeSet::new();
    for wt in removed {
        let Some(name) = branch_name(wt) else {
            notes.push(format!("{}: no local branch", wt.path.display()));
            continue;
        };
        let key = (wt.repo_root.clone(), name.to_string());
        if !seen.insert(key) {
            continue;
        }
        if branch_still_used(wt, name, items, removed) {
            notes.push(format!("{name}: kept, still checked out"));
            continue;
        }
        match delete_local_branch(wt, name) {
            Ok(()) => notes.push(format!("deleted local {name}")),
            Err(err) => {
                notes.push(format!("{name}: {err}"));
                continue;
            }
        }
        if scope == BranchScope::GitHub {
            notes.push(match delete_github_branch(wt, name) {
                Ok(remote) => format!("deleted {remote}/{name}"),
                Err(err) => format!("{name}: {err}"),
            });
        }
    }
    notes
}

fn branch_name(wt: &Worktree) -> Option<&str> {
    match wt.branch.as_str() {
        "" | "(detached)" | "(bare)" => None,
        name => Some(name),
    }
}

fn branch_still_used(wt: &Worktree, name: &str, items: &[Worktree], removed: &[&Worktree]) -> bool {
    items.iter().any(|other| {
        other.repo_root == wt.repo_root
            && other.branch == name
            && other.path != wt.path
            && !removed.iter().any(|gone| gone.path == other.path)
    })
}

fn delete_local_branch(wt: &Worktree, name: &str) -> Result<(), String> {
    let out = git_at(wt)
        .args(["branch", "-D", name])
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(git_err(&out, "git branch -D failed"))
    }
}

fn delete_github_branch(wt: &Worktree, name: &str) -> Result<String, String> {
    let out = git_at(wt)
        .args(["remote", "-v"])
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if !out.status.success() {
        return Err(git_err(&out, "git remote failed"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(remote) = github_remote(&text) else {
        return Err("no GitHub remote".to_string());
    };
    let pushed = git_at(wt)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["push", &remote, "--delete", name])
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if pushed.status.success() {
        Ok(remote)
    } else {
        Err(git_err(&pushed, "git push --delete failed"))
    }
}

fn github_remote(text: &str) -> Option<String> {
    let mut first = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let name = parts.next()?;
        let url = parts.next()?;
        if !url.contains("github.com") {
            continue;
        }
        if name == "origin" {
            return Some(name.to_string());
        }
        if first.is_none() {
            first = Some(name.to_string());
        }
    }
    first
}

fn git_at(wt: &Worktree) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    if wt.repo_root.exists() {
        cmd.arg("-C").arg(&wt.repo_root);
    } else {
        cmd.arg("--git-dir").arg(&wt.git_common_dir);
    }
    cmd
}

fn git_err(out: &std::process::Output, fallback: &str) -> String {
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if err.is_empty() {
        fallback.to_string()
    } else {
        err
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
    fn github_remote_prefers_origin() {
        let text = "\
upstream\tgit@github.com:other/demo.git (fetch)
upstream\tgit@github.com:other/demo.git (push)
origin\thttps://github.com/harryisfish/demo.git (push)
gitlab\tgit@gitlab.com:acme/demo.git (push)
";
        assert_eq!(github_remote(text).as_deref(), Some("origin"));
    }

    #[test]
    fn deletes_local_branch_and_keeps_main() {
        let git = Command::new("git").arg("--version").output();
        if git.map(|out| !out.status.success()).unwrap_or(true) {
            return;
        }
        let root = std::env::temp_dir().join(format!("wtrm-branch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let repo = root.join("demo");
        let linked = root.join("demo-feature");
        let init = Command::new("git")
            .args(["init", "-b", "main"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(repo.join("README"), "hi\n").unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.email=wtrm@example.com", "-c", "user.name=wtrm"])
                .args(args)
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["add", "README"]));
        assert!(git(&["commit", "-m", "init"]));
        assert!(git(&[
            "worktree",
            "add",
            "-b",
            "feature",
            linked.to_str().unwrap()
        ]));
        let result = crate::scan::load(
            std::slice::from_ref(&root),
            4,
            &crate::scan::Query::default(),
        );
        let feature = result
            .worktrees
            .iter()
            .find(|wt| wt.branch == "feature")
            .unwrap();
        let report = delete_many(
            &result.worktrees,
            std::slice::from_ref(&feature.path),
            false,
            BranchScope::Local,
        );
        assert!(
            report.worktrees[0].error.is_none(),
            "{:?}",
            report.worktrees[0].error
        );
        assert!(report
            .branch_notes
            .iter()
            .any(|note| note.contains("deleted local feature")));
        let branches = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "feature", "main"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&branches.stdout);
        assert!(!names.contains("feature"), "{names}");
        assert!(names.contains("main"), "{names}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_main_checkout() {
        let wt = fixture("/repos/demo", "demo", true, Some(10));
        let err = delete_one(&wt, true).unwrap_err();
        assert!(err.contains("main checkout"));
    }
}
