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
    about = "Scan git worktrees, show how active they are, and delete a selection"
)]
struct Cli {
    /// Directories to scan. Defaults to ~/project, ~/Projects, and other existing dev folders
    #[arg(value_name = "DIR")]
    paths: Vec<PathBuf>,

    /// How many levels to walk below each root
    #[arg(long, default_value_t = 8)]
    max_depth: u8,

    /// Scan the home directory, still skipping dependency and system folders
    #[arg(long)]
    all: bool,

    /// Print a table instead of the interactive UI
    #[arg(long)]
    list: bool,

    /// Print JSON
    #[arg(long)]
    json: bool,

    /// Keep only worktrees already merged into the default branch
    #[arg(long)]
    merged: bool,

    /// Keep only worktrees idle for this long, for example 14d, 48h, 2w
    #[arg(long, visible_alias = "older-than", value_name = "DURATION")]
    inactive: Option<String>,

    /// Keep only worktrees created at least this long ago, for example 30d
    #[arg(long, value_name = "DURATION")]
    created_before: Option<String>,
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let inactive_for = match &cli.inactive {
        Some(raw) => Some(
            timeutil::parse_duration(raw)
                .context("cannot parse --inactive; try 14d, 48h, or 2w")?,
        ),
        None => None,
    };
    let created_before = match &cli.created_before {
        Some(raw) => Some(
            timeutil::parse_duration(raw)
                .context("cannot parse --created-before; try 30d or 8w")?,
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
            .context("home directory not found, cannot use --all")?;
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
        roots.push(std::env::current_dir().context("current directory not found")?);
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
    let _ = writeln!(out, "active\trepo\tbranch\tstatus\tfolder\tpath");
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
        let _ = writeln!(out, "no repos with linked worktrees");
    }
}

fn report_errors(result: &ScanResult) {
    for err in &result.errors {
        eprintln!("wtrm: {err}");
    }
}
