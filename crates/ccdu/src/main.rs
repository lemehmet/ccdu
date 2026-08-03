//! The `ccdu` command line entry point.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use ccdu_core::dup::{find_duplicates, shares_inode, total_wasted, DupOptions};
use ccdu_core::exec::journal::Event;
use ccdu_core::exec::{self, Control, ExecEvent, ExecOptions, FaultFn, FaultPoint, Verify};
use ccdu_core::export::{self, Format};
use ccdu_core::format::{format_time, human_size};
use ccdu_core::model::{flags, NodeId, Tree, ROOT};
use ccdu_core::plan::store::Store;
use ccdu_core::plan::{validate, Severity, ValidateOptions};
use ccdu_core::scan::{scan, Progress, ScanOptions};
use ccdu_remote::client::scan_with_ncdu;
use ccdu_remote::{Remote, Target};
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

    /// Serve scan requests on standard input and output. Run over ssh by the remote support; not
    /// meant to be typed.
    #[arg(long, hide = true, exclusive = true)]
    agent: bool,

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

    /// Run a saved plan.
    Apply {
        id: String,
        /// Check everything and report what would happen, without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Permit operations on paths outside the scanned tree.
        #[arg(long)]
        allow_outside: bool,
        /// How hard to check a copied file before its original is removed. `hash` re-reads both
        /// sides and compares digests.
        #[arg(long, value_enum, default_value_t = VerifyArg::Size)]
        verify: VerifyArg,
    },

    /// Continue a plan that was paused or interrupted.
    Resume { id: String },

    /// Report how far a plan's execution got.
    Status { id: String },

    /// Report files with identical contents. `--top` limits how many groups are listed.
    Dupes {
        #[command(flatten)]
        scan: ScanArgs,
        /// Ignore files smaller than this many bytes.
        #[arg(long, default_value_t = 4096, value_name = "BYTES")]
        min_size: u64,
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

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum VerifyArg {
    /// Lengths must match. Catches the truncation an interrupted copy leaves.
    Size,
    /// Read both copies back and compare digests.
    Hash,
}

impl From<VerifyArg> for Verify {
    fn from(v: VerifyArg) -> Verify {
        match v {
            VerifyArg::Size => Verify::Size,
            VerifyArg::Hash => Verify::Hash,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum FormatArg {
    /// ccdu's own format: exact and compact.
    Ccdu,
    /// ncdu's JSON export, readable by `ncdu -f`.
    NcduJson,
}

impl From<FormatArg> for Format {
    fn from(f: FormatArg) -> Format {
        match f {
            FormatArg::Ccdu => Format::Native,
            FormatArg::NcduJson => Format::NcduJson,
        }
    }
}

#[derive(Args, Clone)]
struct ScanArgs {
    /// Directory to scan. `ssh://host/path` or `host:path` scans on another machine.
    path: Option<PathBuf>,

    /// Load a saved scan instead of walking the filesystem. Reads ccdu and ncdu dumps alike;
    /// `-` means standard input.
    #[arg(short = 'f', long = "file", value_name = "FILE", conflicts_with = "path")]
    file: Option<PathBuf>,

    /// Write the scan here instead of showing it. `-` means standard output.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: Option<PathBuf>,

    /// Format for --output.
    #[arg(long, value_enum, default_value_t = FormatArg::Ccdu)]
    format: FormatArg,

    /// Path to ccdu on the remote host. Useful when it is installed somewhere ssh's
    /// non-interactive PATH does not reach.
    #[arg(long, default_value = "ccdu", value_name = "PATH")]
    remote_ccdu: String,

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
    if cli.agent {
        // Nothing but frames may reach stdout from here on.
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        return ccdu_remote::agent::serve(stdin.lock(), stdout.lock()).context("serving as agent");
    }
    match cli.command {
        Some(Command::Scan(args)) => report(args),
        Some(Command::Plan { cmd }) => plan_command(cmd),
        Some(Command::Apply { id, dry_run, yes, allow_outside, verify }) => {
            apply(&id, dry_run, yes, allow_outside, verify.into(), false)
        }
        Some(Command::Resume { id }) => apply(&id, false, true, false, Verify::Size, true),
        Some(Command::Status { id }) => status(&id),
        Some(Command::Dupes { scan, min_size }) => dupes(scan, min_size),
        None => browse(cli.scan),
    }
}

/// Headless duplicate report.
fn dupes(args: ScanArgs, min_size: u64) -> Result<()> {
    let top = args.top;
    let (path, scan_opts) = prepare(&args)?;
    let tree = scan(&path, &scan_opts, None, None)
        .with_context(|| format!("scanning {}", path.display()))?;

    let opts = DupOptions { min_size, threads: scan_opts.threads };
    let groups = find_duplicates(&tree, &opts, None, None);

    if groups.is_empty() {
        println!("no duplicate files under {}", path.display());
        return Ok(());
    }

    println!(
        "{} groups, {} reclaimable by keeping one copy of each\n",
        groups.len(),
        human_size(total_wasted(&groups))
    );
    for group in groups.iter().take(top) {
        println!(
            "{} copies of {} — {} reclaimable",
            group.nodes.len(),
            human_size(group.size),
            human_size(group.wasted)
        );
        for (i, &id) in group.nodes.iter().enumerate() {
            let note = if i == 0 { "  keep" } else { "      " };
            // Removing one name of a hardlinked file frees nothing while its twins remain, so the
            // line says so rather than let the group's total imply otherwise.
            let hardlinked = if shares_inode(&tree, id) {
                "  (hardlinked; removing this frees nothing)"
            } else {
                ""
            };
            println!("{note}  {}{hardlinked}", tree.path_of(id).display());
        }
        println!();
    }
    if groups.len() > top {
        println!("... and {} more groups", groups.len() - top);
    }
    Ok(())
}

/// Set by the signal handler; polled by a watcher thread that pauses the run.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_: libc::c_int) {
    // Async-signal-safe: a relaxed store and nothing else.
    INTERRUPTED.store(true, Ordering::Relaxed);
}

fn status(id: &str) -> Result<()> {
    let store = Store::open_default();
    let plan = store.load(id).with_context(|| format!("loading plan {id}"))?;
    let dir = store.dir_for(id)?;

    let state = exec::state(&dir)?;
    println!("plan   {}", plan.id);
    println!("root   {}", plan.root.display());
    println!("state  {}", describe(state));

    let records = exec::journal::read_dir(&dir)?;
    let done = records.iter().filter(|r| matches!(r.event, Event::OpDone { .. })).count();
    let failed = records.iter().filter(|r| matches!(r.event, Event::OpFailed { .. })).count();
    let freed: u64 = records
        .iter()
        .filter_map(|r| match r.event {
            Event::OpDone { freed, .. } => Some(freed),
            _ => None,
        })
        .sum();

    println!("done   {done} of {} operations, {} reclaimed", plan.ops.len(), human_size(freed));
    if failed > 0 {
        println!("failed {failed}");
        for record in &records {
            if let Event::OpFailed { op, error } = &record.event {
                println!("       #{op}  {error}");
            }
        }
    }
    if state == exec::RunState::Paused || state == exec::RunState::Interrupted {
        println!("\nrun `ccdu resume {}` to continue", plan.id);
    }
    Ok(())
}

fn describe(state: exec::RunState) -> &'static str {
    match state {
        exec::RunState::NotStarted => "not started",
        exec::RunState::Interrupted => "interrupted (crashed or killed)",
        exec::RunState::Paused => "paused",
        exec::RunState::Finished => "finished",
    }
}

fn apply(
    id: &str,
    dry_run: bool,
    yes: bool,
    allow_outside: bool,
    verify: Verify,
    resuming: bool,
) -> Result<()> {
    let store = Store::open_default();
    let plan = store.load(id).with_context(|| format!("loading plan {id}"))?;
    let dir = store.dir_for(id)?;

    let opts = ValidateOptions { allow_outside_root: allow_outside, ..Default::default() };
    let findings = validate(&plan, &opts);
    let errors: Vec<_> = findings.iter().filter(|f| f.severity == Severity::Error).collect();

    // On a resume the entries we already started have moved on by our own hand, so validation's
    // view of them is stale. The executor re-checks each one against the journal, which knows
    // which were in flight; that check is the authority, not this one.
    if !errors.is_empty() && !resuming {
        for f in &errors {
            match f.op {
                Some(i) => eprintln!("error  #{i}  {}", f.message),
                None => eprintln!("error       {}", f.message),
            }
        }
        anyhow::bail!("{} problem(s) block this plan; nothing was changed", errors.len());
    }
    for f in findings.iter().filter(|f| f.severity == Severity::Warning) {
        match f.op {
            Some(i) => eprintln!("warn   #{i}  {}", f.message),
            None => eprintln!("warn        {}", f.message),
        }
    }

    if !yes && !dry_run && !confirm(&plan)? {
        println!("cancelled; nothing was changed");
        return Ok(());
    }

    // SIGINT pauses rather than kills, so Ctrl-C leaves a resumable run instead of an ambiguous
    // one. A second Ctrl-C still terminates, because the handler is not reinstalled.
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_interrupt as *const () as libc::sighandler_t);
    }

    let control = Control::new();
    let finished = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::unbounded::<ExecEvent>();
    let fault = fault_from_env();

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if INTERRUPTED.load(Ordering::Relaxed) {
                    eprintln!("\npausing; run `ccdu resume {}` to continue", plan.id);
                    control.pause();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        scope.spawn(move || {
            for event in rx {
                match event {
                    ExecEvent::Started { index, summary } => println!("  #{index}  {summary}"),
                    ExecEvent::Finished { index, freed } => {
                        println!("  #{index}  done, {} reclaimed", human_size(freed))
                    }
                    ExecEvent::Failed { index, error } => eprintln!("  #{index}  failed: {error}"),
                    ExecEvent::AlreadyDone { index } => println!("  #{index}  already done"),
                }
            }
        });

        let exec_opts = ExecOptions { dry_run, verify, fault: fault.as_ref().map(|f| f.as_ref()) };
        let result = exec::execute(&plan, &dir, &exec_opts, &control, Some(&tx));
        drop(tx);
        finished.store(true, Ordering::Relaxed);
        result
    })?;

    println!();
    if dry_run {
        println!(
            "dry run: {} operations would run, {} would be reclaimed",
            outcome.done,
            human_size(outcome.freed)
        );
    } else if outcome.paused {
        println!(
            "paused after {} operations, {} reclaimed; resume with `ccdu resume {}`",
            outcome.done,
            human_size(outcome.freed),
            plan.id
        );
    } else {
        println!("{} operations done, {} reclaimed", outcome.done, human_size(outcome.freed));
    }
    if outcome.failed > 0 {
        println!("{} failed; see `ccdu status {}`", outcome.failed, plan.id);
        std::process::exit(1);
    }
    Ok(())
}

fn confirm(plan: &ccdu_core::plan::Plan) -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("refusing to run unattended without --yes");
    }
    println!(
        "About to run {} operations under {}, reclaiming about {}.",
        plan.ops.len(),
        plan.root.display(),
        human_size(plan.delete_bytes())
    );
    if plan.move_bytes() > 0 {
        println!("{} will be moved.", human_size(plan.move_bytes()));
    }
    println!("This cannot be undone.");
    print!("Continue? [y/N] ");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Testing aid: `CCDU_FAULT=<point>:<op>` aborts the process at that journal boundary, so the
/// crash-recovery path can be exercised against a real killed process rather than a simulated one.
fn fault_from_env() -> Option<Box<FaultFn<'static>>> {
    let spec = std::env::var("CCDU_FAULT").ok()?;
    let (name, index) = spec.split_once(':')?;
    let target: usize = index.parse().ok()?;
    let point = match name {
        "before_op_begin" => FaultPoint::BeforeOpBegin,
        "after_op_begin" => FaultPoint::AfterOpBegin,
        "mid_delete" => FaultPoint::MidDelete,
        "mid_copy" => FaultPoint::MidCopy,
        "before_source_removal" => FaultPoint::BeforeSourceRemoval,
        "before_op_done" => FaultPoint::BeforeOpDone,
        "after_op_done" => FaultPoint::AfterOpDone,
        _ => return None,
    };
    Some(Box::new(move |p, op| {
        if p == point && op == target {
            // Not a panic: this has to look like the machine going away, with no unwinding and no
            // chance to flush anything.
            std::process::abort();
        }
        Ok(())
    }))
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

/// Fetch a tree from another machine, preferring its ccdu and falling back to its ncdu.
fn scan_remote(target: &Target, args: &ScanArgs) -> Result<ccdu_core::model::Tree> {
    let threads = args.threads.unwrap_or(8);
    let exclude = args.excludes.clone();

    match Remote::connect(target.agent_command(&args.remote_ccdu)) {
        Ok(mut remote) => {
            eprintln!("scanning {} on {} (ccdu {})", target.path, remote.host, remote.version);
            let tree = remote
                .scan(&target.path, args.one_file_system, threads, exclude, None)
                .with_context(|| format!("scanning {} on {}", target.path, target.host))?;
            Ok(tree)
        }
        Err(e) => {
            // A host with no ccdu on it is the common case, not a failure. ncdu is the fallback
            // because it is the tool that is already there.
            eprintln!("no ccdu agent on {} ({e}); trying ncdu", target.host);
            scan_with_ncdu(target)
                .with_context(|| format!("scanning {} on {} with ncdu", target.path, target.host))
        }
    }
}

/// Default behaviour: scan and open the browser.
fn browse(args: ScanArgs) -> Result<()> {
    // Writing a scan and browsing one are different jobs; `-o` means the former even without the
    // `scan` subcommand, so `ccdu /path -o dump` does what it looks like.
    if args.output.is_some() {
        return report(args);
    }
    if let Some(file) = args.file.clone() {
        let tree = load(&file)?;
        let why = format!(
            "loaded from {}; stage against a live scan, or run ccdu where the files are",
            file.display()
        );
        return ccdu_tui::browse_tree(tree, Some(why)).context("terminal interface");
    }
    if let Some(target) = remote_target(&args) {
        let tree = scan_remote(&target, &args)?;
        let why = format!(
            "these files are on {}; run `ccdu {}` there to change them",
            target.host, target.path
        );
        return ccdu_tui::browse_tree(tree, Some(why)).context("terminal interface");
    }
    let (path, opts) = prepare(&args)?;
    ccdu_tui::run(path, opts).context("terminal interface")
}

/// A path argument that names another machine rather than a local directory.
fn remote_target(args: &ScanArgs) -> Option<Target> {
    Target::parse(args.path.as_ref()?.to_str()?)
}

fn load(file: &std::path::Path) -> Result<ccdu_core::model::Tree> {
    export::read_path(file).with_context(|| format!("reading {}", file.display()))
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
    let started = Instant::now();
    let tree = if let Some(file) = &args.file {
        load(file)?
    } else if let Some(target) = remote_target(&args) {
        scan_remote(&target, &args)?
    } else {
        let (path, opts) = prepare(&args)?;
        let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
        let reporter = std::thread::spawn(move || report_progress(rx));
        let tree = scan(&path, &opts, Some(&tx), None)
            .with_context(|| format!("scanning {}", path.display()))?;
        drop(tx);
        reporter.join().ok();
        tree
    };

    if let Some(output) = &args.output {
        let format: Format = args.format.into();
        export::write_path(&tree, output, format)
            .with_context(|| format!("writing {}", output.display()))?;
        // To stderr, so `-o -` stays a clean pipe.
        eprintln!(
            "wrote {} entries as {} to {}",
            tree.node(ROOT).items,
            format.name(),
            output.display()
        );
        return Ok(());
    }

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
