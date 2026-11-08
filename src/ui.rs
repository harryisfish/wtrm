use std::collections::{BTreeSet, HashSet};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::ops;
use crate::remove;
use crate::report::ActionItem;
use crate::scan::{self, Query, ScanResult, Worktree};
use crate::timeutil::{self, age_label};

pub struct Options {
    pub roots: Vec<PathBuf>,
    pub max_depth: u8,
    pub query: Query,
    pub inactive_secs: i64,
    pub stale_created_secs: i64,
    pub stale_idle_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Rescan,
    Delete {
        force: bool,
        branches: remove::BranchScope,
    },
    CleanDeps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Idle,
    Running { action: ActionKind, total: usize },
    Finished { succeeded: usize, failed: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Scan,
    Delete,
    Clean,
}

impl ActionKind {
    fn label(self) -> &'static str {
        match self {
            ActionKind::Scan => "scan",
            ActionKind::Delete => "delete",
            ActionKind::Clean => "clean",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pending {
    None,
    Delete,
    Clean,
}

pub struct App {
    pub items: Vec<Worktree>,
    pub repos: usize,
    pub errors: Vec<String>,
    pub roots_label: String,
    pub cursor: usize,
    pub selected: BTreeSet<PathBuf>,
    pub filter: String,
    pub filter_mode: bool,
    pub(crate) pending: Pending,
    pub confirm_buf: String,
    pub note: String,
    pub op: OperationStatus,
    pub last_result: Vec<ActionItem>,
    pub confirm_hint: String,
    pub now: i64,
    pub query: Query,
    pub inactive_secs: i64,
    pub stale_created_secs: i64,
    pub stale_idle_secs: i64,
    pub branch_scope: remove::BranchScope,
    visible: Vec<usize>,
}

impl App {
    pub fn from_scan(result: ScanResult, roots_label: String) -> Self {
        let errors = result.errors.clone();
        let mut app = Self {
            items: Vec::new(),
            repos: 0,
            errors: Vec::new(),
            roots_label,
            cursor: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filter_mode: false,
            pending: Pending::None,
            confirm_buf: String::new(),
            note: String::new(),
            op: OperationStatus::Idle,
            last_result: Vec::new(),
            confirm_hint: String::new(),
            now: timeutil::now_unix(),
            query: Query::default(),
            inactive_secs: 14 * 86_400,
            stale_created_secs: 30 * 86_400,
            stale_idle_secs: 7 * 86_400,
            branch_scope: remove::BranchScope::Keep,
            visible: Vec::new(),
        };
        app.apply_scan(result);
        app.note = if let Some(err) = errors.first() {
            format!("{} read errors. {err}", errors.len())
        } else {
            format!("scanned {}", app.roots_label)
        };
        app
    }

    pub fn apply_scan(&mut self, result: ScanResult) -> usize {
        let before = self.selected.len();
        self.repos = result.repos;
        self.errors = result.errors;
        self.items = result.worktrees;
        self.selected.retain(|path| {
            self.items
                .iter()
                .any(|wt| &wt.path == path && wt.deletable())
        });
        self.pending = Pending::None;
        self.confirm_buf.clear();
        self.now = timeutil::now_unix();
        self.clamp_cursor();
        before.saturating_sub(self.selected.len())
    }

    pub fn apply_mutate_result(&mut self, items: Vec<ActionItem>) {
        let gone: HashSet<PathBuf> = items
            .iter()
            .filter(|item| item.result == "ok")
            .map(|item| item.path.clone())
            .collect();
        self.items.retain(|wt| !gone.contains(&wt.path));
        let linked: HashSet<_> = self
            .items
            .iter()
            .filter(|wt| wt.deletable())
            .map(|wt| wt.git_common_dir.clone())
            .collect();
        self.items
            .retain(|wt| wt.deletable() || linked.contains(&wt.git_common_dir));
        self.selected.retain(|path| {
            self.items
                .iter()
                .any(|wt| &wt.path == path && wt.deletable())
        });
        self.repos = self
            .items
            .iter()
            .map(|wt| &wt.git_common_dir)
            .collect::<HashSet<_>>()
            .len();
        let (succeeded, failed) = ops::counts(&items);
        self.op = OperationStatus::Finished { succeeded, failed };
        self.note = ops::summarize_items(&items);
        self.last_result = items;
        self.pending = Pending::None;
        self.confirm_buf.clear();
        self.confirm_hint.clear();
        self.clamp_cursor();
    }

    pub fn status_text(&self) -> String {
        let op = match self.op {
            OperationStatus::Idle => None,
            OperationStatus::Running { action, total } => {
                Some(format!("{} {total}…", action.label()))
            }
            OperationStatus::Finished { succeeded, failed } => Some(if failed == 0 {
                format!("finished: {succeeded} succeeded")
            } else {
                format!("finished: {succeeded} succeeded, {failed} failed")
            }),
        };
        match (op, self.note.is_empty()) {
            (Some(op), true) => op,
            (Some(op), false) => format!("{op}. {}", self.note),
            (None, _) => self.note.clone(),
        }
    }

    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    fn scanning(&self) -> bool {
        matches!(
            self.op,
            OperationStatus::Running {
                action: ActionKind::Scan,
                ..
            }
        )
    }

    fn rebuild_visible(&mut self) {
        let query = self.filter.to_lowercase();
        self.visible = scan::matching_indexes(&self.items, &self.query, self.now)
            .into_iter()
            .filter(|idx| {
                let wt = &self.items[*idx];
                if query.is_empty() {
                    return true;
                }
                let blob = format!(
                    "{} {} {} {} {} {}",
                    wt.id,
                    wt.repo,
                    wt.branch,
                    wt.path.display(),
                    wt.parent.display(),
                    wt.flags()
                );
                blob.to_lowercase().contains(&query)
            })
            .collect();
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        let action = if self.filter_mode {
            self.on_filter_key(code)
        } else if self.pending != Pending::None {
            self.on_confirm_key(code)
        } else {
            self.on_browse_key(code)
        };
        self.clamp_cursor();
        action
    }

    fn on_filter_key(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filter_mode = false;
                self.rebuild_visible();
                self.note = format!("view changed: {} matches", self.visible.len());
            }
            KeyCode::Enter => {
                self.filter_mode = false;
                self.rebuild_visible();
                self.note = format!("view changed: {} matches", self.visible.len());
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(ch) => self.filter.push(ch),
            _ => {}
        }
        Action::None
    }

    fn on_confirm_key(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_pending(false),
            KeyCode::Char('f') | KeyCode::Char('F') if self.pending == Pending::Delete => {
                self.confirm_pending(true)
            }
            KeyCode::Char('b') if self.pending == Pending::Delete => {
                self.branch_scope = self.branch_scope.next();
                self.confirm_buf.clear();
                Action::None
            }
            KeyCode::Esc => {
                self.pending = Pending::None;
                self.confirm_buf.clear();
                Action::None
            }
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Backspace if self.pending == Pending::Delete => {
                self.confirm_buf.pop();
                Action::None
            }
            KeyCode::Enter if self.pending == Pending::Delete => {
                if self.confirm_buf.eq_ignore_ascii_case("delete") {
                    self.confirm_pending(false)
                } else {
                    self.confirm_hint =
                        "Enter does not delete. press y or type delete.".to_string();
                    Action::None
                }
            }
            KeyCode::Char(ch) if self.pending == Pending::Delete => {
                if ch.is_ascii_alphabetic() {
                    self.confirm_buf.push(ch);
                    if self.confirm_buf.eq_ignore_ascii_case("delete") {
                        return self.confirm_pending(false);
                    }
                    if self.confirm_buf.len() > 6 {
                        self.confirm_buf.clear();
                    }
                }
                Action::None
            }
            KeyCode::Enter => {
                self.confirm_hint = if self.pending == Pending::Clean {
                    "Enter does not clean. press y.".to_string()
                } else {
                    "Enter does not delete. press y or type delete.".to_string()
                };
                Action::None
            }
            _ => Action::None,
        }
    }

    fn confirm_pending(&mut self, force: bool) -> Action {
        let pending = self.pending;
        self.pending = Pending::None;
        self.confirm_buf.clear();
        match pending {
            Pending::Clean => Action::CleanDeps,
            Pending::Delete => Action::Delete {
                force,
                branches: self.branch_scope,
            },
            Pending::None => Action::None,
        }
    }

    fn on_browse_key(&mut self, code: KeyCode) -> Action {
        if self.scanning() && self.items.is_empty() {
            return match code {
                KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
                _ => Action::None,
            };
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = self.cursor.saturating_add(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::PageDown => {
                self.cursor = self.cursor.saturating_add(10);
                Action::None
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(10);
                Action::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.cursor = 0;
                Action::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.cursor = usize::MAX;
                Action::None
            }
            KeyCode::Char(' ') => {
                self.toggle_current();
                Action::None
            }
            KeyCode::Char('a') => {
                self.toggle_all_visible();
                Action::None
            }
            KeyCode::Char('m') => {
                self.apply_preset(Query {
                    merged_only: true,
                    ..Query::default()
                });
                Action::None
            }
            KeyCode::Char('i') => {
                self.apply_preset(Query {
                    inactive_for: Some(self.inactive_secs),
                    ..Query::default()
                });
                Action::None
            }
            KeyCode::Char('s') => {
                self.apply_preset(Query {
                    inactive_for: Some(self.stale_idle_secs),
                    created_before: Some(self.stale_created_secs),
                    merged_only: false,
                });
                Action::None
            }
            KeyCode::Char('0') => {
                self.query = Query::default();
                self.cursor = 0;
                self.rebuild_visible();
                self.note = format!("view changed: {} matches", self.visible.len());
                Action::None
            }
            KeyCode::Char('x') => {
                self.arm_clean();
                Action::None
            }
            KeyCode::Char('/') => {
                self.filter_mode = true;
                Action::None
            }
            KeyCode::Char('r') => Action::Rescan,
            KeyCode::Char('d') => {
                self.arm_delete();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn toggle_current(&mut self) {
        let Some(path) = self.current_path() else {
            return;
        };
        let deletable = self
            .items
            .iter()
            .find(|wt| wt.path == path)
            .is_some_and(|wt| wt.deletable());
        if !deletable {
            self.note = "the main checkout is only a reference and cannot be deleted".to_string();
            return;
        }
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
        self.note = format!("selection changed: {} selected", self.selected.len());
    }

    fn toggle_all_visible(&mut self) {
        let paths: Vec<PathBuf> = self
            .visible
            .iter()
            .filter_map(|&idx| {
                let wt = &self.items[idx];
                wt.deletable().then(|| wt.path.clone())
            })
            .collect();
        if paths.is_empty() {
            self.note = "nothing here can be deleted".to_string();
            return;
        }
        let all_on = paths.iter().all(|path| self.selected.contains(path));
        if all_on {
            for path in paths {
                self.selected.remove(&path);
            }
        } else {
            for path in paths {
                self.selected.insert(path);
            }
        }
        self.note = format!(
            "selection changed: {} selected (a toggles visible rows)",
            self.selected.len()
        );
    }

    fn arm_delete(&mut self) {
        if self.selected.is_empty() {
            self.toggle_current();
        }
        if self.selected.is_empty() {
            self.note = "select a worktree to delete".to_string();
            return;
        }
        self.pending = Pending::Delete;
        self.confirm_buf.clear();
        self.confirm_hint.clear();
        self.branch_scope = remove::BranchScope::Keep;
    }

    fn arm_clean(&mut self) {
        if self.selected.is_empty() {
            self.toggle_current();
        }
        if self.selected.is_empty() {
            self.note = "select a worktree to clean".to_string();
            return;
        }
        self.pending = Pending::Clean;
        self.confirm_buf.clear();
        self.confirm_hint.clear();
    }

    fn apply_preset(&mut self, query: Query) {
        self.query = query;
        self.filter.clear();
        self.filter_mode = false;
        self.pending = Pending::None;
        self.confirm_buf.clear();
        self.rebuild_visible();
        self.cursor = self
            .visible
            .iter()
            .position(|&idx| self.items[idx].deletable())
            .unwrap_or(0);
        self.note = format!(
            "view changed: {} matches. selection unchanged: {} selected",
            self.visible.len(),
            self.selected.len()
        );
    }

    fn current_path(&self) -> Option<PathBuf> {
        let idx = *self.visible.get(self.cursor)?;
        self.items.get(idx).map(|wt| wt.path.clone())
    }

    fn clamp_cursor(&mut self) {
        self.rebuild_visible();
        let len = self.visible.len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}

pub fn browse(opts: Options) -> anyhow::Result<()> {
    let roots_label = opts
        .roots
        .iter()
        .map(|path| scan::shorten_path(path))
        .collect::<Vec<_>>()
        .join(", ");
    if !io::stdout().is_terminal() {
        anyhow::bail!("stdout is not a terminal. use --list or --json");
    }
    let mut app = App::from_scan(ScanResult::default(), roots_label.clone());
    app.query = opts.query.clone();
    app.inactive_secs = opts.inactive_secs;
    app.stale_created_secs = opts.stale_created_secs;
    app.stale_idle_secs = opts.stale_idle_secs;
    app.op = OperationStatus::Running {
        action: ActionKind::Scan,
        total: 0,
    };
    app.note = format!("scanning {roots_label} …");
    let mut terminal = ratatui::try_init().context("cannot enter the terminal ui")?;
    let _restore = Restore;
    let mut scan_rx = Some(spawn_scan(&opts.roots, opts.max_depth));
    let mut scan_started = Instant::now();
    let mut last_tick = Instant::now();
    let mut need_draw = true;
    loop {
        if let Some(rx) = scan_rx.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    let count = result.worktrees.len();
                    app.query = opts.query.clone();
                    let pruned = app.apply_scan(result);
                    app.op = OperationStatus::Idle;
                    app.note = if pruned > 0 {
                        format!("scanned {count} worktrees. selection pruned: {pruned}")
                    } else {
                        format!("scanned {roots_label}, {count} worktrees")
                    };
                    scan_rx = None;
                    need_draw = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.op = OperationStatus::Idle;
                    app.note = "scan failed".to_string();
                    scan_rx = None;
                    need_draw = true;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if last_tick.elapsed() >= Duration::from_millis(200) {
                        app.note = format!(
                            "scanning {roots_label} … {:.1}s",
                            scan_started.elapsed().as_secs_f32()
                        );
                        last_tick = Instant::now();
                        need_draw = true;
                    }
                }
            }
        }
        if need_draw {
            terminal.draw(|frame| draw(frame, &app))?;
            need_draw = false;
        }
        let wait = if scan_rx.is_some() {
            Duration::from_millis(33)
        } else {
            Duration::from_secs(30)
        };
        if !event::poll(wait)? {
            continue;
        }
        loop {
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Resize(_, _) => {
                    need_draw = true;
                    break;
                }
                _ => break,
            };
            if key.kind == KeyEventKind::Release {
                if !event::poll(Duration::ZERO)? {
                    break;
                }
                continue;
            }
            match app.on_key(key.code, key.modifiers) {
                Action::None => {}
                Action::Quit => return Ok(()),
                Action::Rescan => {
                    if scan_rx.is_none() {
                        app.op = OperationStatus::Running {
                            action: ActionKind::Scan,
                            total: 0,
                        };
                        app.note = "rescanning…".to_string();
                        scan_started = Instant::now();
                        last_tick = Instant::now();
                        scan_rx = Some(spawn_scan(&opts.roots, opts.max_depth));
                    }
                }
                Action::Delete { force, branches } => {
                    if scan_rx.is_some() {
                        app.note = "wait for scan to finish".to_string();
                    } else {
                        run_delete(&mut terminal, &mut app, force, branches)?;
                    }
                }
                Action::CleanDeps => {
                    if scan_rx.is_some() {
                        app.note = "wait for scan to finish".to_string();
                    } else {
                        run_clean(&mut terminal, &mut app)?;
                    }
                }
            }
            need_draw = true;
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

fn spawn_scan(roots: &[PathBuf], max_depth: u8) -> Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    let roots = roots.to_vec();
    thread::spawn(move || {
        let _ = tx.send(scan::scan(&roots, max_depth));
    });
    rx
}

fn run_delete(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    force: bool,
    branches: remove::BranchScope,
) -> anyhow::Result<()> {
    let paths: Vec<_> = app.selected.iter().cloned().collect();
    let mut all = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        app.op = OperationStatus::Running {
            action: ActionKind::Delete,
            total: paths.len(),
        };
        app.note = format!(
            "deleting {}/{}  {}",
            i + 1,
            paths.len(),
            scan::shorten_path(path)
        );
        terminal.draw(|frame| draw(frame, app))?;
        all.extend(ops::delete_targets(
            &app.items,
            std::slice::from_ref(path),
            &[],
            true,
            force,
            branches,
        ));
    }
    app.apply_mutate_result(all);
    Ok(())
}

fn run_clean(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let paths: Vec<_> = app.selected.iter().cloned().collect();
    let mut all = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        app.op = OperationStatus::Running {
            action: ActionKind::Clean,
            total: paths.len(),
        };
        app.note = format!(
            "cleaning {}/{}  {}",
            i + 1,
            paths.len(),
            scan::shorten_path(path)
        );
        terminal.draw(|frame| draw(frame, app))?;
        all.extend(ops::clean_targets(
            &app.items,
            std::slice::from_ref(path),
            &[],
            true,
        ));
    }
    app.apply_mutate_result(all);
    Ok(())
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let extra = if app.last_result.is_empty() {
        0
    } else {
        (app.last_result.len().min(4) as u16).saturating_add(1)
    };
    let foot_h = match app.pending {
        Pending::None if app.scanning() && app.items.is_empty() => 4 + extra,
        Pending::None if app.filter_mode => 7 + extra,
        Pending::None => 11 + extra,
        _ => 14,
    }
    .min(frame.area().height.saturating_sub(5));
    let chunks =
        Layout::vertical([Constraint::Min(3), Constraint::Length(foot_h)]).split(frame.area());
    draw_table(frame, app, chunks[0]);
    draw_footer(frame, app, chunks[1]);
}

struct ColumnSpec {
    headers: Vec<&'static str>,
    widths: Vec<Constraint>,
    show_folder: bool,
    show_path: bool,
}

fn column_spec(width: u16) -> ColumnSpec {
    if width >= 120 {
        ColumnSpec {
            headers: vec![
                "", "id", "active", "repo", "branch", "folder", "status", "path",
            ],
            widths: vec![
                Constraint::Length(2),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(14),
                Constraint::Length(16),
                Constraint::Length(20),
                Constraint::Length(22),
                Constraint::Min(16),
            ],
            show_folder: true,
            show_path: true,
        }
    } else {
        ColumnSpec {
            headers: vec!["", "id", "repo", "branch", "age", "status"],
            widths: vec![
                Constraint::Length(2),
                Constraint::Length(8),
                Constraint::Length(if width < 80 { 8 } else { 14 }),
                Constraint::Length(if width < 80 { 10 } else { 16 }),
                Constraint::Length(6),
                Constraint::Min(16),
            ],
            show_folder: false,
            show_path: false,
        }
    }
}

fn draw_table(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let visible = app.visible();
    let shown = if app.filter.is_empty() && app.query.is_empty() {
        format!("{}", app.items.len())
    } else {
        format!("{}/{}", visible.len(), app.items.len())
    };
    let title = format!(
        " wtrm  {shown} worktrees · {} selected · {} repos ",
        app.selected.len(),
        app.repos
    );
    let spec = column_spec(area.width);
    let header = Row::new(spec.headers.clone()).style(Style::new().add_modifier(Modifier::BOLD));
    let rows_h = area.height.saturating_sub(3) as usize;
    let len = visible.len();
    let (start, end) = if len == 0 || rows_h == 0 {
        (0, 0)
    } else {
        let cursor = app.cursor.min(len - 1);
        let start = cursor.saturating_sub(rows_h.saturating_sub(1));
        let end = (start + rows_h).min(len);
        (start, end)
    };
    let rows: Vec<Row> = visible[start..end]
        .iter()
        .map(|&idx| {
            let wt = &app.items[idx];
            let mark = if app.selected.contains(&wt.path) {
                "●"
            } else if wt.deletable() {
                " "
            } else {
                "·"
            };
            let mut cells = vec![Cell::from(mark), Cell::from(wt.id_short())];
            if spec.show_path || spec.show_folder {
                cells.push(Cell::from(age_label(wt.last_active, app.now)));
                cells.push(Cell::from(wt.repo.as_str()));
                cells.push(Cell::from(wt.branch.as_str()));
                if spec.show_folder {
                    cells.push(Cell::from(scan::shorten_path(&wt.parent)));
                }
                cells.push(Cell::from(wt.flags()));
                if spec.show_path {
                    cells.push(Cell::from(scan::shorten_path(&wt.path)));
                }
            } else {
                cells.push(Cell::from(wt.repo.as_str()));
                cells.push(Cell::from(wt.branch.as_str()));
                cells.push(Cell::from(age_label(wt.last_active, app.now)));
                cells.push(Cell::from(wt.flags()));
            }
            Row::new(cells).style(row_style(wt, app.now))
        })
        .collect();
    let table = Table::new(rows, spec.widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");
    let mut state = TableState::default();
    if end > start {
        state.select(Some(app.cursor.min(len - 1) - start));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn row_style(wt: &Worktree, now: i64) -> Style {
    if wt.main || wt.bare {
        return Style::new().fg(Color::DarkGray);
    }
    if wt.missing || wt.dirty == Some(true) || wt.locked {
        return Style::new().fg(Color::Yellow);
    }
    if wt.merged == Some(true) {
        return Style::new().fg(Color::Cyan);
    }
    match wt.last_active {
        Some(ts) if now.saturating_sub(ts) >= 86_400 * 14 => Style::new().fg(Color::Red),
        Some(_) => Style::new(),
        None => Style::new().fg(Color::Red),
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let lines = match app.pending {
        Pending::Delete => confirm_lines(app),
        Pending::Clean => clean_lines(app),
        Pending::None if app.filter_mode => filter_lines(app),
        Pending::None => browse_lines(app),
    };
    let title = match app.pending {
        Pending::Delete => " confirm delete ",
        Pending::Clean => " confirm clean ",
        Pending::None if app.filter_mode => " filter ",
        Pending::None if !app.last_result.is_empty() => " result ",
        Pending::None => " help ",
    };
    let hints = match app.pending {
        Pending::Delete => {
            let typed = if app.confirm_buf.is_empty() {
                "type y or delete to confirm".to_string()
            } else {
                format!("typed: {}_", app.confirm_buf)
            };
            vec![
                Line::from("y delete clean worktrees"),
                Line::from("f force delete dirty or locked worktrees"),
                Line::from("b branch policy: keep → local → GitHub"),
                Line::from(format!("{typed}    Esc cancel")),
                Line::from("Enter does nothing"),
            ]
        }
        Pending::Clean => vec![Line::from("y confirm    Esc cancel    Enter does nothing")],
        Pending::None if app.filter_mode => vec![Line::from("Enter apply    Esc clear")],
        Pending::None if app.scanning() && app.items.is_empty() => {
            vec![Line::from("q quit    scanning…")]
        }
        Pending::None => vec![
            Line::from("j/k move   space select   a toggle visible"),
            Line::from("d delete   x clean deps   / filter   r refresh   q quit"),
            Line::from("m merged   i idle   s old+idle   0 all"),
        ],
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let browsing = app.pending == Pending::None
        && !app.filter_mode
        && !(app.scanning() && app.items.is_empty());
    let sections = Layout::vertical([
        Constraint::Length(u16::from(app.pending == Pending::None)),
        Constraint::Length(u16::from(browsing)),
        Constraint::Min(0),
        Constraint::Length(hints.len() as u16),
    ])
    .split(inner);
    if app.pending == Pending::None {
        frame.render_widget(Paragraph::new(app.status_text()), sections[0]);
    }
    if browsing {
        let text_filter = if app.filter.is_empty() {
            String::new()
        } else {
            format!(" · text: {}", app.filter)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("filter {}{text_filter}", app.query.label()),
                Style::new().fg(Color::DarkGray),
            ))),
            sections[1],
        );
    }
    let paragraph = if app.pending == Pending::None && app.filter_mode {
        let hidden = lines[0].width().saturating_sub(sections[2].width as usize);
        let mut scroll = 0;
        // A partially clipped wide grapheme can push the caret out of view.
        for grapheme in lines[0].styled_graphemes(Style::new()) {
            if scroll >= hidden {
                break;
            }
            scroll += Line::from(grapheme.symbol).width();
        }
        Paragraph::new(lines).scroll((0, scroll.min(u16::MAX as usize) as u16))
    } else {
        Paragraph::new(lines).wrap(Wrap { trim: false })
    };
    frame.render_widget(paragraph, sections[2]);
    frame.render_widget(Paragraph::new(hints), sections[3]);
}

fn filter_lines(app: &App) -> Vec<Line<'static>> {
    vec![Line::from(format!("filter: {}_", app.filter))]
}

fn browse_lines(app: &App) -> Vec<Line<'static>> {
    if app.scanning() && app.items.is_empty() {
        return Vec::new();
    }
    let detail = match app
        .visible()
        .get(app.cursor)
        .and_then(|&idx| app.items.get(idx))
    {
        Some(wt) => {
            let when = wt
                .last_active_utc
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let created = wt
                .created_utc
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "id {}  ·  {}  ·  {}  ·  active {when}  ·  created {created}  ·  {}",
                wt.id,
                scan::shorten_path(&wt.path),
                wt.branch,
                wt.flags()
            )
        }
        None if app.items.is_empty() => "no repos with linked worktrees".to_string(),
        None => "no worktrees match the current filters".to_string(),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            "merged means the commit is in the local default branch.",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "squash merges are not detected.",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    lines.extend(result_lines(app));
    lines.push(Line::from(detail));
    lines
}

fn result_lines(app: &App) -> Vec<Line<'static>> {
    if app.last_result.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::from(Span::styled(
        "last result:",
        Style::new().add_modifier(Modifier::BOLD),
    ))];
    for text in ops::result_lines(&app.last_result, 4) {
        lines.push(Line::from(text));
    }
    lines
}

fn clean_lines(app: &App) -> Vec<Line<'static>> {
    let chosen: Vec<&Worktree> = selected_worktrees(app);
    let mut lines = vec![
        Line::from(format!(
            "clean dependency directories in {} worktrees. the worktrees stay.",
            chosen.len()
        )),
        Line::from(
            "removes node_modules, target, .next, .turbo, .venv, venv, __pycache__, Pods, .gradle.",
        ),
    ];
    if !app.confirm_hint.is_empty() {
        lines.push(Line::from(app.confirm_hint.clone()));
    }
    for wt in chosen.iter().take(6) {
        lines.push(Line::from(format!(
            "  {}  {}  {}",
            wt.id_short(),
            wt.branch,
            scan::shorten_path(&wt.path)
        )));
    }
    if chosen.len() > 6 {
        lines.push(Line::from(format!("  …{} more", chosen.len() - 6)));
    }
    lines
}

fn selected_worktrees(app: &App) -> Vec<&Worktree> {
    app.selected
        .iter()
        .filter_map(|path| app.items.iter().find(|wt| &wt.path == path))
        .collect()
}

fn confirm_lines(app: &App) -> Vec<Line<'static>> {
    let chosen = selected_worktrees(app);
    let mut lines = vec![
        Line::from(format!("delete {} worktrees", chosen.len())),
        Line::from(format!("branch policy: {}", app.branch_scope.label())),
    ];
    if !app.confirm_hint.is_empty() {
        lines.push(Line::from(app.confirm_hint.clone()));
    }
    for wt in chosen.iter().take(5) {
        lines.push(Line::from(format!(
            "  {}  {}  {}  {}",
            wt.id_short(),
            wt.flags(),
            wt.branch,
            scan::shorten_path(&wt.path)
        )));
    }
    if chosen.len() > 5 {
        lines.push(Line::from(format!("  …{} more", chosen.len() - 5)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::fixture;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with(items: Vec<Worktree>) -> App {
        App::from_scan(
            ScanResult {
                repos: 1,
                worktrees: items,
                errors: Vec::new(),
            },
            "~/project".to_string(),
        )
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| {
                let mut line = String::new();
                let mut skip = 0;
                for cell in row {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    line.push_str(cell.symbol());
                    skip = Line::from(cell.symbol()).width().saturating_sub(1);
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn long_path_app() -> App {
        app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture(
                &format!("/repos/{}/feature", "long-directory/".repeat(24)),
                "demo",
                false,
                Some(10),
            ),
        ])
    }

    #[test]
    fn browsing_keeps_all_shortcuts_visible_with_long_details() {
        let mut app = long_path_app();
        app.items[1].dirty = Some(true);
        app.items[1].locked = true;
        app.items[1].merged = Some(true);
        app.query.merged_only = true;
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        for (width, height) in [(120, 30), (80, 24), (60, 18)] {
            let screen = render(&app, width, height);
            for hint in [
                "j/k move",
                "space select",
                "a toggle visible",
                "d delete",
                "x clean deps",
                "/ filter",
                "r refresh",
                "q quit",
                "m merged",
                "i idle",
                "s old+idle",
                "0 all",
                "filter merged",
                "merged means the commit is in the local default branch.",
                "squash merges are not detected.",
            ] {
                assert!(
                    screen.contains(hint),
                    "{width}x{height} missing {hint}:\n{screen}"
                );
            }
            assert!(screen.contains(&app.items[1].id), "{screen}");
            assert!(screen.contains("[dirty] [locked]"), "{screen}");
            assert!(screen.contains("scanned ~/project"), "{screen}");
            let header = screen.lines().nth(1).unwrap();
            assert_eq!(header.contains("folder"), width >= 120, "{screen}");
            assert_eq!(header.contains("path"), width >= 120, "{screen}");
        }
    }

    #[test]
    fn empty_and_filter_states_keep_their_recovery_keys_visible() {
        for (width, height) in [(120, 30), (80, 24), (60, 18)] {
            let mut app = app_with(Vec::new());
            let screen = render(&app, width, height);
            assert!(
                screen.contains("no repos with linked worktrees"),
                "{screen}"
            );
            assert!(screen.contains("r refresh"), "{screen}");
            assert_eq!(
                app.on_key(KeyCode::Char('r'), KeyModifiers::NONE),
                Action::Rescan
            );

            app.op = OperationStatus::Running {
                action: ActionKind::Scan,
                total: 0,
            };
            app.note = "scanning ~/project …".to_string();
            let screen = render(&app, width, height);
            assert!(screen.contains("scanning ~/project"), "{screen}");
            assert!(screen.contains("q quit"), "{screen}");
            assert!(
                !screen.contains("no repos with linked worktrees"),
                "{screen}"
            );

            let mut app = long_path_app();
            app.filter = "no-such-worktree".to_string();
            app.clamp_cursor();
            let screen = render(&app, width, height);
            assert!(
                screen.contains("no worktrees match the current filters"),
                "{screen}"
            );
            assert!(
                !screen.contains("no repos with linked worktrees"),
                "{screen}"
            );
            assert!(screen.contains("/ filter"), "{screen}");
            assert!(screen.contains("0 all"), "{screen}");
            assert!(screen.contains("text: no-such-worktree"), "{screen}");

            app.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
            app.filter = format!("{}终点é-tail", "长过滤/".repeat(64));
            let screen = render(&app, width, height);
            assert!(screen.contains("终点é-tail_"), "{screen}");
            assert!(!screen.contains('\u{fffd}'), "{screen}");
            assert!(screen.contains("Enter apply    Esc clear"), "{screen}");
        }
    }

    #[test]
    fn confirmation_keeps_safety_and_cancel_visible() {
        let mut app = long_path_app();
        app.items[1].dirty = Some(true);
        app.items[1].locked = true;
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        for scope in [
            remove::BranchScope::Keep,
            remove::BranchScope::Local,
            remove::BranchScope::GitHub,
        ] {
            assert_eq!(app.branch_scope, scope);
            for (width, height) in [(120, 30), (80, 24), (60, 18)] {
                let screen = render(&app, width, height);
                for hint in [
                    "confirm delete",
                    "delete 1 worktrees",
                    "y delete clean worktrees",
                    "f force delete dirty or locked worktrees",
                    "b branch policy: keep → local → GitHub",
                    "type y or delete to confirm",
                    "Esc cancel",
                    "Enter does nothing",
                ] {
                    assert!(
                        screen.contains(hint),
                        "{width}x{height} missing {hint}:\n{screen}"
                    );
                }
                assert!(
                    screen.contains(&format!("branch policy: {}", scope.label())),
                    "{screen}"
                );
            }
            assert_eq!(
                app.on_key(KeyCode::Char('b'), KeyModifiers::NONE),
                Action::None
            );
        }
        assert_eq!(app.on_key(KeyCode::Enter, KeyModifiers::NONE), Action::None);
        assert_eq!(app.pending, Pending::Delete);
        assert!(render(&app, 60, 18).contains("Enter does not delete"));
        assert_eq!(app.on_key(KeyCode::Esc, KeyModifiers::NONE), Action::None);
        assert_eq!(app.pending, Pending::None);

        app.on_key(KeyCode::Char('x'), KeyModifiers::NONE);
        for (width, height) in [(120, 30), (80, 24), (60, 18)] {
            let screen = render(&app, width, height);
            for hint in [
                "confirm clean",
                "the worktrees",
                "stay.",
                "y confirm",
                "Esc cancel",
                "Enter does nothing",
            ] {
                assert!(
                    screen.contains(hint),
                    "{width}x{height} missing {hint}:\n{screen}"
                );
            }
        }
        for ch in "delete".chars() {
            assert_eq!(
                app.on_key(KeyCode::Char(ch), KeyModifiers::NONE),
                Action::None
            );
        }
        assert_eq!(app.pending, Pending::Clean);
        assert_eq!(app.on_key(KeyCode::Esc, KeyModifiers::NONE), Action::None);
        assert_eq!(app.pending, Pending::None);
    }

    #[test]
    fn long_result_paths_do_not_hide_status_or_shortcuts() {
        let mut app = long_path_app();
        let wt = app.items[1].clone();
        app.apply_mutate_result(vec![ActionItem::new(
            "delete",
            &wt,
            "dirty",
            Some("dirty".to_string()),
            "blocked",
        )]);
        for (width, height) in [(120, 30), (80, 24), (60, 18)] {
            let screen = render(&app, width, height);
            for hint in [
                "finished: 0 succeeded, 1 failed",
                "last result:",
                "blocked",
                "j/k move",
                "r refresh",
                "q quit",
            ] {
                assert!(
                    screen.contains(hint),
                    "{width}x{height} missing {hint}:\n{screen}"
                );
            }
        }
        assert_eq!(app.items.len(), 2);
    }

    #[test]
    fn space_selects_linked_and_skips_main() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/feat", "demo", false, Some(10)),
        ]);
        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(app.selected.is_empty());
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(app.selected.len(), 1);
        assert!(app.note.contains("selection changed"));
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.pending, Pending::Delete);
        assert_eq!(app.on_key(KeyCode::Enter, KeyModifiers::NONE), Action::None);
        assert_eq!(
            app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::Keep,
            }
        );
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(
            app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::GitHub,
            }
        );
    }

    #[test]
    fn typing_delete_confirms() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/feat", "demo", false, Some(10)),
        ]);
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(
            app.on_key(KeyCode::Char('d'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyCode::Char('e'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyCode::Char('l'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyCode::Char('e'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyCode::Char('t'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyCode::Char('e'), KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::Keep,
            }
        );
    }

    #[test]
    fn presets_filter_without_selecting() {
        let now = 1_000_000;
        let mut merged = fixture("/repos/demo/.worktrees/merged", "demo", false, Some(now));
        merged.merged = Some(true);
        merged.branch = "merged".to_string();
        merged.id = scan::stable_id("demo", "merged", &merged.path);
        let mut idle = fixture(
            "/repos/demo/.worktrees/idle",
            "demo",
            false,
            Some(now - 20 * 86_400),
        );
        idle.created_at = Some(now - 2 * 86_400);
        idle.branch = "idle".to_string();
        idle.id = scan::stable_id("demo", "idle", &idle.path);
        let mut old = fixture(
            "/repos/demo/.worktrees/old",
            "demo",
            false,
            Some(now - 20 * 86_400),
        );
        old.created_at = Some(now - 40 * 86_400);
        old.branch = "old".to_string();
        old.id = scan::stable_id("demo", "old", &old.path);
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(now)),
            merged,
            idle,
            old,
        ]);
        app.now = now;
        app.inactive_secs = 14 * 86_400;
        app.stale_created_secs = 30 * 86_400;
        app.stale_idle_secs = 7 * 86_400;

        app.on_key(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(app.selected.is_empty());
        assert!(app.query.merged_only);
        assert!(app.note.contains("view changed"));
        assert_eq!(app.visible().len(), 2);

        app.on_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(app.selected.is_empty());
        assert_eq!(
            app.visible()
                .iter()
                .filter(|&&i| app.items[i].deletable())
                .count(),
            2
        );

        app.on_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(app.selected.is_empty());
        let deletable: Vec<_> = app
            .visible()
            .iter()
            .copied()
            .filter(|&i| app.items[i].deletable())
            .map(|i| app.items[i].path.clone())
            .collect();
        assert_eq!(deletable, vec![PathBuf::from("/repos/demo/.worktrees/old")]);

        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(app.selected.len(), 1);
        app.on_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.pending, Pending::Clean);
        assert_eq!(app.on_key(KeyCode::Enter, KeyModifiers::NONE), Action::None);
        assert_eq!(
            app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Action::CleanDeps
        );
    }

    #[test]
    fn select_all_then_force() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/a", "demo", false, Some(1)),
            fixture("/repos/demo/.worktrees/b", "demo", false, Some(2)),
        ]);
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(app.selected.len(), 2);
        assert!(app.note.contains("selection changed"));
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(
            app.on_key(KeyCode::Char('f'), KeyModifiers::NONE),
            Action::Delete {
                force: true,
                branches: remove::BranchScope::Keep,
            }
        );
    }

    #[test]
    fn refresh_reports_pruned_selection() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/a", "demo", false, Some(1)),
            fixture("/repos/demo/.worktrees/b", "demo", false, Some(2)),
        ]);
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        let pruned = app.apply_scan(ScanResult {
            repos: 1,
            worktrees: vec![
                fixture("/repos/demo", "demo", true, Some(10)),
                fixture("/repos/demo/.worktrees/a", "demo", false, Some(1)),
            ],
            errors: Vec::new(),
        });
        assert_eq!(pruned, 1);
        assert_eq!(app.selected.len(), 1);
    }

    #[test]
    fn narrow_columns_hide_folder_and_path() {
        let narrow = column_spec(80);
        assert!(!narrow.show_folder);
        assert!(!narrow.show_path);
        assert!(narrow.headers.contains(&"status"));
        assert!(narrow.headers.contains(&"repo"));
        let wide = column_spec(140);
        assert!(wide.show_folder);
        assert!(wide.show_path);
    }

    #[test]
    fn enter_explains_it_does_not_delete() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/feat", "demo", false, Some(10)),
        ]);
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.on_key(KeyCode::Enter, KeyModifiers::NONE), Action::None);
        assert!(app.confirm_hint.contains("Enter does not delete"));
        assert_eq!(app.pending, Pending::Delete);
    }

    #[test]
    fn mutate_result_drops_deleted_rows_and_keeps_feedback() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/feat", "demo", false, Some(10)),
        ]);
        let feat = app.items[1].clone();
        app.apply_mutate_result(vec![ActionItem::new("delete", &feat, "clean", None, "ok")]);
        assert!(
            app.items.is_empty(),
            "main should drop when no linked trees remain: {:?}",
            app.items
                .iter()
                .map(|wt| wt.path.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(app.last_result.len(), 1);
        assert!(app.note.contains("succeeded"));
        assert_eq!(
            app.op,
            OperationStatus::Finished {
                succeeded: 1,
                failed: 0
            }
        );
    }

    #[test]
    fn blocked_result_keeps_rows_and_names_the_path() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/feat", "demo", false, Some(10)),
        ]);
        let feat = app.items[1].clone();
        app.apply_mutate_result(vec![ActionItem::new(
            "delete",
            &feat,
            "dirty",
            Some("dirty".to_string()),
            "blocked",
        )]);
        assert_eq!(app.items.len(), 2);
        assert!(app.note.contains("blocked"));
        assert!(app.note.contains("dirty"));
        assert_eq!(app.last_result[0].result, "blocked");
    }

    #[test]
    fn empty_scan_only_accepts_quit() {
        let mut app = app_with(Vec::new());
        app.op = OperationStatus::Running {
            action: ActionKind::Scan,
            total: 0,
        };
        assert_eq!(
            app.on_key(KeyCode::Char('d'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyCode::Char('j'), KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(app.pending, Pending::None);
        assert_eq!(
            app.on_key(KeyCode::Char('q'), KeyModifiers::NONE),
            Action::Quit
        );
    }

    #[test]
    fn rescan_with_rows_still_selects() {
        let mut app = app_with(vec![
            fixture("/repos/demo", "demo", true, Some(10)),
            fixture("/repos/demo/.worktrees/feat", "demo", false, Some(10)),
        ]);
        app.op = OperationStatus::Running {
            action: ActionKind::Scan,
            total: 0,
        };
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(app.selected.len(), 1);
    }
}
