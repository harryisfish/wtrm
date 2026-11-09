use std::collections::{BTreeSet, HashSet};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};
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
    Delete { force: bool },
    Clean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Browse,
    Detail,
    Selected,
    Results,
    Help,
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
    view: View,
    scroll: usize,
    viewport: Rect,
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
            view: View::Browse,
            scroll: 0,
            viewport: Rect::new(0, 0, 80, 24),
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
        self.scroll = 0;
        self.now = timeutil::now_unix();
        self.clamp_cursor();
        before.saturating_sub(self.selected.len())
    }

    pub fn apply_mutate_result(&mut self, items: Vec<ActionItem>) {
        let gone: HashSet<PathBuf> = items
            .iter()
            .filter(|item| item.action == "delete" && item.result == "ok")
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
        self.note.clear();
        self.last_result = items;
        self.pending = Pending::None;
        self.confirm_buf.clear();
        self.confirm_hint.clear();
        self.view = View::Results;
        self.scroll = 0;
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
                format!("finished: {succeeded} succeeded, {failed} failed or blocked")
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
        } else if self.view != View::Browse {
            self.on_view_key(code)
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
        if self.scroll_key(code) {
            return Action::None;
        }
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if self.high_risk() {
                    self.confirm_hint = "High risk: type delete, then press Enter.".to_string();
                    Action::None
                } else {
                    self.confirm_pending()
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if matches!(self.pending, Pending::Delete { .. }) =>
            {
                if let Pending::Delete { force } = &mut self.pending {
                    *force = !*force;
                }
                self.confirm_buf.clear();
                self.confirm_hint.clear();
                self.scroll = 0;
                Action::None
            }
            KeyCode::Char('b') if matches!(self.pending, Pending::Delete { .. }) => {
                self.branch_scope = self.branch_scope.next();
                self.confirm_buf.clear();
                self.confirm_hint.clear();
                self.scroll = 0;
                Action::None
            }
            KeyCode::Esc => {
                self.pending = Pending::None;
                self.confirm_buf.clear();
                self.scroll = 0;
                Action::None
            }
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Backspace if matches!(self.pending, Pending::Delete { .. }) => {
                self.confirm_buf.pop();
                Action::None
            }
            KeyCode::Enter if matches!(self.pending, Pending::Delete { .. }) => {
                if self.confirm_buf.eq_ignore_ascii_case("delete") {
                    self.confirm_pending()
                } else {
                    self.confirm_hint = "Type delete, then press Enter.".to_string();
                    Action::None
                }
            }
            KeyCode::Char(ch) if matches!(self.pending, Pending::Delete { .. }) => {
                if ch.is_ascii_alphabetic() && self.confirm_buf.len() < 6 {
                    self.confirm_buf.push(ch);
                }
                Action::None
            }
            KeyCode::Enter => {
                self.confirm_hint = "Enter does not clean. Press y.".to_string();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn high_risk(&self) -> bool {
        matches!(self.pending, Pending::Delete { force: true })
            || (matches!(self.pending, Pending::Delete { .. })
                && self.branch_scope != remove::BranchScope::Keep)
    }

    fn confirm_pending(&mut self) -> Action {
        if !confirmation_fits(self) {
            self.confirm_hint = "Resize terminal to review and confirm.".to_string();
            return Action::None;
        }
        let pending = self.pending;
        self.pending = Pending::None;
        self.confirm_buf.clear();
        self.view = View::Browse;
        self.scroll = 0;
        match pending {
            Pending::Clean => Action::CleanDeps,
            Pending::Delete { force } => Action::Delete {
                force,
                branches: self.branch_scope,
            },
            Pending::None => Action::None,
        }
    }

    fn scroll_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = usize::MAX,
            _ => return false,
        }
        true
    }

    fn on_view_key(&mut self, code: KeyCode) -> Action {
        if self.scroll_key(code) {
            return Action::None;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.open_view(View::Browse),
            KeyCode::Char('c') if self.view == View::Selected => self.clear_selection(),
            KeyCode::Char('d') if self.view == View::Selected => self.arm_delete(),
            KeyCode::Char('x') if self.view == View::Selected => self.arm_clean(),
            _ => {}
        }
        Action::None
    }

    fn open_view(&mut self, view: View) {
        self.view = view;
        self.scroll = 0;
    }

    fn clear_selection(&mut self) {
        self.selected.clear();
        self.scroll = 0;
        self.note = "selection cleared: 0 selected".to_string();
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
            KeyCode::Char('c') => {
                self.clear_selection();
                Action::None
            }
            KeyCode::Char('v') => {
                self.open_view(View::Selected);
                Action::None
            }
            KeyCode::Char('R') => {
                self.open_view(View::Results);
                Action::None
            }
            KeyCode::Char('?') => {
                self.open_view(View::Help);
                Action::None
            }
            KeyCode::Enter => {
                self.open_view(View::Detail);
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
                self.filter.clear();
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
        if self.selected.is_empty() && self.view == View::Browse {
            self.toggle_current();
        }
        if self.selected.is_empty() {
            self.note = "select a worktree to delete".to_string();
            return;
        }
        self.pending = Pending::Delete { force: false };
        self.confirm_buf.clear();
        self.confirm_hint.clear();
        self.branch_scope = remove::BranchScope::Keep;
        self.scroll = 0;
    }

    fn arm_clean(&mut self) {
        if self.selected.is_empty() && self.view == View::Browse {
            self.toggle_current();
        }
        if self.selected.is_empty() {
            self.note = "select a worktree to clean".to_string();
            return;
        }
        self.pending = Pending::Clean;
        self.confirm_buf.clear();
        self.confirm_hint.clear();
        self.scroll = 0;
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
            terminal.draw(|frame| draw(frame, &mut app))?;
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
            match event::read()? {
                Event::Resize(width, height) => {
                    app.viewport = Rect::new(0, 0, width, height);
                    need_draw = true;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
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
                }
                _ => {}
            }
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
            None,
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

fn draw(frame: &mut Frame, app: &mut App) {
    app.viewport = frame.area();
    if app.pending != Pending::None || app.view != View::Browse {
        draw_panel(frame, app);
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .split(frame.area());
    let filter = if app.filter_mode {
        format!("filter edit: {}_", app.filter)
    } else {
        format!("filter: {} | text: {:?}", app.query.label(), app.filter)
    };
    let header = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(chunks[0]);
    frame.render_widget(Paragraph::new(selection_text(app)), header[0]);
    let filter = Line::from(filter);
    let mut scroll = 0;
    if app.filter_mode {
        let hidden = filter.width().saturating_sub(header[1].width as usize);
        // Round past a clipped wide grapheme so the caret stays visible.
        for grapheme in filter.styled_graphemes(Style::new()) {
            if scroll >= hidden {
                break;
            }
            scroll += Line::from(grapheme.symbol).width();
        }
    }
    frame.render_widget(
        Paragraph::new(filter).scroll((0, scroll.min(u16::MAX as usize) as u16)),
        header[1],
    );
    draw_table(frame, app, chunks[1]);
    let lines = if app.filter_mode {
        vec![
            Line::from("Enter apply · Esc clear text"),
            Line::from("Text and preset filters both apply."),
            Line::from(app.status_text()),
        ]
    } else if app.scanning() && app.items.is_empty() {
        vec![Line::from("q quit"), Line::from(app.status_text())]
    } else {
        vec![
            Line::from("j/k move · space select · a visible · v selected · c clear"),
            Line::from("d delete · x clean deps · Enter details · R results · ? help"),
            Line::from("m/i/s presets · 0 clear · / filter · r refresh · q quit"),
            Line::from(app.status_text()),
        ]
    };
    frame.render_widget(Paragraph::new(lines), chunks[2]);
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
            headers: vec!["", "id", "repo", "branch", "active", "status"],
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
    let title = format!(" wtrm  {shown} worktrees · {} repos ", app.repos);
    if visible.is_empty() {
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(wrap_lines(vec![Line::from(empty_text(app))], inner.width)),
            inner,
        );
        return;
    }
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
        None => Style::new(),
    }
}

fn selection_text(app: &App) -> String {
    let shown = app
        .visible()
        .iter()
        .filter(|&&idx| app.selected.contains(&app.items[idx].path))
        .count();
    format!(
        "selected: {} · hidden by filters: {}",
        app.selected.len(),
        app.selected.len().saturating_sub(shown)
    )
}

fn empty_text(app: &App) -> &'static str {
    if app.scanning() {
        "Scanning for linked worktrees…"
    } else if app.items.is_empty() {
        "No repos with linked worktrees. r rescans. R shows read errors."
    } else {
        "No matches. 0 clears all filters. Selected worktrees stay selected."
    }
}

fn selected_worktrees(app: &App) -> Vec<&Worktree> {
    app.selected
        .iter()
        .filter_map(|path| app.items.iter().find(|wt| &wt.path == path))
        .collect()
}

fn selected_lines(app: &App) -> Vec<Line<'static>> {
    let chosen = selected_worktrees(app);
    let mut lines = Vec::new();
    for (i, wt) in chosen.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        let hidden = !app
            .visible()
            .iter()
            .any(|&idx| app.items[idx].path == wt.path);
        lines.push(Line::from(format!(
            "{}/{} · {} · {}{}",
            i + 1,
            chosen.len(),
            wt.id,
            wt.flags(),
            if hidden { " · hidden by filters" } else { "" }
        )));
        lines.push(Line::from(format!("branch: {}", wt.branch)));
        lines.push(Line::from(format!("path: {}", wt.path.display())));
    }
    if chosen.is_empty() {
        lines.push(Line::from("No selected worktrees."));
    }
    lines
}

fn confirmation_header(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(selection_text(app))];
    match app.pending {
        Pending::Delete { force } => {
            lines.push(Line::from(
                "Removes worktree directories via git worktree remove.",
            ));
            if force {
                lines.push(Line::styled(
                    "FORCE ON: uncommitted changes will be lost.",
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::from("Dirty and locked worktrees may be removed."));
            } else {
                lines.push(Line::from(
                    "Force OFF: existing paths must be clean; locks block.",
                ));
            }
            match app.branch_scope {
                remove::BranchScope::Keep => {
                    lines.push(Line::from("Branches KEEP: local and remote branches stay."));
                }
                remove::BranchScope::Local => {
                    lines.push(Line::from("Branches DELETE: local via git branch -D."));
                    lines.push(Line::from(
                        "Unmerged commits can lose their branch reference.",
                    ));
                    lines.push(Line::from(
                        "Remote branches and branches still in use stay.",
                    ));
                }
                remove::BranchScope::GitHub => {
                    lines.push(Line::from(
                        "Branches DELETE: local and GitHub remote branches.",
                    ));
                    lines.push(Line::from(
                        "Local uses git branch -D, including unmerged branches.",
                    ));
                    lines.push(Line::from(
                        "GitHub uses git push --delete after local deletion.",
                    ));
                    lines.push(Line::from(
                        "Branches still in use stay. Errors appear in results.",
                    ));
                }
            }
        }
        Pending::Clean => {
            lines.push(Line::from("Removes dependency directories recursively."));
            lines.push(Line::from(
                "Worktrees and branches stay. Main checkouts stay protected.",
            ));
        }
        Pending::None => {}
    }
    lines
}

fn confirmation_controls(app: &App) -> Vec<Line<'static>> {
    match app.pending {
        Pending::Delete { force } => {
            let mut lines = vec![Line::from(if app.high_risk() {
                "Type delete then Enter to confirm · Esc cancel"
            } else {
                "y confirm · or type delete then Enter · Esc cancel"
            })];
            lines.push(Line::from(format!(
                "f force {} · b branch policy: {}",
                if force { "off" } else { "on" },
                app.branch_scope.label()
            )));
            lines.push(Line::from(format!(
                "typed: {}_{}",
                app.confirm_buf,
                if app.confirm_hint.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", app.confirm_hint)
                }
            )));
            lines
        }
        Pending::Clean => vec![
            Line::from("y confirm clean · Esc cancel · Enter does nothing"),
            Line::from(app.confirm_hint.clone()),
        ],
        Pending::None => Vec::new(),
    }
}

fn confirmation_fits(app: &App) -> bool {
    let width = app.viewport.width.saturating_sub(2);
    let fixed = wrap_lines(confirmation_header(app), width).len()
        + wrap_lines(confirmation_controls(app), width).len()
        + wrap_lines(vec![Line::from("j/k scroll · PgUp/PgDn · Home/End")], width).len();
    app.viewport.width >= 40 && app.viewport.height as usize > fixed + 2
}

fn detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(wt) = app
        .visible()
        .get(app.cursor)
        .and_then(|&idx| app.items.get(idx))
    else {
        return vec![Line::from(empty_text(app))];
    };
    vec![
        Line::from(format!("id: {}", wt.id)),
        Line::from(format!("path: {}", wt.path.display())),
        Line::from(format!("repo: {}", wt.repo)),
        Line::from(format!("branch: {}", wt.branch)),
        Line::from(format!("status: {}", wt.flags())),
        Line::from(format!("folder: {}", wt.parent.display())),
        Line::from(format!("main checkout: {}", wt.repo_root.display())),
        Line::from(format!(
            "last active: {} · {}",
            wt.last_active_utc.as_deref().unwrap_or("unknown"),
            age_label(wt.last_active, app.now)
        )),
        Line::from(format!(
            "created: {}",
            wt.created_utc.as_deref().unwrap_or("unknown")
        )),
    ]
}

fn result_lines(app: &App) -> Vec<Line<'static>> {
    let mut items: Vec<_> = app.last_result.iter().collect();
    items.sort_by_key(|item| match item.result.as_str() {
        "failed" => 0,
        "blocked" => 1,
        _ => 2,
    });
    let mut lines = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            format!(
                "{}/{} · {} · {} · id {}",
                i + 1,
                items.len(),
                item.result,
                item.action,
                item.id
            ),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(format!("path: {}", item.path.display())));
        if let Some(blocked) = &item.blocked_by {
            lines.push(Line::from(format!("blocked by: {blocked}")));
        }
        lines.push(Line::from(format!("reason: {}", item.reason)));
    }
    for (i, error) in app.errors.iter().enumerate() {
        lines.push(Line::from(format!(
            "scan error {}/{}: {error}",
            i + 1,
            app.errors.len()
        )));
        lines.push(Line::default());
    }
    if lines.is_empty() {
        lines.push(Line::from("No operation results or scan errors."));
    }
    lines
}

fn help_lines() -> Vec<Line<'static>> {
    [
        "Browse and inspect",
        "j/k or arrows move · PgUp/PgDn page · Home/End first/last",
        "Enter shows full details · R shows all results and scan errors",
        "r rescans · q or Esc quits · Ctrl+C quits from any view",
        "",
        "Selection",
        "space toggles the current linked worktree. Main checkouts cannot be selected.",
        "a toggles deletable rows in the current view. Other selections stay.",
        "v shows every selected worktree, including those hidden by filters.",
        "c clears all selections, including hidden selections.",
        "d reviews deletion · x reviews dependency cleanup",
        "",
        "Filters",
        "/ edits the text filter · Enter applies · Esc clears text",
        "In browse: m merged · i inactive · s old and inactive · 0 clears all filters",
        "Presets replace the text filter. Filters never change selections.",
        "Merged uses the default branch, preferring local and falling back to its remote-tracking ref. No network lookup. Squash merges are not detected.",
        "",
        "Review and confirm",
        "j/k, PgUp/PgDn and Home/End scroll all targets, details and results.",
        "Esc cancels a confirmation. Esc or q closes a viewer.",
        "y confirms ordinary deletion or cleanup. Enter alone does nothing.",
        "f toggles force mode. Uncommitted changes may be lost when force is on.",
        "b cycles keep, local deletion and local plus GitHub deletion.",
        "Force or branch deletion requires typing delete, then Enter.",
        "Changing force or branch policy clears the typed confirmation.",
    ]
    .into_iter()
    .map(Line::from)
    .collect()
}

fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::new();
    for line in lines {
        for part in line.to_string().split('\n') {
            let source = Span::raw(part);
            let mut spans = Vec::new();
            let mut used = 0;
            for grapheme in source.styled_graphemes(line.style) {
                let span = Span::styled(grapheme.symbol.to_owned(), grapheme.style);
                let size = span.width();
                if used + size > width as usize && !spans.is_empty() {
                    wrapped.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                }
                used += size;
                spans.push(span);
            }
            wrapped.push(Line::from(spans));
        }
    }
    wrapped
}

fn draw_panel(frame: &mut Frame, app: &mut App) {
    let (title, header, content, mut controls) = match app.pending {
        Pending::Delete { .. } | Pending::Clean => {
            let title = if app.pending == Pending::Clean {
                " confirm clean "
            } else if app.high_risk() {
                " HIGH RISK delete "
            } else {
                " confirm delete "
            };
            let mut content = Vec::new();
            if app.pending == Pending::Clean {
                content.push(Line::from("Removes node_modules, target, .next, .turbo, .venv, venv, __pycache__, Pods and .gradle, including nested matches."));
                content.push(Line::default());
            }
            content.extend(selected_lines(app));
            (
                title,
                confirmation_header(app),
                content,
                confirmation_controls(app),
            )
        }
        Pending::None => match app.view {
            View::Selected => (
                " all selected worktrees ",
                vec![Line::from(selection_text(app))],
                selected_lines(app),
                vec![Line::from("c clear all · d delete · x clean · Esc/q back")],
            ),
            View::Results => (
                " all results · failures first ",
                vec![Line::from(format!(
                    "{} results · {} succeeded · {} failed or blocked · {} scan errors",
                    app.last_result.len(),
                    ops::counts(&app.last_result).0,
                    ops::counts(&app.last_result).1,
                    app.errors.len()
                ))],
                result_lines(app),
                vec![Line::from("Esc/q back")],
            ),
            View::Detail => (
                " worktree details ",
                Vec::new(),
                detail_lines(app),
                vec![Line::from("Esc/q back")],
            ),
            View::Help | View::Browse => (
                " help ",
                Vec::new(),
                help_lines(),
                vec![Line::from("Esc/q back")],
            ),
        },
    };
    controls.push(Line::from("j/k scroll · PgUp/PgDn · Home/End"));
    let block = Block::bordered().title(title);
    let inner = block.inner(frame.area());
    let header = wrap_lines(header, inner.width);
    let controls = wrap_lines(controls, inner.width);
    if inner.height as usize <= header.len() + controls.len() || inner.width < 38 {
        frame.render_widget(block, frame.area());
        let message = if app.pending == Pending::None {
            "Resize terminal to view full content.\nEsc/q back."
        } else {
            "Resize terminal to review and confirm.\nConfirmation disabled. Esc cancels."
        };
        frame.render_widget(
            Paragraph::new(wrap_lines(vec![Line::from(message)], inner.width)),
            inner,
        );
        return;
    }
    let areas = Layout::vertical([
        Constraint::Length(header.len() as u16),
        Constraint::Min(1),
        Constraint::Length(controls.len() as u16),
    ])
    .split(inner);
    let content = wrap_lines(content, inner.width);
    app.scroll = app
        .scroll
        .min(content.len().saturating_sub(areas[1].height as usize));
    let end = (app.scroll + areas[1].height as usize).min(content.len());
    let block = block.title_bottom(format!(
        " lines {}-{}/{} ",
        app.scroll + 1,
        end,
        content.len()
    ));
    frame.render_widget(block, frame.area());
    frame.render_widget(Paragraph::new(header), areas[0]);
    frame.render_widget(Paragraph::new(content[app.scroll..end].to_vec()), areas[1]);
    frame.render_widget(Paragraph::new(controls), areas[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::fixture;
    use ratatui::{backend::TestBackend, Terminal};

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

    fn render(app: &mut App, width: u16, height: u16) -> String {
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

    fn scroll_screens(app: &mut App, width: u16, height: u16) -> String {
        app.on_key(KeyCode::Home, KeyModifiers::NONE);
        let mut screens = render(app, width, height);
        loop {
            let before = app.scroll;
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
            let screen = render(app, width, height);
            if app.scroll == before {
                break;
            }
            screens.push('\n');
            screens.push_str(&screen);
        }
        screens
    }

    fn long_worktrees() -> Vec<Worktree> {
        (1..=8)
            .map(|i| {
                fixture(
                    &format!(
                        "/repos/demo/.worktrees/{i:02}/{}TAIL{i:02}",
                        "long-directory/".repeat(26)
                    ),
                    "demo",
                    false,
                    Some(10),
                )
            })
            .collect()
    }

    #[test]
    fn browsing_keeps_shortcuts_visible_with_long_details() {
        for (width, height) in [(120, 30), (80, 24), (60, 18)] {
            let mut app = app_with(long_worktrees());
            app.items[0].dirty = Some(true);
            app.items[0].locked = true;
            let screen = render(&mut app, width, height);
            for hint in [
                "j/k move",
                "space select",
                "a visible",
                "v selected",
                "c clear",
                "d delete",
                "x clean deps",
                "Enter details",
                "R results",
                "? help",
                "m/i/s presets",
                "0 clear",
                "/ filter",
                "r refresh",
                "q quit",
            ] {
                assert!(
                    screen.contains(hint),
                    "{width}x{height} missing {hint}:\n{screen}"
                );
            }
            assert!(screen.contains("[dirty] [locked]"), "{screen}");
            assert!(screen.contains("scanned ~/project"), "{screen}");
            let header = screen.lines().nth(3).unwrap();
            assert_eq!(header.contains("folder"), width >= 120, "{screen}");
            assert_eq!(header.contains("path"), width >= 120, "{screen}");
        }
    }

    #[test]
    fn empty_and_filter_states_keep_recovery_keys_and_caret_visible() {
        for (width, height) in [(120, 30), (80, 24), (60, 18), (61, 18)] {
            let mut app = app_with(Vec::new());
            let screen = render(&mut app, width, height);
            assert!(
                screen.contains("No repos with linked worktrees."),
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
            let screen = render(&mut app, width, height);
            assert!(screen.contains("scanning ~/project"), "{screen}");
            assert!(screen.contains("q quit"), "{screen}");
            assert!(
                !screen.contains("No repos with linked worktrees."),
                "{screen}"
            );

            let mut app = app_with(long_worktrees());
            app.filter = "no-such-worktree".to_string();
            app.clamp_cursor();
            let screen = render(&mut app, width, height);
            assert!(screen.contains("No matches."), "{screen}");
            assert!(
                !screen.contains("No repos with linked worktrees."),
                "{screen}"
            );
            assert!(screen.contains("/ filter"), "{screen}");
            assert!(screen.contains("0 clear"), "{screen}");
            assert!(screen.contains("text: \"no-such-worktree\""), "{screen}");

            app.on_key(KeyCode::Char('0'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
            app.filter = format!("{}终点é-tail", "长过滤/".repeat(64));
            let screen = render(&mut app, width, height);
            assert!(
                screen.lines().nth(1).unwrap().contains("终点é-tail_"),
                "{screen}"
            );
            assert!(screen.contains("selected: 8"), "{screen}");
            assert!(!screen.contains('\u{fffd}'), "{screen}");
            assert!(screen.contains("Enter apply · Esc clear text"), "{screen}");
        }
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
        assert_eq!(app.pending, Pending::Delete { force: false });
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
            Action::None
        );
        for ch in "delete".chars() {
            assert_eq!(
                app.on_key(KeyCode::Char(ch), KeyModifiers::NONE),
                Action::None
            );
        }
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::GitHub,
            }
        );
    }

    #[test]
    fn typing_delete_requires_enter() {
        let mut app = app_with(vec![fixture(
            "/repos/demo/.worktrees/feat",
            "demo",
            false,
            Some(10),
        )]);
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        for ch in "delete".chars() {
            assert_eq!(
                app.on_key(KeyCode::Char(ch), KeyModifiers::NONE),
                Action::None
            );
        }
        assert_eq!(app.pending, Pending::Delete { force: false });
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::Keep
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
            Action::None
        );
        assert_eq!(app.pending, Pending::Delete { force: true });
        assert_eq!(
            app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Action::None
        );
        for ch in "delete".chars() {
            assert_eq!(
                app.on_key(KeyCode::Char(ch), KeyModifiers::NONE),
                Action::None
            );
        }
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete {
                force: true,
                branches: remove::BranchScope::Keep
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
        assert!(!column_spec(119).show_path);
        assert_eq!(column_spec(60).widths[2], Constraint::Length(8));
        assert_eq!(column_spec(60).widths[3], Constraint::Length(10));
        let wide = column_spec(120);
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
        assert!(app.confirm_hint.contains("Type delete"));
        assert_eq!(app.pending, Pending::Delete { force: false });
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
        assert_eq!(app.status_text(), "finished: 1 succeeded");
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
        assert_eq!(
            app.status_text(),
            "finished: 0 succeeded, 1 failed or blocked"
        );
        assert!(result_lines(&app)
            .iter()
            .any(|line| line.to_string().contains("dirty")));
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

    #[test]
    fn hidden_selections_are_counted_reviewed_and_cleared_globally() {
        for (width, height) in [(60, 16), (80, 24), (140, 32)] {
            let mut app = app_with(long_worktrees());
            app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
            for ch in "TAIL01".chars() {
                app.on_key(KeyCode::Char(ch), KeyModifiers::NONE);
            }
            app.on_key(KeyCode::Enter, KeyModifiers::NONE);
            let screen = render(&mut app, width, height);
            assert!(
                screen.contains("selected: 8 · hidden by filters: 7"),
                "{screen}"
            );
            assert!(screen.contains("TAIL01"), "{screen}");
            assert_eq!(app.selected.len(), 8);

            app.on_key(KeyCode::Char('v'), KeyModifiers::NONE);
            let screens = scroll_screens(&mut app, width, height);
            assert!(screens.contains("8/8 ·"), "{screens}");
            assert!(screens.contains("TAIL08"), "{screens}");
            app.on_key(KeyCode::End, KeyModifiers::NONE);
            let end = render(&mut app, width, height);
            assert!(end.contains("TAIL08"), "{end}");
            app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
            let cleared = render(&mut app, width, height);
            assert!(
                cleared.contains("selected: 0 · hidden by filters: 0"),
                "{cleared}"
            );
            assert!(cleared.contains("No selected worktrees."), "{cleared}");
            assert_eq!(app.visible().len(), 1);
        }
    }

    #[test]
    fn confirmation_keeps_risks_and_controls_visible_while_every_target_scrolls() {
        for (width, height) in [(60, 16), (80, 24), (140, 32)] {
            let mut app = app_with(long_worktrees());
            app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
            app.filter = "TAIL01".to_string();
            app.clamp_cursor();
            app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('f'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
            let first = render(&mut app, width, height);
            assert!(first.contains("HIGH RISK delete"), "{first}");
            assert!(
                first.contains("uncommitted changes will be lost"),
                "{first}"
            );
            assert!(first.contains("GitHub uses git push --delete"), "{first}");
            assert!(first.contains("Type delete then Enter"), "{first}");
            assert!(confirmation_fits(&app));
            let screens = scroll_screens(&mut app, width, height);
            assert!(screens.contains("8/8 ·"), "{screens}");
            assert!(screens.contains("TAIL08"), "{screens}");
            app.on_key(KeyCode::End, KeyModifiers::NONE);
            let end = render(&mut app, width, height);
            assert!(end.contains("TAIL08"), "{end}");
            assert!(end.contains("uncommitted changes will be lost"), "{end}");
            assert!(end.contains("Type delete then Enter"), "{end}");
            assert!(app.confirm_buf.is_empty());
            assert_eq!(
                app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
                Action::None
            );
            assert_eq!(app.pending, Pending::Delete { force: true });
        }
    }

    #[test]
    fn clean_confirmation_scrolls_every_target_and_keeps_consequences_visible() {
        for (width, height) in [(60, 16), (80, 24), (140, 32)] {
            let mut app = app_with(long_worktrees());
            app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
            app.on_key(KeyCode::Char('x'), KeyModifiers::NONE);
            let first = render(&mut app, width, height);
            assert!(
                first.contains("dependency directories recursively"),
                "{first}"
            );
            assert!(first.contains("Worktrees and branches stay"), "{first}");
            assert!(first.contains("y confirm clean"), "{first}");
            let screens = scroll_screens(&mut app, width, height);
            assert!(screens.contains(".gradle"), "{screens}");
            assert!(screens.contains("8/8 ·"), "{screens}");
            app.on_key(KeyCode::End, KeyModifiers::NONE);
            let end = render(&mut app, width, height);
            assert!(end.contains("TAIL08"), "{end}");
            assert!(end.contains("Worktrees and branches stay"), "{end}");
            assert!(end.contains("y confirm clean"), "{end}");
            assert_eq!(app.pending, Pending::Clean);
        }
    }

    #[test]
    fn high_risk_policy_changes_invalidate_typed_confirmation() {
        let mut app = app_with(long_worktrees());
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        for ch in "delete".chars() {
            app.on_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(app.confirm_buf, "");
        assert!(app.high_risk());
        assert_eq!(app.on_key(KeyCode::Enter, KeyModifiers::NONE), Action::None);
        for ch in "delete".chars() {
            app.on_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Char('f'), KeyModifiers::NONE);
        assert_eq!(app.confirm_buf, "");
        assert_eq!(
            app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Action::None
        );
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.branch_scope, remove::BranchScope::Keep);
        assert_eq!(app.pending, Pending::Delete { force: false });
    }

    #[test]
    fn too_small_confirmation_is_explicit_and_cannot_execute() {
        let mut app = app_with(long_worktrees());
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        let screen = render(&mut app, 40, 8);
        assert!(screen.contains("Resize terminal"), "{screen}");
        assert!(screen.contains("Confirmation disabled"), "{screen}");
        assert_eq!(
            app.on_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Action::None
        );
        for ch in "delete".chars() {
            app.on_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.on_key(KeyCode::Enter, KeyModifiers::NONE), Action::None);
        render(&mut app, 80, 24);
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::Keep
            }
        );
    }

    #[test]
    fn zero_clears_text_and_preset_and_empty_states_differ() {
        let mut app = app_with(long_worktrees());
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('m'), KeyModifiers::NONE);
        app.filter = "no-such-worktree".to_string();
        app.clamp_cursor();
        let no_match = render(&mut app, 60, 16);
        assert!(no_match.contains("No matches."), "{no_match}");
        assert!(no_match.contains("no-such-worktree"), "{no_match}");
        app.on_key(KeyCode::Char('0'), KeyModifiers::NONE);
        let reset = render(&mut app, 60, 16);
        assert!(reset.contains("filter: all | text: \"\""), "{reset}");
        assert_eq!(app.visible().len(), 8);
        assert_eq!(app.selected.len(), 8);
        let empty = render(&mut app_with(Vec::new()), 60, 16);
        assert!(empty.contains("No repos with linked worktrees."), "{empty}");
    }

    #[test]
    fn results_render_failures_first_and_scroll_past_four_full_errors() {
        for (width, height) in [(60, 16), (80, 24), (140, 32)] {
            let mut app = app_with(long_worktrees());
            let items = app
                .items
                .iter()
                .enumerate()
                .map(|(i, wt)| {
                    ActionItem::new(
                        "delete",
                        wt,
                        format!(
                            "cannot remove\n{}RESULT{:02}_END",
                            "long error ".repeat(30),
                            i + 1
                        ),
                        None,
                        if i == 7 { "failed" } else { "ok" },
                    )
                })
                .collect();
            app.apply_mutate_result(items);
            let first = render(&mut app, width, height);
            assert!(first.contains("1/8 · failed · delete"), "{first}");
            assert_eq!(first.matches("7 succeeded").count(), 1, "{first}");
            assert_eq!(first.matches("1 failed or blocked").count(), 1, "{first}");
            let screens = scroll_screens(&mut app, width, height);
            for i in 1..=8 {
                assert!(
                    screens.contains(&format!("RESULT{i:02}_END")),
                    "missing result {i}"
                );
            }
            app.on_key(KeyCode::End, KeyModifiers::NONE);
            let end = render(&mut app, width, height);
            assert!(end.contains("RESULT07_END"), "{end}");
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            app.on_key(KeyCode::Char('R'), KeyModifiers::NONE);
            let reopened = render(&mut app, width, height);
            assert!(reopened.contains("1/8 · failed · delete"), "{reopened}");
        }
    }

    #[test]
    fn details_and_scan_errors_keep_long_paths_and_multiline_errors() {
        for (width, height) in [(60, 16), (80, 24), (140, 32)] {
            let mut app = app_with(long_worktrees());
            app.on_key(KeyCode::Enter, KeyModifiers::NONE);
            let screens = scroll_screens(&mut app, width, height);
            assert!(screens.contains("TAIL01"), "{screens}");
            assert!(screens.contains("last active:"), "{screens}");
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            app.errors = vec![format!(
                "cannot read\n{}SCAN_ERROR_END",
                "long error ".repeat(30)
            )];
            app.on_key(KeyCode::Char('R'), KeyModifiers::NONE);
            let screens = scroll_screens(&mut app, width, height);
            assert!(screens.contains("cannot read"), "{screens}");
            assert!(screens.contains("SCAN_ERROR_END"), "{screens}");
        }
    }

    #[test]
    fn clean_result_keeps_worktree_and_selection() {
        let mut app = app_with(long_worktrees());
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.apply_mutate_result(vec![ActionItem::new(
            "clean",
            &app.items[0],
            "removed target",
            None,
            "ok",
        )]);
        assert_eq!(app.items.len(), 8);
        assert_eq!(app.selected.len(), 8);
        assert_eq!(app.status_text(), "finished: 1 succeeded");
    }

    #[test]
    fn unknown_activity_is_neutral_and_column_name_is_consistent() {
        let unknown = fixture("/repos/demo/.worktrees/unknown", "demo", false, None);
        let old = fixture("/repos/demo/.worktrees/old", "demo", false, Some(1));
        assert_eq!(row_style(&unknown, 30 * 86_400), Style::new());
        assert_eq!(row_style(&old, 30 * 86_400).fg, Some(Color::Red));
        assert!(column_spec(60).headers.contains(&"active"));
        assert!(column_spec(140).headers.contains(&"active"));
    }
}
