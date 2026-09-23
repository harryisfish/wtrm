use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rayon::prelude::*;
use serde::Serialize;

use crate::timeutil::{self, format_utc};

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    ".next",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    "Library",
    ".Trash",
    "Applications",
    "Pods",
    ".gradle",
    "DerivedData",
    ".cache",
    ".npm",
    ".cargo",
    ".rustup",
    "Pictures",
    "Movies",
    "Music",
];

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub parent: PathBuf,
    pub repo: String,
    pub repo_root: PathBuf,
    pub git_common_dir: PathBuf,
    pub branch: String,
    pub head: String,
    pub main: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
    pub missing: bool,
    pub dirty: Option<bool>,
    pub last_commit: Option<i64>,
    pub last_active: Option<i64>,
    pub last_active_utc: Option<String>,
}

impl Worktree {
    pub fn deletable(&self) -> bool {
        !self.main && !self.bare
    }

    pub fn flags(&self) -> String {
        let mut flags = Vec::new();
        if self.bare {
            flags.push("bare");
        } else if self.main {
            flags.push("main");
        } else if self.missing {
            flags.push("missing");
        } else {
            match self.dirty {
                Some(true) => flags.push("dirty"),
                Some(false) => flags.push("clean"),
                None => flags.push("unknown"),
            }
        }
        if self.locked {
            flags.push("locked");
        }
        if self.prunable {
            flags.push("prunable");
        }
        if flags.is_empty() {
            flags.push("-");
        }
        flags.join(",")
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ScanResult {
    pub repos: usize,
    pub worktrees: Vec<Worktree>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct RawWt {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    prunable: bool,
}

struct Job {
    raw: RawWt,
    repo: String,
    repo_root: PathBuf,
    git_common_dir: PathBuf,
    main: bool,
}

pub fn load(roots: &[PathBuf], max_depth: u8, older_than_secs: Option<i64>) -> ScanResult {
    let mut result = scan(roots, max_depth);
    result.worktrees = apply_view(result.worktrees, older_than_secs, timeutil::now_unix());
    result.repos = result
        .worktrees
        .iter()
        .map(|wt| &wt.git_common_dir)
        .collect::<HashSet<_>>()
        .len();
    result
}

pub fn apply_view(
    mut worktrees: Vec<Worktree>,
    older_than_secs: Option<i64>,
    now: i64,
) -> Vec<Worktree> {
    if let Some(min_age) = older_than_secs {
        let old_repos: HashSet<PathBuf> = worktrees
            .iter()
            .filter(|wt| wt.deletable() && is_old(wt, now, min_age))
            .map(|wt| wt.git_common_dir.clone())
            .collect();
        worktrees.retain(|wt| {
            if wt.deletable() {
                is_old(wt, now, min_age)
            } else {
                old_repos.contains(&wt.git_common_dir)
            }
        });
    }
    worktrees.sort_by(|a, b| {
        b.deletable()
            .cmp(&a.deletable())
            .then_with(|| activity_ord(a.last_active, b.last_active))
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| a.path.cmp(&b.path))
    });
    worktrees
}

fn is_old(wt: &Worktree, now: i64, min_age: i64) -> bool {
    match wt.last_active {
        Some(ts) => now.saturating_sub(ts) >= min_age,
        None => true,
    }
}

fn activity_ord(a: Option<i64>, b: Option<i64>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

pub fn scan(roots: &[PathBuf], max_depth: u8) -> ScanResult {
    let mut errors = Vec::new();
    let mut commons = BTreeSet::new();
    for root in roots {
        if !root.exists() {
            errors.push(format!("不存在: {}", root.display()));
            continue;
        }
        discover(root, max_depth, &mut commons);
    }

    let inspected: Vec<Result<Vec<Job>, String>> =
        commons.par_iter().map(|common| inspect(common)).collect();

    let mut jobs = Vec::new();
    for item in inspected {
        match item {
            Ok(found) => jobs.extend(found),
            Err(err) => errors.push(err),
        }
    }

    let worktrees: Vec<Worktree> = jobs.into_par_iter().map(enrich).collect();
    ScanResult {
        repos: 0,
        worktrees,
        errors,
    }
}

fn discover(root: &Path, max_depth: u8, out: &mut BTreeSet<PathBuf>) {
    let mut stack = vec![(root.to_path_buf(), 0u8)];
    let mut seen = HashSet::new();
    while let Some((dir, depth)) = stack.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let git = dir.join(".git");
        if git.exists() {
            if let Some(common) = common_git_dir(&git) {
                out.insert(common);
            }
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.iter().any(|skip| *skip == name.as_ref()) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
}

fn inspect(common: &Path) -> Result<Vec<Job>, String> {
    let git_dir = common.to_string_lossy().into_owned();
    let text = git_output(&["--git-dir", &git_dir, "worktree", "list", "--porcelain"])?;
    let records = parse_porcelain(&text);
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let main_path = records
        .iter()
        .find(|raw| !raw.bare)
        .map(|raw| raw.path.clone());
    let has_extra = records
        .iter()
        .any(|raw| !raw.bare && main_path.as_ref() != Some(&raw.path));
    if !has_extra {
        return Ok(Vec::new());
    }
    let repo_root = main_path
        .clone()
        .or_else(|| common.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| common.to_path_buf());
    let repo = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let git_common_dir = common.to_path_buf();
    Ok(records
        .into_iter()
        .map(|raw| {
            let main = !raw.bare && main_path.as_ref() == Some(&raw.path);
            Job {
                raw,
                repo: repo.clone(),
                repo_root: repo_root.clone(),
                git_common_dir: git_common_dir.clone(),
                main,
            }
        })
        .collect())
}

fn enrich(job: Job) -> Worktree {
    let branch = branch_label(&job.raw);
    let path = job.raw.path;
    let missing = !path.exists();
    let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let (dirty, last_commit, last_active) = if job.raw.bare || missing {
        (None, None, None)
    } else {
        let dirty = match git_output_in(&path, &["status", "--porcelain"]) {
            Ok(text) => Some(!text.trim().is_empty()),
            Err(_) => None,
        };
        let last_commit = git_output_in(&path, &["log", "-1", "--format=%ct"])
            .ok()
            .and_then(|text| text.trim().parse::<i64>().ok());
        let last_active = [last_commit, mtime(&path), index_mtime(&path)]
            .into_iter()
            .flatten()
            .max();
        (dirty, last_commit, last_active)
    };
    Worktree {
        path,
        parent,
        repo: job.repo,
        repo_root: job.repo_root,
        git_common_dir: job.git_common_dir,
        branch,
        head: job.raw.head,
        main: job.main,
        bare: job.raw.bare,
        locked: job.raw.locked,
        prunable: job.raw.prunable,
        missing,
        dirty,
        last_commit,
        last_active,
        last_active_utc: last_active.map(format_utc),
    }
}

fn branch_label(raw: &RawWt) -> String {
    if raw.bare {
        "(bare)".to_string()
    } else if raw.detached || raw.branch.is_none() {
        "(detached)".to_string()
    } else {
        raw.branch
            .as_deref()
            .unwrap_or("")
            .trim_start_matches("refs/heads/")
            .to_string()
    }
}

fn index_mtime(worktree: &Path) -> Option<i64> {
    let dot_git = worktree.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        read_gitdir(&dot_git)?
    };
    mtime(&git_dir.join("index"))
}

fn mtime(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|dur| dur.as_secs() as i64)
}

fn parse_porcelain(text: &str) -> Vec<RawWt> {
    let mut out = Vec::new();
    let mut cur: Option<RawWt> = None;
    for line in text.lines() {
        if line.is_empty() {
            if let Some(done) = cur.take() {
                out.push(done);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = cur.take() {
                out.push(done);
            }
            cur = Some(RawWt {
                path: PathBuf::from(path),
                ..RawWt::default()
            });
            continue;
        }
        let Some(cur) = cur.as_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            cur.head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            cur.branch = Some(branch.to_string());
        } else if line == "detached" {
            cur.detached = true;
        } else if line == "bare" {
            cur.bare = true;
        } else if let Some(rest) = line.strip_prefix("locked") {
            cur.locked = true;
            let reason = rest.trim();
            if !reason.is_empty() {
                let _ = reason;
            }
        } else if let Some(rest) = line.strip_prefix("prunable") {
            cur.prunable = true;
            let _ = rest;
        }
    }
    if let Some(done) = cur {
        out.push(done);
    }
    out
}

fn common_git_dir(dot_git: &Path) -> Option<PathBuf> {
    let git_dir = if dot_git.is_dir() {
        dot_git.to_path_buf()
    } else {
        read_gitdir(dot_git)?
    };
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let rel = text.trim();
            if rel.is_empty() {
                git_dir.clone()
            } else {
                git_dir.join(rel)
            }
        }
        Err(_) => git_dir.clone(),
    };
    Some(normalize(&common))
}

fn read_gitdir(dot_git_file: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dot_git_file).ok()?;
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(rest);
    let abs = if path.is_absolute() {
        path
    } else {
        dot_git_file.parent()?.join(path)
    };
    Some(normalize(&abs))
}

fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn git_output(args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.args(args);
    finish_git(cmd, args)
}

fn git_output_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.arg("-C").arg(dir);
    cmd.args(args);
    finish_git(cmd, args)
}

fn finish_git(mut cmd: Command, args: &[&str]) -> Result<String, String> {
    let out = cmd.output().map_err(|err| format!("无法运行 git: {err}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("git {} 失败", args.join(" "))
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

pub fn shorten_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
pub fn fixture(path: &str, repo: &str, main: bool, last_active: Option<i64>) -> Worktree {
    let path = PathBuf::from(path);
    let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
    Worktree {
        path,
        parent,
        repo: repo.to_string(),
        repo_root: PathBuf::from(format!("/repos/{repo}")),
        git_common_dir: PathBuf::from(format!("/repos/{repo}/.git")),
        branch: if main {
            "main".to_string()
        } else {
            "feature".to_string()
        },
        head: "abc".to_string(),
        main,
        bare: false,
        locked: false,
        prunable: false,
        missing: false,
        dirty: Some(false),
        last_commit: last_active,
        last_active,
        last_active_utc: last_active.map(format_utc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_records() {
        let text = "\
worktree /repo
HEAD aaaa
branch refs/heads/main

worktree /repo/.worktrees/feat
HEAD bbbb
branch refs/heads/feat
locked feature freeze

worktree /gone
HEAD cccc
detached
prunable gitdir file points to non-existent location
";
        let parsed = parse_porcelain(text);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(parsed[1].locked);
        assert!(parsed[2].detached);
        assert!(parsed[2].prunable);
    }

    #[test]
    fn older_than_keeps_matching_repo_main() {
        let now = 2_000_000;
        let old = fixture(
            "/repos/old/.worktrees/feat",
            "old",
            false,
            Some(now - 86_400 * 20),
        );
        let old_main = fixture("/repos/old", "old", true, Some(now - 10));
        let fresh = fixture(
            "/repos/new/.worktrees/feat",
            "new",
            false,
            Some(now - 86_400),
        );
        let fresh_main = fixture("/repos/new", "new", true, Some(now - 10));
        let viewed = apply_view(
            vec![fresh, old_main, fresh_main, old],
            Some(86_400 * 10),
            now,
        );
        let repos: HashSet<_> = viewed.iter().map(|wt| wt.repo.as_str()).collect();
        assert_eq!(repos, HashSet::from(["old"]));
        assert!(viewed.iter().any(|wt| wt.main));
        assert!(viewed.iter().any(|wt| wt.deletable()));
    }

    #[test]
    fn scans_a_real_linked_worktree() {
        let git = Command::new("git").arg("--version").output();
        if git.map(|out| !out.status.success()).unwrap_or(true) {
            return;
        }
        let root = std::env::temp_dir().join(format!("wtrm-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let _cleanup = scopeguard(&root);
        let repo = root.join("demo");
        let linked = root.join("demo-feature");
        let init = Command::new("git")
            .args(["init", "-b", "main"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(repo.join("README"), "hi\n").unwrap();
        assert!(git_in(&repo, &["add", "README"]));
        assert!(git_in(&repo, &["commit", "-m", "init"]));
        assert!(git_in(
            &repo,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()]
        ));

        let result = load(std::slice::from_ref(&root), 4, None);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.repos, 1);
        assert_eq!(result.worktrees.len(), 2);
        let extra = result
            .worktrees
            .iter()
            .find(|wt| wt.deletable())
            .expect("linked worktree");
        assert_eq!(extra.branch, "feature");
        assert_eq!(extra.repo, "demo");
        assert_eq!(extra.dirty, Some(false));
        assert!(extra.last_active.is_some());
        assert!(result.worktrees.iter().any(|wt| wt.main && !wt.deletable()));

        fs::write(linked.join("extra.txt"), "x\n").unwrap();
        let again = load(std::slice::from_ref(&root), 4, None);
        let dirty = again.worktrees.iter().find(|wt| wt.deletable()).unwrap();
        assert_eq!(dirty.dirty, Some(true));
        let refused = crate::remove::delete_one(dirty, false).unwrap_err();
        assert!(refused.contains("干净"), "{refused}");
        crate::remove::delete_one(dirty, true).unwrap();
        assert!(!linked.exists());
    }

    fn scopeguard(path: &Path) -> impl Drop {
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        Guard(path.to_path_buf())
    }

    fn git_in(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=wtrm@example.com", "-c", "user.name=wtrm"])
            .args(args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
