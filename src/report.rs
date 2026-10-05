use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::scan::{self, ScanResult, Worktree};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub roots: Vec<String>,
    pub scanned_repos: usize,
    pub worktrees: Vec<Worktree>,
    pub items: Vec<ActionItem>,
    pub errors: Vec<String>,
    pub complete: bool,
    pub scan_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionItem {
    pub action: String,
    pub id: String,
    pub path: PathBuf,
    pub reason: String,
    pub blocked_by: Option<String>,
    pub result: String,
}

impl Report {
    pub fn from_scan(roots: &[PathBuf], result: ScanResult) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            roots: roots.iter().map(|path| scan::shorten_path(path)).collect(),
            scanned_repos: result.repos,
            worktrees: result.worktrees,
            items: Vec::new(),
            errors: result.errors,
            complete: true,
            scan_ms: 0,
        }
    }

    pub fn with_scan_ms(mut self, scan_ms: u64) -> Self {
        self.scan_ms = scan_ms;
        self
    }

    pub fn with_items(mut self, items: Vec<ActionItem>) -> Self {
        self.items = items;
        self
    }

    pub fn failed(&self) -> bool {
        self.items.iter().any(|item| item.result == "failed")
    }

    pub fn blocked_or_failed_on_execute(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.result == "failed" || item.result == "blocked")
    }
}

impl ActionItem {
    pub fn new(
        action: &str,
        wt: &Worktree,
        reason: impl Into<String>,
        blocked_by: Option<String>,
        result: &str,
    ) -> Self {
        Self {
            action: action.to_string(),
            id: wt.id.clone(),
            path: wt.path.clone(),
            reason: reason.into(),
            blocked_by,
            result: result.to_string(),
        }
    }

    pub fn missing(action: &str, id: &str, path: &Path, label: &str) -> Self {
        Self {
            action: action.to_string(),
            id: id.to_string(),
            path: path.to_path_buf(),
            reason: format!("unknown worktree ({label})"),
            blocked_by: Some("not found".to_string()),
            result: "blocked".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::fixture;

    #[test]
    fn envelope_keeps_stable_keys() {
        let wt = fixture("/repos/demo/.worktrees/feat", "demo", false, Some(1));
        let report = Report::from_scan(
            &[PathBuf::from("/home/me/project")],
            ScanResult {
                repos: 1,
                worktrees: vec![wt.clone()],
                errors: vec!["missing: /gone".to_string()],
            },
        )
        .with_items(vec![ActionItem::new(
            "delete", &wt, "clean", None, "dry-run",
        )]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json["roots"].is_array());
        assert_eq!(json["scanned_repos"], 1);
        assert!(json["worktrees"].is_array());
        assert!(json["items"].is_array());
        assert!(json["errors"].is_array());
        assert_eq!(json["complete"], true);
        assert_eq!(json["scan_ms"], 0);
        assert_eq!(json["items"][0]["action"], "delete");
        assert_eq!(json["items"][0]["result"], "dry-run");
        assert_eq!(json["worktrees"][0]["id"], wt.id);
    }
}
