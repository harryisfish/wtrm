use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::remove::{self, BranchScope};
use crate::report::ActionItem;
use crate::scan::Worktree;

pub fn plan_delete(worktrees: &[Worktree]) -> Vec<ActionItem> {
    worktrees
        .iter()
        .filter(|wt| wt.deletable())
        .map(|wt| describe_delete(wt, false, "dry-run"))
        .collect()
}

pub fn delete_targets(
    worktrees: &[Worktree],
    paths: &[PathBuf],
    ids: &[String],
    execute: bool,
    force: bool,
    branches: BranchScope,
) -> Vec<ActionItem> {
    let (targets, mut items) = resolve_targets(worktrees, paths, ids, "delete");
    if execute {
        let paths: Vec<PathBuf> = targets.iter().map(|wt| wt.path.clone()).collect();
        let report = remove::delete_many(worktrees, &paths, force, branches);
        for outcome in report.worktrees {
            let Some(wt) = targets.iter().find(|wt| wt.path == outcome.path) else {
                continue;
            };
            let mut item = describe_delete(wt, force, "ok");
            if let Some(err) = outcome.error {
                if item.blocked_by.is_some() {
                    item.result = "blocked".to_string();
                    item.reason = err;
                } else {
                    item.result = "failed".to_string();
                    item.reason = err;
                }
            } else {
                item.result = "ok".to_string();
                item.blocked_by = None;
            }
            items.push(item);
        }
        if !report.branch_notes.is_empty() {
            let notes = report.branch_notes.join("; ");
            if let Some(item) = items.iter_mut().rev().find(|item| item.result == "ok") {
                item.reason = format!("{}; {notes}", item.reason);
            }
        }
    } else {
        for wt in targets {
            items.push(describe_delete(wt, force, "dry-run"));
        }
    }
    items
}

pub fn clean_targets(
    worktrees: &[Worktree],
    paths: &[PathBuf],
    ids: &[String],
    execute: bool,
) -> Vec<ActionItem> {
    let (targets, mut items) = resolve_targets(worktrees, paths, ids, "clean");
    for wt in targets {
        items.push(describe_clean(wt, execute));
    }
    items
}

pub fn summarize_items(items: &[ActionItem]) -> String {
    let ok = items.iter().filter(|item| item.result == "ok").count();
    let dry = items.iter().filter(|item| item.result == "dry-run").count();
    let blocked = items.iter().filter(|item| item.result == "blocked").count();
    let failed = items.iter().filter(|item| item.result == "failed").count();
    let mut parts = Vec::new();
    if ok > 0 {
        parts.push(format!("{ok} succeeded"));
    }
    if dry > 0 {
        parts.push(format!("{dry} dry-run"));
    }
    if blocked > 0 {
        let sample = items.iter().find(|item| item.result == "blocked");
        match sample {
            Some(item) => parts.push(format!(
                "{blocked} blocked ({}: {})",
                crate::scan::shorten_path(&item.path),
                item.blocked_by.as_deref().unwrap_or(item.reason.as_str())
            )),
            None => parts.push(format!("{blocked} blocked")),
        }
    }
    if failed > 0 {
        let sample = items.iter().find(|item| item.result == "failed");
        match sample {
            Some(item) => parts.push(format!(
                "{failed} failed ({}: {})",
                crate::scan::shorten_path(&item.path),
                item.reason
            )),
            None => parts.push(format!("{failed} failed")),
        }
    }
    if parts.is_empty() {
        "no matching worktrees".to_string()
    } else {
        parts.join(", ")
    }
}

pub fn result_lines(items: &[ActionItem], max: usize) -> Vec<String> {
    let mut lines: Vec<String> = items
        .iter()
        .take(max)
        .map(|item| {
            format!(
                "{}  {}  {}  {}",
                item.result,
                crate::scan::shorten_path(&item.path),
                item.blocked_by.as_deref().unwrap_or("-"),
                item.reason
            )
        })
        .collect();
    if items.len() > max {
        lines.push(format!("  …{} more", items.len() - max));
    }
    lines
}

pub fn counts(items: &[ActionItem]) -> (usize, usize) {
    let succeeded = items
        .iter()
        .filter(|item| item.result == "ok" || item.result == "dry-run")
        .count();
    let failed = items
        .iter()
        .filter(|item| item.result == "failed" || item.result == "blocked")
        .count();
    (succeeded, failed)
}

fn resolve_targets<'a>(
    worktrees: &'a [Worktree],
    paths: &[PathBuf],
    ids: &[String],
    action: &str,
) -> (Vec<&'a Worktree>, Vec<ActionItem>) {
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    let mut missing = Vec::new();
    for path in paths {
        match find_by_path(worktrees, path) {
            Some(wt) if seen.insert(wt.path.clone()) => targets.push(wt),
            Some(_) => {}
            None => missing.push(ActionItem::missing(
                action,
                "",
                path,
                &path.display().to_string(),
            )),
        }
    }
    for id in ids {
        match find_by_id(worktrees, id) {
            Ok(Some(wt)) if seen.insert(wt.path.clone()) => targets.push(wt),
            Ok(Some(_)) => {}
            Ok(None) => missing.push(ActionItem::missing(action, id, Path::new(""), id)),
            Err(err) => missing.push(ActionItem {
                action: action.to_string(),
                id: id.clone(),
                path: PathBuf::new(),
                reason: err,
                blocked_by: Some("ambiguous id".to_string()),
                result: "blocked".to_string(),
            }),
        }
    }
    (targets, missing)
}

fn find_by_path<'a>(worktrees: &'a [Worktree], path: &Path) -> Option<&'a Worktree> {
    worktrees.iter().find(|wt| same_path(&wt.path, path))
}

fn find_by_id<'a>(worktrees: &'a [Worktree], id: &str) -> Result<Option<&'a Worktree>, String> {
    if id.is_empty() {
        return Ok(None);
    }
    let exact: Vec<_> = worktrees.iter().filter(|wt| wt.id == id).collect();
    if exact.len() == 1 {
        return Ok(Some(exact[0]));
    }
    if id.len() >= 8 {
        let prefixed: Vec<_> = worktrees
            .iter()
            .filter(|wt| wt.id.starts_with(id))
            .collect();
        match prefixed.len() {
            0 => Ok(None),
            1 => Ok(Some(prefixed[0])),
            _ => Err(format!("id {id} matches {} worktrees", prefixed.len())),
        }
    } else if exact.is_empty() {
        Ok(None)
    } else {
        Err(format!("id {id} matches {} worktrees", exact.len()))
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn describe_delete(wt: &Worktree, force: bool, planned_result: &str) -> ActionItem {
    let blocked = blocked_by_delete(wt, force);
    let result = if blocked.is_some() {
        "blocked"
    } else {
        planned_result
    };
    ActionItem::new("delete", wt, delete_reason(wt), blocked, result)
}

fn describe_clean(wt: &Worktree, execute: bool) -> ActionItem {
    if !wt.deletable() {
        return ActionItem::new(
            "clean",
            wt,
            "main checkout",
            Some("main checkout".to_string()),
            "blocked",
        );
    }
    if wt.missing {
        return ActionItem::new(
            "clean",
            wt,
            "directory does not exist",
            Some("missing".to_string()),
            "blocked",
        );
    }
    match remove::find_deps(&wt.path) {
        Ok(dirs) => {
            if dirs.is_empty() {
                let result = if execute { "ok" } else { "dry-run" };
                return ActionItem::new("clean", wt, "no dependency directories", None, result);
            }
            if execute {
                match remove::clean_deps(&wt.path) {
                    Ok(removed) => ActionItem::new(
                        "clean",
                        wt,
                        format!("removed {}", join_paths(&removed)),
                        None,
                        "ok",
                    ),
                    Err(err) => ActionItem::new("clean", wt, err, None, "failed"),
                }
            } else {
                ActionItem::new(
                    "clean",
                    wt,
                    format!("would remove {}", join_paths(&dirs)),
                    None,
                    "dry-run",
                )
            }
        }
        Err(err) => ActionItem::new("clean", wt, err, None, "failed"),
    }
}

pub fn blocked_by_delete(wt: &Worktree, force: bool) -> Option<String> {
    if wt.main || wt.bare || wt.path == wt.repo_root {
        return Some("main checkout".to_string());
    }
    if force {
        return None;
    }
    if wt.locked {
        return Some("locked".to_string());
    }
    if !wt.missing && wt.dirty != Some(false) {
        return Some("dirty".to_string());
    }
    None
}

fn delete_reason(wt: &Worktree) -> String {
    let mut parts = Vec::new();
    if wt.merged == Some(true) {
        parts.push("merged into default branch");
    }
    if wt.missing {
        parts.push("missing directory");
    } else {
        match wt.dirty {
            Some(false) => parts.push("clean"),
            Some(true) => parts.push("dirty"),
            None => {}
        }
    }
    if wt.locked {
        parts.push("locked");
    }
    if parts.is_empty() {
        "linked worktree".to_string()
    } else {
        parts.join(", ")
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::fixture;

    #[test]
    fn plan_marks_dirty_as_blocked() {
        let mut dirty = fixture("/repos/demo/.worktrees/feat", "demo", false, Some(1));
        dirty.dirty = Some(true);
        let main = fixture("/repos/demo", "demo", true, Some(1));
        let items = plan_delete(&[main, dirty]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].action, "delete");
        assert_eq!(items[0].result, "blocked");
        assert_eq!(items[0].blocked_by.as_deref(), Some("dirty"));
    }

    #[test]
    fn plan_allows_clean_linked() {
        let clean = fixture("/repos/demo/.worktrees/feat", "demo", false, Some(1));
        let items = plan_delete(&[clean]);
        assert_eq!(items[0].result, "dry-run");
        assert_eq!(items[0].blocked_by, None);
        assert!(items[0].reason.contains("clean"));
    }

    #[test]
    fn delete_dry_run_does_not_need_git() {
        let wt = fixture("/repos/demo/.worktrees/feat", "demo", false, Some(1));
        let items = delete_targets(
            std::slice::from_ref(&wt),
            std::slice::from_ref(&wt.path),
            &[],
            false,
            false,
            BranchScope::Keep,
        );
        assert_eq!(items[0].result, "dry-run");
        assert_eq!(items[0].path, wt.path);
        assert!(!items[0].id.is_empty());
    }

    #[test]
    fn unknown_id_is_blocked() {
        let wt = fixture("/repos/demo/.worktrees/feat", "demo", false, Some(1));
        let items = delete_targets(
            &[wt],
            &[],
            &["deadbeefdeadbeef".to_string()],
            false,
            false,
            BranchScope::Keep,
        );
        assert_eq!(items[0].result, "blocked");
        assert_eq!(items[0].blocked_by.as_deref(), Some("not found"));
    }

    #[test]
    fn id_prefix_resolves() {
        let wt = fixture("/repos/demo/.worktrees/feat", "demo", false, Some(1));
        let prefix = wt.id[..8].to_string();
        let items = delete_targets(
            std::slice::from_ref(&wt),
            &[],
            std::slice::from_ref(&prefix),
            false,
            false,
            BranchScope::Keep,
        );
        assert_eq!(items[0].id, wt.id);
        assert_eq!(items[0].result, "dry-run");
    }
}
