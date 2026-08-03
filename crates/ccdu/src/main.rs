//! The `ccdu` command line entry point.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use ccdu_core::model::{flags, NodeId, Tree, ROOT};
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
    /// Scan a directory and print a usage summary.
    Scan(ScanArgs),
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
    let args = match cli.command {
        Some(Command::Scan(args)) => args,
        None => cli.scan,
    };
    run_scan(args)
}

fn run_scan(args: ScanArgs) -> Result<()> {
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

    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
    let reporter = std::thread::spawn(move || report_progress(rx));

    let started = Instant::now();
    let tree =
        scan(&path, &opts, Some(&tx)).with_context(|| format!("scanning {}", path.display()))?;
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
            human(p.disk)
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
        human(size_of(ROOT))
    );
    println!("  {:<12} {}", "entries", root.items);
    println!(
        "  {:<12} {:.2}s ({} nodes, {} of arena)",
        "scanned in",
        elapsed.as_secs_f64(),
        tree.len(),
        human(tree.memory_bytes() as u64)
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
            human(size_of(id)),
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

fn human(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
