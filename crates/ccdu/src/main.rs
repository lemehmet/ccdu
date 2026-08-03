//! The `ccdu` command line entry point.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use ccdu_core::format::{format_time, human_size};
use ccdu_core::model::{flags, NodeId, Tree, ROOT};
use ccdu_core::plan::store::Store;
use ccdu_core::plan::{validate, Severity, ValidateOptions};
use ccdu_core::scan::{scan, Progress, ScanOptions};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ccdu",
    version,
    about = "Staged, resumable disk-usage analyzer and cleaner",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    scan: ScanArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory and print a usage summary instead of opening the browser.
    Scan(ScanArgs),

    /// Inspect saved plans.
    Plan {
        #[command(subcommand)]
        cmd: PlanCmd,
    },
}

#[derive(Subcommand)]
enum PlanCmd {
    /// List saved plans, newest first.
    #[command(alias = "ls")]
    List,
    /// Print a plan's operations.
    Show { id: String },
    /// Re-check a plan against the current state of the filesystem.
    Validate {
        id: String,
        /// Permit operations on paths outside the scanned tree.
        #[arg(long)]
        allow_outside: bool,
    },
    /// Delete a saved plan. Removes the plan, never the files it names.
    #[command(alias = "remove")]
    Rm { id: String },
}

#[derive(Args, Clone)]
struct ScanArgs {
    /// Directory to scan.
    path: Option<PathBuf>,

    /// Stay on one filesystem: do not descend into other mounts.
    #[arg(short = 'x', long)]
    one_file_system: bool,

    /// Number of scanning threads.
    #[arg(short = 't', long, value_name = "N")]
    threads: Option<usize>,

    /// Skip entries with this exact name. Repeatable.
    #[arg(long = "exclude", value_name = "NAME")]
    excludes: Vec<String>,

    /// Report apparent size (st_size) instead of actual disk usage.
    #[arg(short = 'a', long)]
    apparent: bool,

    /// How many of the largest entries to list.
    #[arg(long, default_value_t = 20, value_name = "N")]
    top: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Scan(args)) => report(args),
        Some(Command::Plan { cmd }) => plan_command(cmd),
        None => browse(cli.scan),
    }
}

fn plan_command(cmd: PlanCmd) -> Result<()> {
    let store = Store::open_default();
    match cmd {
        PlanCmd::List => {
            let plans = store.list()?;
            if plans.is_empty() {
                println!("no plans in {}", store.dir().display());
                return Ok(());
            }
            for p in plans {
                println!(
                    "{}  {}  {:>4} ops  {:>10} to reclaim  {}",
                    p.id,
                    format_time(p.created),
                    p.ops,
                    human_size(p.delete_bytes),
                    p.root.display()
                );
            }
            Ok(())
        }

        PlanCmd::Show { id } => {
            let plan = store.load(&id).with_context(|| format!("loading plan {id}"))?;
            println!("id       {}", plan.id);
            println!("created  {}", format_time(plan.created));
            println!("host     {}", plan.host);
            println!("root     {}", plan.root.display());
            println!("reclaims {}", human_size(plan.delete_bytes()));
            if plan.move_bytes() > 0 {
                println!("moves    {}", human_size(plan.move_bytes()));
            }
            println!();
            for (i, op) in plan.ops.iter().enumerate() {
                println!("{i:>4}  {:>10}  {}", human_size(op.est_bytes()), op.summary());
            }
            Ok(())
        }

        PlanCmd::Validate { id, allow_outside } => {
            let plan = store.load(&id).with_context(|| format!("loading plan {id}"))?;
            let opts = ValidateOptions { allow_outside_root: allow_outside, ..Default::default() };
            let findings = validate(&plan, &opts);

            let errors = findings.iter().filter(|f| f.severity == Severity::Error).count();
            for f in &findings {
                let mark = if f.severity == Severity::Error { "error" } else { "warn " };
                match f.op {
                    Some(i) => println!("{mark}  #{i}  {}", f.message),
                    None => println!("{mark}       {}", f.message),
                }
            }
            if findings.is_empty() {
                println!("ok: {} operations, nothing to report", plan.ops.len());
            }
            if errors > 0 {
                // A non-zero status so this is usable from a script.
                std::process::exit(1);
            }
            Ok(())
        }

        PlanCmd::Rm { id } => {
            store.remove(&id).with_context(|| format!("removing plan {id}"))?;
            println!("removed plan {id}");
            Ok(())
        }
    }
}

/// Default behaviour: scan and open the browser.
fn browse(args: ScanArgs) -> Result<()> {
    let (path, opts) = prepare(&args)?;
    ccdu_tui::run(path, opts).context("terminal interface")
}

/// Resolve the scan root and turn command line flags into scan options.
fn prepare(args: &ScanArgs) -> Result<(PathBuf, ScanOptions)> {
    let path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let path = path.canonicalize().with_context(|| format!("cannot access {}", path.display()))?;

    let mut opts = ScanOptions {
        one_file_system: args.one_file_system,
        exclude_names: args.excludes.iter().map(|e| e.as_bytes().to_vec()).collect(),
        ..Default::default()
    };
    if let Some(threads) = args.threads {
        opts.threads = threads.max(1);
    }
    Ok((path, opts))
}

/// Headless mode: scan and print a summary, for scripts and for machines without a usable
/// terminal.
fn report(args: ScanArgs) -> Result<()> {
    let (path, opts) = prepare(&args)?;

    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
    let reporter = std::thread::spawn(move || report_progress(rx));

    let started = Instant::now();
    let tree = scan(&path, &opts, Some(&tx), None)
        .with_context(|| format!("scanning {}", path.display()))?;
    drop(tx);
    reporter.join().ok();

    print_report(&tree, &args, started.elapsed());
    Ok(())
}

/// Overwrite a single status line while the scan runs, then clear it.
fn report_progress(rx: crossbeam_channel::Receiver<Progress>) {
    let mut stderr = std::io::stderr();
    let interactive = std::io::IsTerminal::is_terminal(&stderr);
    let mut wrote = false;
    for p in rx {
        if !interactive {
            continue;
        }
        wrote = true;
        let _ = write!(
            stderr,
            "\r\x1b[2Kscanning {} entries in {} dirs, {}",
            p.entries,
            p.dirs,
            human_size(p.disk)
        );
        let _ = stderr.flush();
    }
    if wrote {
        let _ = write!(stderr, "\r\x1b[2K");
        let _ = stderr.flush();
    }
}

fn print_report(tree: &Tree, args: &ScanArgs, elapsed: std::time::Duration) {
    let size_of = |id: NodeId| {
        let n = tree.node(id);
        if args.apparent {
            n.apparent
        } else {
            n.disk
        }
    };

    let root = tree.node(ROOT);
    println!("{}", tree.root_path().display());
    println!(
        "  {:<12} {}",
        if args.apparent { "apparent" } else { "disk usage" },
        human_size(size_of(ROOT))
    );
    println!("  {:<12} {}", "entries", root.items);
    println!(
        "  {:<12} {:.2}s ({} nodes, {} of arena)",
        "scanned in",
        elapsed.as_secs_f64(),
        tree.len(),
        human_size(tree.memory_bytes() as u64)
    );
    if tree.errors > 0 {
        println!("  {:<12} {} (sizes are a lower bound)", "unreadable", tree.errors);
    }

    let mut children: Vec<NodeId> = tree.children(ROOT).collect();
    if children.is_empty() {
        return;
    }
    children.sort_unstable_by_key(|&id| std::cmp::Reverse(size_of(id)));

    println!();
    for &id in children.iter().take(args.top) {
        let node = tree.node(id);
        let marker = if node.is_dir() { "/" } else { "" };
        println!(
            "{:>10}  {}{}{}",
            human_size(size_of(id)),
            tree.name(id).to_string_lossy(),
            marker,
            annotations(node.flags)
        );
    }
    if children.len() > args.top {
        println!("... and {} more", children.len() - args.top);
    }
}

/// Explain why an entry's size may not be what you expect.
fn annotations(f: u16) -> String {
    let mut notes = Vec::new();
    if f & flags::ERR != 0 {
        notes.push("unreadable");
    }
    if f & flags::EXCLUDED != 0 {
        notes.push("excluded");
    }
    if f & flags::OTHER_FS != 0 {
        notes.push("other filesystem");
    }
    if f & flags::HARDLINK_DUP != 0 {
        notes.push("hardlink, counted elsewhere");
    }
    if f & flags::LOOP != 0 {
        notes.push("already visited");
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!("  [{}]", notes.join(", "))
    }
}
