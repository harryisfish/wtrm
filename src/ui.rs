use std::collections::BTreeSet;
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

use crate::remove;
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
    pub status: String,
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
            status: String::new(),
            now: timeutil::now_unix(),
            query: Query::default(),
            inactive_secs: 14 * 86_400,
            stale_created_secs: 30 * 86_400,
            stale_idle_secs: 7 * 86_400,
            branch_scope: remove::BranchScope::Keep,
        };
        app.apply_scan(result);
        app.status = if let Some(err) = errors.first() {
            format!("{} read errors. {err}", errors.len())
        } else {
            format!("scanned {}", app.roots_label)
        };
        app
    }

    pub fn apply_scan(&mut self, result: ScanResult) {
        self.repos = result.repos;
        self.errors = result.errors;
        self.items = result.worktrees;
        self.selected.retain(|path| {
            self.items
                .iter()
                .any(|wt| &wt.path == path && wt.deletable())
        });
        self.pending = Pending::None;
        self.now = timeutil::now_unix();
        self.clamp_cursor();
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
                    "{} {} {} {} {}",
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
            }
            KeyCode::Enter => self.filter_mode = false,
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
            KeyCode::Enter => {
                let pending = self.pending;
                self.pending = Pending::None;
                match pending {
                    Pending::Clean => Action::CleanDeps,
                    Pending::Delete => Action::Delete {
                        force: false,
                        branches: self.branch_scope,
                    },
                    Pending::None => Action::None,
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F') if self.pending == Pending::Delete => {
                self.pending = Pending::None;
                Action::Delete {
                    force: true,
                    branches: self.branch_scope,
                }
            }
            KeyCode::Char('b') if self.pending == Pending::Delete => {
                self.branch_scope = self.branch_scope.next();
                Action::None
            }
            KeyCode::Esc => {
                self.pending = Pending::None;
                Action::None
            }
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
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
                self.selected.clear();
                self.cursor = 0;
                self.status = "showing all".to_string();
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
            self.status = "the main checkout is only a reference and cannot be deleted".to_string();
            return;
        }
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
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
            self.status = "nothing here can be deleted".to_string();
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
    }

    fn arm_delete(&mut self) {
        if self.selected.is_empty() {
            self.toggle_current();
        }
        if self.selected.is_empty() {
            self.status = "select a worktree to delete".to_string();
            return;
        }
        self.pending = Pending::Delete;
        self.branch_scope = remove::BranchScope::Keep;
    }

    fn arm_clean(&mut self) {
        if self.selected.is_empty() {
            self.toggle_current();
        }
        if self.selected.is_empty() {
            self.status = "select a worktree to clean".to_string();
            return;
        }
        self.pending = Pending::Clean;
    }

    fn apply_preset(&mut self, query: Query) {
        self.query = query;
        self.filter.clear();
        self.filter_mode = false;
        self.pending = Pending::None;
        self.cursor = 0;
        self.selected.clear();
        for idx in self.visible() {
            let wt = &self.items[idx];
            if wt.deletable() {
                self.selected.insert(wt.path.clone());
            }
        }
        self.status = format!(
            "filter {}, {} selected. d delete, x clean deps",
            self.query.label(),
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
                app.status = "rescanning…".to_string();
                terminal.draw(|frame| draw(frame, &app))?;
                let result = scan::scan(&opts.roots, opts.max_depth);
                app.apply_scan(result);
                app.status = format!("refreshed, {} worktrees", app.items.len());
            }
            Action::Delete { force, branches } => {
                let paths: Vec<_> = app.selected.iter().cloned().collect();
                app.status = "deleting…".to_string();
                terminal.draw(|frame| draw(frame, &app))?;
                let message =
                    remove::summarize(&remove::delete_many(&app.items, &paths, force, branches));
                let result = scan::scan(&opts.roots, opts.max_depth);
                app.apply_scan(result);
                app.status = message;
            }
            Action::CleanDeps => {
                let paths: Vec<_> = app.selected.iter().cloned().collect();
                app.status = "cleaning dependencies…".to_string();
                terminal.draw(|frame| draw(frame, &app))?;
                let message = remove::summarize_clean(&remove::clean_many(&app.items, &paths));
                let result = scan::scan(&opts.roots, opts.max_depth);
                app.apply_scan(result);
                app.status = message;
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
    let foot_h = if app.pending == Pending::None { 9 } else { 12 };
    let chunks =
        Layout::vertical([Constraint::Min(3), Constraint::Length(foot_h)]).split(frame.area());
    draw_table(frame, app, chunks[0]);
    draw_footer(frame, app, chunks[1]);
}

fn draw_table(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let visible = app.visible();
    let shown = if app.filter.is_empty() {
        format!("{}", app.items.len())
    } else {
        format!("{}/{}", visible.len(), app.items.len())
    };
    let title = format!(
        " wtrm  {shown} worktrees · {} selected · {} repos ",
        app.selected.len(),
        app.repos
    );
    let header = Row::new(["", "active", "repo", "branch", "folder", "status", "path"])
        .style(Style::new().add_modifier(Modifier::BOLD));
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
            Row::new([
                Cell::from(mark),
                Cell::from(age_label(wt.last_active, app.now)),
                Cell::from(wt.repo.as_str()),
                Cell::from(wt.branch.as_str()),
                Cell::from(scan::shorten_path(&wt.parent)),
                Cell::from(wt.flags()),
                Cell::from(scan::shorten_path(&wt.path)),
            ])
            .style(row_style(wt, app.now))
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Length(16),
        Constraint::Length(22),
        Constraint::Length(28),
        Constraint::Length(16),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
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
        Pending::None => browse_lines(app),
    };
    let title = match app.pending {
        Pending::Delete => " confirm delete ",
        Pending::Clean => " confirm clean ",
        Pending::None if app.filter_mode => " filter ",
        Pending::None => " help ",
    };
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
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
                "{}  ·  {}  ·  active {when}  ·  created {created}  ·  {}",
                scan::shorten_path(&wt.path),
                wt.branch,
                wt.flags()
            )
        }
        None => "no repos with linked worktrees".to_string(),
    };
    let filter = if app.filter.is_empty() {
        String::new()
    } else {
        format!("filter: {}  ", app.filter)
    };
    vec![
        Line::from(detail),
        Line::from(Span::styled(
            format!(
                "filter {}. merged means the commit is in the local default branch. squash merges are not detected.",
                app.query.label()
            ),
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(format!("{filter}{}", app.status)),
        Line::from("m merged   i idle   s old+idle   0 all   x clean deps"),
        Line::from("j/k move   space select   a all   d delete   b branch   / filter   r refresh   q quit"),
    ]
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
        Line::from("Enter confirm    Esc cancel"),
    ];
    for wt in chosen.iter().take(6) {
        lines.push(Line::from(format!(
            "  {}  {}",
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
    let mut lines = vec![Line::from(format!(
        "delete {} worktrees. branches: {}. Enter clean, f force, b cycle branches, Esc cancel.",
        chosen.len(),
        app.branch_scope.label()
    ))];
    for wt in chosen.iter().take(6) {
        lines.push(Line::from(format!(
            "  {}  {}  {}",
            wt.flags(),
            wt.branch,
            scan::shorten_path(&wt.path)
        )));
    }
    if chosen.len() > 6 {
        lines.push(Line::from(format!("  …{} more", chosen.len() - 6)));
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
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.pending, Pending::Delete);
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::Keep,
            }
        );
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete {
                force: false,
                branches: remove::BranchScope::GitHub,
            }
        );
    }

    #[test]
    fn presets_select_merged_idle_and_old() {
        let now = 1_000_000;
        let mut merged = fixture("/repos/demo/.worktrees/merged", "demo", false, Some(now));
        merged.merged = Some(true);
        let mut idle = fixture(
            "/repos/demo/.worktrees/idle",
            "demo",
            false,
            Some(now - 20 * 86_400),
        );
        idle.created_at = Some(now - 2 * 86_400);
        let mut old = fixture(
            "/repos/demo/.worktrees/old",
            "demo",
            false,
            Some(now - 20 * 86_400),
        );
        old.created_at = Some(now - 40 * 86_400);
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
        assert_eq!(app.selected.len(), 1);
        assert!(app
            .selected
            .contains(&PathBuf::from("/repos/demo/.worktrees/merged")));

        app.on_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(app.selected.len(), 2);

        app.on_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(
            app.selected.iter().cloned().collect::<Vec<_>>(),
            vec![PathBuf::from("/repos/demo/.worktrees/old")]
        );

        app.on_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.pending, Pending::Clean);
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
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
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(
            app.on_key(KeyCode::Char('f'), KeyModifiers::NONE),
            Action::Delete {
                force: true,
                branches: remove::BranchScope::Keep,
            }
        );
    }
}
