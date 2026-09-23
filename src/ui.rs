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
use crate::scan::{self, ScanResult, Worktree};
use crate::timeutil::{self, age_label};

pub struct Options {
    pub roots: Vec<PathBuf>,
    pub max_depth: u8,
    pub older_than_secs: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Rescan,
    Delete { force: bool },
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
    pub confirm: bool,
    pub status: String,
    pub now: i64,
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
            confirm: false,
            status: String::new(),
            now: timeutil::now_unix(),
        };
        app.apply_scan(result);
        app.status = if let Some(err) = errors.first() {
            format!("{} 个读取错误。{err}", errors.len())
        } else {
            format!("扫描 {}", app.roots_label)
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
        self.confirm = false;
        self.now = timeutil::now_unix();
        self.clamp_cursor();
    }

    pub fn visible(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, wt)| {
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
            .map(|(idx, _)| idx)
            .collect()
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        let action = if self.filter_mode {
            self.on_filter_key(code)
        } else if self.confirm {
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
                self.confirm = false;
                Action::Delete { force: false }
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.confirm = false;
                Action::Delete { force: true }
            }
            KeyCode::Esc => {
                self.confirm = false;
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
            self.status = "主检出只作参照，不会删除".to_string();
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
            self.status = "当前没有可删除的 worktree".to_string();
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
            self.status = "先选中要删除的 worktree".to_string();
            return;
        }
        self.confirm = true;
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
    eprintln!("正在扫描 {roots_label} …");
    let result = scan::load(&opts.roots, opts.max_depth, opts.older_than_secs);
    eprintln!(
        "找到 {} 个 worktree（{} 个仓库）",
        result.worktrees.len(),
        result.repos
    );
    if !io::stdout().is_terminal() {
        anyhow::bail!("标准输出不是终端，无法进入交互界面。加上 --list 或 --json");
    }
    let mut app = App::from_scan(result, roots_label);
    let mut terminal = ratatui::try_init().context("无法进入终端界面")?;
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
                app.status = "正在重新扫描…".to_string();
                terminal.draw(|frame| draw(frame, &app))?;
                let result = scan::load(&opts.roots, opts.max_depth, opts.older_than_secs);
                app.apply_scan(result);
                app.status = format!("已刷新，{} 个 worktree", app.items.len());
            }
            Action::Delete { force } => {
                let paths: Vec<_> = app.selected.iter().cloned().collect();
                app.status = "正在删除…".to_string();
                terminal.draw(|frame| draw(frame, &app))?;
                let message = remove::summarize(&remove::delete_many(&app.items, &paths, force));
                let result = scan::load(&opts.roots, opts.max_depth, opts.older_than_secs);
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
    let foot_h = if app.confirm { 12 } else { 6 };
    let chunks =
        Layout::vertical([Constraint::Min(3), Constraint::Length(foot_h)]).split(frame.area());
    draw_table(frame, app, chunks[0]);
    draw_footer(frame, app, chunks[1]);
}

fn draw_table(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let visible = app.visible();
    let shown = if app.filter.is_empty() {
        format!("{} 个", app.items.len())
    } else {
        format!("{}/{}", visible.len(), app.items.len())
    };
    let title = format!(
        " wtrm  {shown} worktree · 已选 {} · {} 个仓库 ",
        app.selected.len(),
        app.repos
    );
    let header = Row::new(["", "活跃", "仓库", "分支", "文件夹", "状态", "路径"])
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
    match wt.last_active {
        Some(ts) if now.saturating_sub(ts) >= 86_400 * 14 => Style::new().fg(Color::Red),
        Some(_) => Style::new(),
        None => Style::new().fg(Color::Red),
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let lines = if app.confirm {
        confirm_lines(app)
    } else {
        browse_lines(app)
    };
    let title = if app.confirm {
        " 确认删除 "
    } else if app.filter_mode {
        " 过滤 "
    } else {
        " 说明 "
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
                .unwrap_or_else(|| "未知".to_string());
            format!(
                "{}  ·  {}  ·  活跃 {when}  ·  {}",
                scan::shorten_path(&wt.path),
                wt.branch,
                wt.flags()
            )
        }
        None => "没有发现带附加 worktree 的仓库".to_string(),
    };
    let filter = if app.filter.is_empty() {
        String::new()
    } else {
        format!("过滤: {}  ", app.filter)
    };
    vec![
        Line::from(detail),
        Line::from(Span::styled(
            "活跃 = 最近提交、index、目录修改时间中的最新值。主检出不可删除。",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(format!("{filter}{}", app.status)),
        Line::from("j/k 移动   space 多选   a 全选可删   d 删除   / 过滤   r 刷新   q 退出"),
    ]
}

fn confirm_lines(app: &App) -> Vec<Line<'static>> {
    let chosen: Vec<&Worktree> = app
        .selected
        .iter()
        .filter_map(|path| app.items.iter().find(|wt| &wt.path == path))
        .collect();
    let mut lines = vec![Line::from(format!(
        "将删除 {} 个 worktree。Enter 只删干净的，f 连同脏/锁定的一起强制删除，Esc 取消。",
        chosen.len()
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
        lines.push(Line::from(format!("  …还有 {} 个", chosen.len() - 6)));
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
        assert!(app.confirm);
        assert_eq!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Delete { force: false }
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
            Action::Delete { force: true }
        );
    }
}
