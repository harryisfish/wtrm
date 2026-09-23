mod remove;
mod scan;
mod timeutil;
mod ui;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use crate::scan::{Query, ScanResult};
use crate::timeutil::age_label;

const DAY: i64 = 86_400;

#[derive(Parser, Debug)]
#[command(
    name = "wtrm",
    version,
    about = "扫描本机的 git worktree，查看活跃程度并多选删除"
)]
struct Cli {
    /// 要扫描的目录。缺省时使用 ~/project、~/Projects 等已存在的开发目录
    #[arg(value_name = "DIR")]
    paths: Vec<PathBuf>,

    /// 每个根目录向下寻找仓库的最大层数
    #[arg(long, default_value_t = 8)]
    max_depth: u8,

    /// 扫描整个主目录（仍跳过依赖目录和系统目录）
    #[arg(long)]
    all: bool,

    /// 只打印，不进入交互界面
    #[arg(long)]
    list: bool,

    /// 以 JSON 打印
    #[arg(long)]
    json: bool,

    /// 只保留已经合并进默认分支的 worktree
    #[arg(long)]
    merged: bool,

    /// 只保留这么久没活跃的 worktree，例如 14d、48h、2w
    #[arg(long, visible_alias = "older-than", value_name = "DURATION")]
    inactive: Option<String>,

    /// 只保留至少这么久以前创建的 worktree，例如 30d
    #[arg(long, value_name = "DURATION")]
    created_before: Option<String>,
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let inactive_for = match &cli.inactive {
        Some(raw) => {
            Some(timeutil::parse_duration(raw).context("无法解析 --inactive，示例：14d、48h、2w")?)
        }
        None => None,
    };
    let created_before = match &cli.created_before {
        Some(raw) => Some(
            timeutil::parse_duration(raw).context("无法解析 --created-before，示例：30d、8w")?,
        ),
        None => None,
    };
    let query = Query {
        inactive_for,
        created_before,
        merged_only: cli.merged,
    };
    let roots = resolve_roots(&cli)?;
    let opts = ui::Options {
        roots,
        max_depth: cli.max_depth,
        query,
        inactive_secs: inactive_for.unwrap_or(14 * DAY),
        stale_created_secs: created_before.unwrap_or(30 * DAY),
        stale_idle_secs: if created_before.is_some() {
            inactive_for.unwrap_or(7 * DAY)
        } else {
            7 * DAY
        },
    };
    if cli.json {
        let result = scan::load(&opts.roots, opts.max_depth, &opts.query);
        serde_json::to_writer_pretty(io::stdout(), &result.worktrees)?;
        println!();
        report_errors(&result);
        return Ok(());
    }
    if cli.list || !io::stdout().is_terminal() {
        let result = scan::load(&opts.roots, opts.max_depth, &opts.query);
        print_table(&result);
        report_errors(&result);
        return Ok(());
    }
    ui::browse(opts)
}

fn resolve_roots(cli: &Cli) -> anyhow::Result<Vec<PathBuf>> {
    if cli.all {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("找不到主目录，无法 --all")?;
        return Ok(vec![home]);
    }
    if !cli.paths.is_empty() {
        return Ok(cli.paths.clone());
    }
    let mut roots = default_roots();
    if let Ok(cwd) = std::env::current_dir() {
        if !roots.iter().any(|root| cwd.starts_with(root)) {
            roots.push(cwd);
        }
    }
    if roots.is_empty() {
        roots.push(std::env::current_dir().context("找不到当前目录")?);
    }
    Ok(roots)
}

fn default_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        "project",
        "Projects",
        "Developer",
        "dev",
        "src",
        "code",
        "repos",
        "work",
        "git",
        "workspace",
    ]
    .into_iter()
    .map(|name| home.join(name))
    .filter(|path| path.is_dir())
    .collect()
}

fn print_table(result: &ScanResult) {
    let now = timeutil::now_unix();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "活跃\t仓库\t分支\t状态\t文件夹\t路径");
    for wt in &result.worktrees {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            age_label(wt.last_active, now),
            wt.repo,
            wt.branch,
            wt.flags(),
            scan::shorten_path(&wt.parent),
            scan::shorten_path(&wt.path)
        );
    }
    if result.worktrees.is_empty() {
        let _ = writeln!(out, "没有发现带附加 worktree 的仓库");
    }
}

fn report_errors(result: &ScanResult) {
    for err in &result.errors {
        eprintln!("wtrm: {err}");
    }
}
