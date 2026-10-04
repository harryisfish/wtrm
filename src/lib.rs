mod git;
mod ops;
mod remove;
mod report;
mod scan;
mod timeutil;
mod ui;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};

use crate::remove::BranchScope;
use crate::report::{ActionItem, Report};
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
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    scan: ScanArgs,

    /// Print a table instead of the interactive UI
    #[arg(long, global = true)]
    list: bool,

    /// Print JSON
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Args, Debug, Clone)]
struct ScanArgs {
    /// Directories to scan. Defaults to ~/project, ~/Projects, and other existing dev folders
    #[arg(value_name = "DIR", global = true)]
    paths: Vec<PathBuf>,

    /// How many levels to walk below each root
    #[arg(long, default_value_t = 8, global = true)]
    max_depth: u8,

    /// Scan the home directory, still skipping dependency and system folders
    #[arg(long, global = true)]
    all: bool,

    /// Keep only worktrees already merged into the default branch
    #[arg(long, global = true)]
    merged: bool,

    /// Keep only worktrees idle for this long, for example 14d, 48h, 2w
    #[arg(
        long,
        visible_alias = "older-than",
        value_name = "DURATION",
        global = true
    )]
    inactive: Option<String>,

    /// Keep only worktrees created at least this long ago, for example 30d
    #[arg(long, value_name = "DURATION", global = true)]
    created_before: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print a dry-run delete plan for matching worktrees
    Plan,
    /// Delete worktrees by --path or --id (dry-run unless --yes)
    Delete(DeleteArgs),
    /// Remove dependency directories by --path or --id (dry-run unless --yes)
    Clean(CleanArgs),
}

#[derive(Args, Debug, Clone)]
struct DeleteArgs {
    /// Worktree path (repeatable)
    #[arg(long)]
    path: Vec<PathBuf>,

    /// Stable worktree id (repeatable). An 8-character prefix is enough when unique
    #[arg(long)]
    id: Vec<String>,

    /// Execute instead of dry-run
    #[arg(long)]
    yes: bool,

    /// Show the plan without executing (default)
    #[arg(long)]
    dry_run: bool,

    /// Force delete dirty or locked worktrees
    #[arg(long)]
    force: bool,

    /// Also delete the branch: keep, local, or github
    #[arg(long, default_value = "keep", value_parser = parse_branches)]
    branches: BranchScope,
}

#[derive(Args, Debug, Clone)]
struct CleanArgs {
    /// Worktree path (repeatable)
    #[arg(long)]
    path: Vec<PathBuf>,

    /// Stable worktree id (repeatable). An 8-character prefix is enough when unique
    #[arg(long)]
    id: Vec<String>,

    /// Execute instead of dry-run
    #[arg(long)]
    yes: bool,

    /// Show the plan without executing (default)
    #[arg(long)]
    dry_run: bool,
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Plan) => run_plan(&cli),
        Some(Command::Delete(args)) => run_delete(&cli, args),
        Some(Command::Clean(args)) => run_clean(&cli, args),
        None => run_scan_or_tui(&cli),
    }
}

fn run_scan_or_tui(cli: &Cli) -> anyhow::Result<()> {
    let query = query_from(&cli.scan)?;
    let roots = resolve_roots(&cli.scan)?;
    let opts = ui::Options {
        roots: roots.clone(),
        max_depth: cli.scan.max_depth,
        query: query.clone(),
        inactive_secs: query.inactive_for.unwrap_or(14 * DAY),
        stale_created_secs: query.created_before.unwrap_or(30 * DAY),
        stale_idle_secs: if query.created_before.is_some() {
            query.inactive_for.unwrap_or(7 * DAY)
        } else {
            7 * DAY
        },
    };
    if cli.json {
        let (result, ms) = timed_load(&opts.roots, opts.max_depth, &opts.query);
        return emit_report(
            &Report::from_scan(&opts.roots, result).with_scan_ms(ms),
            true,
        );
    }
    if cli.list || !io::stdout().is_terminal() {
        let (result, ms) = timed_load(&opts.roots, opts.max_depth, &opts.query);
        note_scan_time(ms, &result);
        print_table(&result);
        report_errors(&result);
        return Ok(());
    }
    ui::browse(opts)
}

fn run_plan(cli: &Cli) -> anyhow::Result<()> {
    let (roots, result, ms) = load_scan(cli)?;
    let items = ops::plan_delete(&result.worktrees);
    emit_report(
        &Report::from_scan(&roots, result)
            .with_scan_ms(ms)
            .with_items(items),
        cli.json,
    )
}

fn run_delete(cli: &Cli, args: &DeleteArgs) -> anyhow::Result<()> {
    require_targets(&args.path, &args.id)?;
    let execute = args.yes && !args.dry_run;
    let (roots, result, ms) = load_scan_with_targets(cli, &args.path)?;
    let items = ops::delete_targets(
        &result.worktrees,
        &args.path,
        &args.id,
        execute,
        args.force,
        args.branches,
    );
    let report = Report::from_scan(&roots, result)
        .with_scan_ms(ms)
        .with_items(items);
    emit_report(&report, cli.json)?;
    fail_if_needed(&report, execute)
}

fn run_clean(cli: &Cli, args: &CleanArgs) -> anyhow::Result<()> {
    require_targets(&args.path, &args.id)?;
    let execute = args.yes && !args.dry_run;
    let (roots, result, ms) = load_scan_with_targets(cli, &args.path)?;
    let items = ops::clean_targets(&result.worktrees, &args.path, &args.id, execute);
    let report = Report::from_scan(&roots, result)
        .with_scan_ms(ms)
        .with_items(items);
    emit_report(&report, cli.json)?;
    fail_if_needed(&report, execute)
}

fn require_targets(paths: &[PathBuf], ids: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() && ids.is_empty() {
        bail!("pass --path or --id; use wtrm plan --json to list targets");
    }
    Ok(())
}

fn fail_if_needed(report: &Report, execute: bool) -> anyhow::Result<()> {
    if report.failed() {
        bail!("one or more actions failed");
    }
    if execute && report.blocked_or_failed_on_execute() {
        bail!("one or more actions were blocked");
    }
    Ok(())
}

fn load_scan(cli: &Cli) -> anyhow::Result<(Vec<PathBuf>, ScanResult, u64)> {
    let query = query_from(&cli.scan)?;
    let roots = resolve_roots(&cli.scan)?;
    let (result, ms) = timed_load(&roots, cli.scan.max_depth, &query);
    Ok((roots, result, ms))
}

fn load_scan_with_targets(
    cli: &Cli,
    targets: &[PathBuf],
) -> anyhow::Result<(Vec<PathBuf>, ScanResult, u64)> {
    let mut roots = resolve_roots(&cli.scan)?;
    for path in targets {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty()
                && !roots
                    .iter()
                    .any(|root| parent.starts_with(root) || root.starts_with(parent))
            {
                roots.push(parent.to_path_buf());
            }
        }
    }
    let (result, ms) = timed_load(&roots, cli.scan.max_depth, &Query::default());
    Ok((roots, result, ms))
}

fn timed_load(roots: &[PathBuf], max_depth: u8, query: &Query) -> (ScanResult, u64) {
    let start = Instant::now();
    let result = scan::load(roots, max_depth, query);
    (result, start.elapsed().as_millis() as u64)
}

fn note_scan_time(ms: u64, result: &ScanResult) {
    if io::stderr().is_terminal() {
        eprintln!(
            "wtrm: scanned {} worktrees in {:.2}s",
            result.worktrees.len(),
            ms as f32 / 1000.0
        );
    }
}

fn query_from(scan: &ScanArgs) -> anyhow::Result<Query> {
    let inactive_for = match &scan.inactive {
        Some(raw) => Some(
            timeutil::parse_duration(raw)
                .context("cannot parse --inactive; try 14d, 48h, or 2w")?,
        ),
        None => None,
    };
    let created_before = match &scan.created_before {
        Some(raw) => Some(
            timeutil::parse_duration(raw)
                .context("cannot parse --created-before; try 30d or 8w")?,
        ),
        None => None,
    };
    Ok(Query {
        inactive_for,
        created_before,
        merged_only: scan.merged,
    })
}

fn resolve_roots(scan: &ScanArgs) -> anyhow::Result<Vec<PathBuf>> {
    if scan.all {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("home directory not found, cannot use --all")?;
        return Ok(vec![home]);
    }
    if !scan.paths.is_empty() {
        return Ok(scan.paths.clone());
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

fn emit_report(report: &Report, json: bool) -> anyhow::Result<()> {
    if json {
        serde_json::to_writer_pretty(io::stdout(), report)?;
        println!();
        return Ok(());
    }
    if report.items.is_empty() {
        print_table(&ScanResult {
            repos: report.scanned_repos,
            worktrees: report.worktrees.clone(),
            errors: report.errors.clone(),
        });
    } else {
        print_items(&report.items);
    }
    for err in &report.errors {
        eprintln!("wtrm: {err}");
    }
    Ok(())
}

fn print_items(items: &[ActionItem]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "action\tresult\tblocked_by\tid\tpath\treason");
    for item in items {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            item.action,
            item.result,
            item.blocked_by.as_deref().unwrap_or(""),
            item.id,
            scan::shorten_path(&item.path),
            item.reason
        );
    }
    if items.is_empty() {
        let _ = writeln!(out, "no matching worktrees");
    }
}

fn print_table(result: &ScanResult) {
    let now = timeutil::now_unix();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "id\tactive\trepo\tbranch\tstatus\tfolder\tpath");
    for wt in &result.worktrees {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            wt.id_short(),
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

fn parse_branches(raw: &str) -> Result<BranchScope, String> {
    match raw.to_ascii_lowercase().as_str() {
        "keep" => Ok(BranchScope::Keep),
        "local" => Ok(BranchScope::Local),
        "github" | "gh" | "local+github" => Ok(BranchScope::GitHub),
        _ => Err("use keep, local, or github".to_string()),
    }
}
