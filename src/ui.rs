use std::collections::{BTreeSet, HashSet};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

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

    pub fn visible(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        let indexes = scan::matching_indexes(&self.items, &self.query, self.now);
        indexes
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
            .collect()
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
                self.note = format!("view changed: {} matches", self.visible().len());
            }
            KeyCode::Enter => {
                self.filter_mode = false;
                self.note = format!("view changed: {} matches", self.visible().len());
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
                self.note = format!("view changed: {} matches", self.visible().len());
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
            .visible()
            .into_iter()
            .filter_map(|idx| {
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
        self.cursor = self
            .visible()
            .iter()
            .position(|&idx| self.items[idx].deletable())
            .unwrap_or(0);
        self.note = format!(
            "view changed: {} matches. selection unchanged: {} selected",
            self.visible().len(),
            self.selected.len()
        );
    }

    fn current_path(&self) -> Option<PathBuf> {
        let idx = *self.visible().get(self.cursor)?;
        self.items.get(idx).map(|wt| wt.path.clone())
    }

    fn clamp_cursor(&mut self) {
        let len = self.visible().len();
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
    eprintln!("scanning {roots_label} …");
    let result = scan::scan(&opts.roots, opts.max_depth);
    eprintln!(
        "found {} worktrees ({} repos)",
        result.worktrees.len(),
        result.repos
    );
    if !io::stdout().is_terminal() {
        anyhow::bail!("stdout is not a terminal. use --list or --json");
    }
    let mut app = App::from_scan(result, roots_label);
    app.query = opts.query;
    app.inactive_secs = opts.inactive_secs;
    app.stale_created_secs = opts.stale_created_secs;
    app.stale_idle_secs = opts.stale_idle_secs;
    let mut terminal = ratatui::try_init().context("cannot enter the terminal ui")?;
    let _restore = Restore;
    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match app.on_key(key.code, key.modifiers) {
            Action::None => {}
            Action::Quit => break,
            Action::Rescan => {
                app.op = OperationStatus::Running {
                    action: ActionKind::Scan,
                    total: 0,
                };
                app.note = "rescanning…".to_string();
                terminal.draw(|frame| draw(frame, &app))?;
                let result = scan::scan(&opts.roots, opts.max_depth);
                let pruned = app.apply_scan(result);
                app.op = OperationStatus::Finished {
                    succeeded: app.items.len(),
                    failed: app.errors.len(),
                };
                app.note = if pruned > 0 {
                    format!(
                        "refreshed, {} worktrees. selection pruned: {pruned}",
                        app.items.len()
                    )
                } else {
                    format!("refreshed, {} worktrees", app.items.len())
                };
            }
            Action::Delete { force, branches } => {
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
                    terminal.draw(|frame| draw(frame, &app))?;
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
            }
            Action::CleanDeps => {
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
                    terminal.draw(|frame| draw(frame, &app))?;
                    all.extend(ops::clean_targets(
                        &app.items,
                        std::slice::from_ref(path),
                        &[],
                        true,
                    ));
                }
                app.apply_mutate_result(all);
            }
        }
    }
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
        Pending::None if app.filter_mode => 7 + extra,
        Pending::None => 9 + extra,
        _ => 14,
    };
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
    if width >= 110 {
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
                Constraint::Length(14),
                Constraint::Length(16),
                Constraint::Length(6),
                Constraint::Min(18),
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
    let rows: Vec<Row> = visible
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
    if !visible.is_empty() {
        state.select(Some(app.cursor.min(visible.len() - 1)));
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
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn filter_lines(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("filter: {}_", app.filter)),
        Line::from("Enter apply    Esc clear"),
        Line::from(app.status_text()),
    ]
}

fn browse_lines(app: &App) -> Vec<Line<'static>> {
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
        None => "no repos with linked worktrees".to_string(),
    };
    let mut lines = vec![
        Line::from(detail),
        Line::from(Span::styled(
            format!(
                "filter {}. merged means the commit is in the local default branch. squash merges are not detected.",
                app.query.label()
            ),
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(app.status_text()),
    ];
    lines.extend(result_lines(app));
    lines.push(Line::from(
        "m merged   i idle   s old+idle   0 all   x clean deps",
    ));
    lines.push(Line::from(
        "j/k move   space select   a toggle visible   d delete   / filter   r refresh   q quit",
    ));
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
        Line::from("y confirm    Esc cancel    Enter does nothing"),
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
    let typed = if app.confirm_buf.is_empty() {
        "type y or delete to confirm".to_string()
    } else {
        format!("typed: {}_", app.confirm_buf)
    };
    let mut lines = vec![
        Line::from(format!("delete {} worktrees", chosen.len())),
        Line::from(format!("branch policy: {}", app.branch_scope.label())),
        Line::from("y delete clean worktrees"),
        Line::from("f force delete dirty or locked worktrees"),
        Line::from("b branch policy: keep → local → GitHub"),
        Line::from(format!("{typed}    Esc cancel    Enter does nothing")),
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
            .into_iter()
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
}
