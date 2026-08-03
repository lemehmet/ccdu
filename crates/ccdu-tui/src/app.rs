//! Browser state and input handling.
//!
//! Kept free of rendering so it can be driven from a test without a terminal. Staging lives here
//! too: marking and staging never touch the filesystem beyond a single `stat` to record what the
//! user is looking at, so everything up to the moment a plan is committed is reversible.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ccdu_core::config::Config;
use ccdu_core::dup::{self, DupGroup, DupOptions, DupProgress};
use ccdu_core::exec::{self, Control, ExecEvent, ExecOptions, Outcome};
use ccdu_core::format::human_size;
use ccdu_core::model::{flags, NodeId, Tree, ROOT};
use ccdu_core::plan::store::Store;
use ccdu_core::plan::{validate, Conflict, Finding, Ident, Op, Plan, Severity, ValidateOptions};
use ccdu_remote::Remote;
use crossbeam_channel::Receiver;

/// Which column the listing is ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Size,
    Name,
    Items,
    Mtime,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Size => "size",
            Sort::Name => "name",
            Sort::Items => "items",
            Sort::Mtime => "mtime",
        }
    }
}

/// How much of the row is given over to the bar graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Graph {
    Off,
    Bar,
    Percent,
    Both,
}

impl Graph {
    fn next(self) -> Graph {
        match self {
            Graph::Off => Graph::Bar,
            Graph::Bar => Graph::Percent,
            Graph::Percent => Graph::Both,
            Graph::Both => Graph::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Browse,
    /// Files with identical contents, worst waste first.
    Dupes,
    Plan,
    /// The last chance to back out. Deliberately its own screen rather than a keystroke away from
    /// the plan, because this is the only irreversible thing ccdu does.
    Confirm,
    Running,
}

/// A commit in progress, or the record of one that finished.
pub struct Running {
    control: Arc<Control>,
    events: Receiver<ExecEvent>,
    result: Receiver<io::Result<Outcome>>,
    /// What has happened so far, newest last.
    pub log: Vec<String>,
    pub total: usize,
    pub completed: usize,
    /// Set once the run ends, whether it finished, paused or failed.
    pub summary: Option<String>,
    pub plan_id: String,
    /// Running on another machine, where our pause switch does not reach.
    pub remote: bool,
}

impl Running {
    pub fn is_finished(&self) -> bool {
        self.summary.is_some()
    }

    pub fn pause(&self) {
        self.control.pause();
    }

    pub fn is_pausing(&self) -> bool {
        self.control.is_paused() && self.summary.is_none()
    }
}

/// A duplicate scan, running or finished.
pub struct Dupes {
    pub groups: Vec<DupGroup>,
    /// Flattened for display: a header per group, then its files.
    pub rows: Vec<DupRow>,
    pub cursor: usize,
    pub latest: DupProgress,
    result: Option<Receiver<Vec<DupGroup>>>,
    progress: Receiver<DupProgress>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DupRow {
    Header(usize),
    File(usize, NodeId),
}

impl Dupes {
    pub fn is_scanning(&self) -> bool {
        self.result.is_some()
    }

    /// The file under the cursor, if the cursor is on one.
    pub fn selected(&self) -> Option<(usize, NodeId)> {
        match self.rows.get(self.cursor) {
            Some(DupRow::File(group, id)) => Some((*group, *id)),
            _ => None,
        }
    }

    pub fn stop(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A change staged against one entry. Holds the identity read at staging time, which is what the
/// executor will re-check before it acts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub kind: StagedKind,
    pub ident: Ident,
    pub est_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StagedKind {
    Delete,
    Move(PathBuf),
}

/// A single-line text prompt, currently only used to ask for a move destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub label: String,
    pub input: String,
    /// Entries the answer will apply to.
    pub targets: Vec<NodeId>,
}

/// A key press translated into an intent, so tests never mention crossterm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Enter,
    Leave,
    Sort(Sort),
    ToggleApparent,
    CycleGraph,
    ToggleInfo,
    ToggleHelp,
    Mark,
    StageDelete,
    StageMove,
    Unstage,
    TogglePlan,
    SavePlan,
    Commit,
    Confirm,
    ToggleTreemap,
    ToggleDupes,
    /// Stage every copy in the selected group except the one being kept.
    StageGroupRest,
    Input(char),
    Backspace,
    Submit,
    Dismiss,
    Quit,
}

pub struct App {
    pub tree: Arc<Tree>,
    pub dir: NodeId,
    pub cursor: usize,
    pub rows: Vec<NodeId>,
    pub sort: Sort,
    pub reverse: bool,
    pub apparent: bool,
    pub graph: Graph,
    pub show_help: bool,
    pub show_info: bool,
    pub quit: bool,

    pub view: View,
    /// Multi-select. Empty means "act on the cursor".
    pub marks: HashSet<NodeId>,
    pub staged: HashMap<NodeId, Staged>,
    pub prompt: Option<Prompt>,
    /// One-line feedback, shown until the next action replaces it.
    pub status: Option<String>,

    /// Built when the plan view is opened, alongside the nodes each operation came from.
    pub plan: Plan,
    pub plan_nodes: Vec<NodeId>,
    pub findings: Vec<Finding>,
    pub plan_cursor: usize,
    /// Where `w` writes. Injected rather than looked up at save time so tests do not have to
    /// reach for a process-wide environment variable.
    pub store: Store,
    pub run: Option<Running>,
    /// Set once a commit has removed things: the tree in memory no longer describes the disk.
    pub stale: bool,
    /// Show the treemap beside the listing.
    pub show_treemap: bool,
    pub dupes: Option<Dupes>,
    /// Why this tree cannot be changed from here, if it cannot. Set for trees that were loaded or
    /// fetched rather than scanned: their paths describe another machine or another moment.
    pub read_only: Option<String>,
    /// The machine holding these files, when it is not this one. Staging asks it what entries look
    /// like, and committing runs there — the plan and its journal live where the files do, so an
    /// interrupted commit is resumable on the machine that was doing the work.
    pub remote: Option<Arc<Mutex<Remote>>>,
    /// Paths the user has put out of reach, built-in list plus their own.
    protected: Vec<PathBuf>,
    headroom: f64,

    remembered: HashMap<NodeId, usize>,
}

/// Move the cursor to the next row holding a file, skipping group headers. Stays put when there
/// is nowhere to go.
fn next_file(rows: &[DupRow], from: usize, step: isize) -> usize {
    let mut at = from as isize;
    loop {
        at += step;
        if at < 0 || at as usize >= rows.len() {
            return from;
        }
        if matches!(rows[at as usize], DupRow::File(..)) {
            return at as usize;
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

impl App {
    pub fn new(tree: Tree) -> Self {
        App::with_config(tree, Config::load().unwrap_or_default())
    }

    /// Build with an explicit configuration, so tests need not touch the user's.
    pub fn with_config(tree: Tree, config: Config) -> Self {
        let root = tree.root_path().to_path_buf();
        let mut app = App {
            // Shared so a duplicate scan can read it from another thread while the browser keeps
            // drawing. Nothing mutates the tree after the scan that produced it.
            tree: Arc::new(tree),
            dir: ROOT,
            cursor: 0,
            rows: Vec::new(),
            sort: Sort::Size,
            reverse: false,
            apparent: config.scan.apparent,
            graph: Graph::Both,
            show_help: false,
            show_info: false,
            quit: false,
            view: View::Browse,
            marks: HashSet::new(),
            staged: HashMap::new(),
            prompt: None,
            status: None,
            plan: Plan::new(root),
            plan_nodes: Vec::new(),
            findings: Vec::new(),
            plan_cursor: 0,
            store: Store::open_default(),
            run: None,
            stale: false,
            show_treemap: config.scan.treemap,
            dupes: None,
            read_only: None,
            remote: None,
            protected: config.protected(),
            headroom: config.safety.headroom,
            remembered: HashMap::new(),
        };
        app.rebuild_rows();
        app
    }

    pub fn size_of(&self, id: NodeId) -> u64 {
        let n = self.tree.node(id);
        if self.apparent {
            n.apparent
        } else {
            n.disk
        }
    }

    pub fn selected(&self) -> Option<NodeId> {
        self.rows.get(self.cursor).copied()
    }

    pub fn max_row_size(&self) -> u64 {
        self.rows.iter().map(|&id| self.size_of(id)).max().unwrap_or(0)
    }

    /// Total disk usage of everything staged for deletion.
    pub fn staged_bytes(&self) -> u64 {
        self.staged.values().map(|s| s.est_bytes).sum()
    }

    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    pub fn rebuild_rows(&mut self) {
        self.rows = self.tree.children(self.dir).collect();
        let sort = self.sort;
        let apparent = self.apparent;
        let tree = &self.tree;
        let size = |id: NodeId| {
            let n = tree.node(id);
            if apparent {
                n.apparent
            } else {
                n.disk
            }
        };
        match sort {
            // Size, item count and mtime read best largest/newest first, names read best A to Z;
            // `reverse` flips whichever default applies.
            Sort::Size => self.rows.sort_unstable_by_key(|&id| std::cmp::Reverse(size(id))),
            Sort::Items => {
                self.rows.sort_unstable_by_key(|&id| std::cmp::Reverse(tree.node(id).items))
            }
            Sort::Mtime => {
                self.rows.sort_unstable_by_key(|&id| std::cmp::Reverse(tree.node(id).mtime))
            }
            Sort::Name => self.rows.sort_unstable_by(|&a, &b| tree.name(a).cmp(tree.name(b))),
        }
        if self.reverse {
            self.rows.reverse();
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// What an action applies to: everything marked, or the entry under the cursor.
    fn targets(&self) -> Vec<NodeId> {
        if self.marks.is_empty() {
            self.selected().into_iter().collect()
        } else {
            // Listing order, so messages and staged order match what the user sees.
            self.rows.iter().copied().filter(|id| self.marks.contains(id)).collect()
        }
    }

    pub fn apply(&mut self, action: Action) {
        if self.prompt.is_some() {
            self.prompt_key(action);
            return;
        }
        if self.show_help || self.show_info {
            match action {
                Action::Dismiss | Action::Quit => {
                    self.show_help = false;
                    self.show_info = false;
                }
                // Only one overlay at a time: stacking them makes both unreadable.
                Action::ToggleHelp => {
                    self.show_help = !self.show_help;
                    self.show_info = false;
                }
                Action::ToggleInfo => {
                    self.show_info = !self.show_info;
                    self.show_help = false;
                }
                _ => {}
            }
            return;
        }
        match self.view {
            View::Dupes => return self.dupes_key(action),
            View::Plan => return self.plan_key(action),
            View::Confirm => return self.confirm_key(action),
            View::Running => return self.running_key(action),
            View::Browse => {}
        }

        match action {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => {
                if self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                }
            }
            Action::PageUp => self.cursor = self.cursor.saturating_sub(10),
            Action::PageDown => {
                self.cursor = (self.cursor + 10).min(self.rows.len().saturating_sub(1))
            }
            Action::Top => self.cursor = 0,
            Action::Bottom => self.cursor = self.rows.len().saturating_sub(1),
            Action::Enter => self.enter(),
            Action::Leave => self.leave(),
            Action::Sort(s) => {
                // Pressing the current sort key again flips the direction, as in ncdu.
                if self.sort == s {
                    self.reverse = !self.reverse;
                } else {
                    self.sort = s;
                    self.reverse = false;
                }
                self.rebuild_rows();
            }
            Action::ToggleApparent => {
                self.apparent = !self.apparent;
                if self.sort == Sort::Size {
                    self.rebuild_rows();
                }
            }
            Action::CycleGraph => self.graph = self.graph.next(),
            Action::ToggleTreemap => self.show_treemap = !self.show_treemap,
            Action::ToggleDupes => self.start_dupes(),
            Action::ToggleInfo => self.show_info = self.selected().is_some(),
            Action::ToggleHelp => self.show_help = true,
            Action::Mark => self.toggle_mark(),
            Action::StageDelete => self.stage_delete(),
            Action::StageMove => self.ask_move_destination(),
            Action::Unstage => self.unstage(),
            Action::TogglePlan => self.open_plan(),
            // Writing from the browser skips the review screen, so validate first and report what
            // that found, rather than let a key press appear to do nothing.
            Action::SavePlan => {
                self.refresh_plan();
                self.save_plan();
            }
            Action::Quit => self.quit = true,
            // Committing is reached through the plan view, never straight from the browser: the
            // review screen is the safety, so it is not optional.
            Action::Commit => self.open_plan(),
            Action::Confirm
            | Action::StageGroupRest
            | Action::Input(_)
            | Action::Backspace
            | Action::Submit
            | Action::Dismiss => {}
        }
    }

    /// Start a duplicate scan on a background thread and switch to its view.
    fn start_dupes(&mut self) {
        if self.dupes.as_ref().is_some_and(|d| d.is_scanning()) {
            self.view = View::Dupes;
            return;
        }

        let tree = Arc::clone(&self.tree);
        let cancel = Arc::new(AtomicBool::new(false));
        let (progress_tx, progress) = crossbeam_channel::unbounded();
        let (result_tx, result) = crossbeam_channel::bounded(1);

        let worker_cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let groups = dup::find_duplicates(
                &tree,
                &DupOptions::default(),
                Some(&progress_tx),
                Some(&worker_cancel),
            );
            result_tx.send(groups).ok();
        });

        self.dupes = Some(Dupes {
            groups: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            latest: DupProgress::default(),
            result: Some(result),
            progress,
            cancel,
        });
        self.view = View::Dupes;
        self.status = None;
    }

    /// Collect whatever the duplicate scan has reported. Called once per frame.
    pub fn poll_dupes(&mut self) {
        let Some(dupes) = self.dupes.as_mut() else { return };
        if let Some(latest) = dupes.progress.try_iter().last() {
            dupes.latest = latest;
        }
        let Some(result) = dupes.result.as_ref() else { return };
        let Ok(groups) = result.try_recv() else { return };

        dupes.result = None;
        dupes.rows = groups
            .iter()
            .enumerate()
            .flat_map(|(i, group)| {
                std::iter::once(DupRow::Header(i))
                    .chain(group.nodes.iter().map(move |&id| DupRow::File(i, id)))
            })
            .collect();
        dupes.groups = groups;
        // Start on the first file rather than the header above it.
        dupes.cursor = dupes.rows.iter().position(|r| matches!(r, DupRow::File(..))).unwrap_or(0);
    }

    fn dupes_key(&mut self, action: Action) {
        let Some(dupes) = self.dupes.as_mut() else {
            self.view = View::Browse;
            return;
        };
        match action {
            // Headers are labels, not choices; the cursor passes over them.
            Action::Down => dupes.cursor = next_file(&dupes.rows, dupes.cursor, 1),
            Action::Up => dupes.cursor = next_file(&dupes.rows, dupes.cursor, -1),
            Action::PageDown => {
                for _ in 0..10 {
                    dupes.cursor = next_file(&dupes.rows, dupes.cursor, 1);
                }
            }
            Action::PageUp => {
                for _ in 0..10 {
                    dupes.cursor = next_file(&dupes.rows, dupes.cursor, -1);
                }
            }
            Action::Top => {
                dupes.cursor =
                    dupes.rows.iter().position(|r| matches!(r, DupRow::File(..))).unwrap_or(0)
            }
            Action::Bottom => {
                dupes.cursor =
                    dupes.rows.iter().rposition(|r| matches!(r, DupRow::File(..))).unwrap_or(0)
            }
            Action::Mark => {
                if let Some((_, id)) = dupes.selected() {
                    if !self.marks.remove(&id) {
                        self.marks.insert(id);
                    }
                    let dupes = self.dupes.as_mut().expect("checked above");
                    dupes.cursor = next_file(&dupes.rows, dupes.cursor, 1);
                }
            }
            Action::StageDelete => self.stage_from_dupes(),
            Action::StageGroupRest => self.stage_group_rest(),
            Action::Unstage => {
                if let Some((_, id)) = dupes.selected() {
                    self.staged.remove(&id);
                    self.status = Some("unstaged".to_string());
                }
            }
            Action::TogglePlan => self.open_plan(),
            Action::ToggleHelp => self.show_help = true,
            Action::ToggleDupes | Action::Dismiss | Action::Quit | Action::Leave => {
                dupes.stop();
                self.view = View::Browse;
            }
            _ => {}
        }
    }

    fn stage_from_dupes(&mut self) {
        let targets: Vec<NodeId> = if self.marks.is_empty() {
            self.dupes.as_ref().and_then(|d| d.selected()).map(|(_, id)| id).into_iter().collect()
        } else {
            self.marks.iter().copied().collect()
        };
        self.stage_all(&targets);
    }

    /// Stage every copy in the group under the cursor except the first, which is kept.
    fn stage_group_rest(&mut self) {
        let Some((group, _)) = self.dupes.as_ref().and_then(|d| d.selected()) else { return };
        let Some(group) = self.dupes.as_ref().and_then(|d| d.groups.get(group)) else { return };
        // Skipping the first is what makes this safe: a bulk action that could empty a group is
        // not a labour saver, it is a way to lose the only copy.
        let targets: Vec<NodeId> = group.nodes.iter().skip(1).copied().collect();
        self.stage_all(&targets);
    }

    fn stage_all(&mut self, targets: &[NodeId]) {
        match self.record_all(targets, |_| StagedKind::Delete) {
            Ok(staged) => {
                self.marks.clear();
                self.status = Some(format!(
                    "staged {staged} for deletion ({} total)",
                    human_size(self.staged_bytes())
                ));
            }
            Err(message) => self.status = Some(message),
        }
    }

    fn enter(&mut self) {
        let Some(id) = self.selected() else { return };
        // Only real directories are enterable: a symlink to one is a link, not a place, and an
        // excluded or unreadable directory has no children to show.
        if !self.tree.node(id).is_dir() || self.tree.node(id).has(flags::SYMLINK) {
            return;
        }
        if self.tree.children(id).next().is_none() {
            return;
        }
        self.remembered.insert(self.dir, self.cursor);
        self.dir = id;
        self.cursor = 0;
        self.rebuild_rows();
    }

    fn leave(&mut self) {
        if self.dir == ROOT {
            return;
        }
        let child = self.dir;
        self.dir = self.tree.node(child).parent;
        self.rebuild_rows();
        self.cursor = self
            .remembered
            .remove(&self.dir)
            .filter(|&i| self.rows.get(i) == Some(&child))
            .or_else(|| self.rows.iter().position(|&r| r == child))
            .unwrap_or(0);
    }

    fn toggle_mark(&mut self) {
        let Some(id) = self.selected() else { return };
        if !self.marks.remove(&id) {
            self.marks.insert(id);
        }
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    fn stage_delete(&mut self) {
        let targets = self.targets();
        self.stage_all(&targets);
    }

    fn ask_move_destination(&mut self) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let label = if targets.len() == 1 {
            format!("move {} into directory:", self.tree.name(targets[0]).to_string_lossy())
        } else {
            format!("move {} entries into directory:", targets.len())
        };
        self.prompt = Some(Prompt { label, input: String::new(), targets });
    }

    /// Stage a set of entries, reading what each one currently looks like.
    ///
    /// Identities are fetched in one go rather than one at a time: over a connection that is a
    /// single round trip instead of one per file, and locally it costs nothing either way.
    fn record_all(
        &mut self,
        targets: &[NodeId],
        kind: impl Fn(NodeId) -> StagedKind,
    ) -> Result<usize, String> {
        if let Some(reason) = &self.read_only {
            return Err(reason.clone());
        }
        if targets.is_empty() {
            return Ok(0);
        }

        let paths: Vec<PathBuf> = targets.iter().map(|&id| self.tree.path_of(id)).collect();
        let idents = self.identities(&paths)?;

        let mut staged = 0;
        for ((&id, path), ident) in targets.iter().zip(&paths).zip(idents) {
            let Some(ident) = ident else {
                return Err(format!("cannot stage {}: it is no longer there", path.display()));
            };
            let est_bytes = self.tree.node(id).disk;
            self.staged.insert(id, Staged { kind: kind(id), ident, est_bytes });
            staged += 1;
        }
        Ok(staged)
    }

    /// What these paths look like now, asked of whichever machine holds them.
    fn identities(&self, paths: &[PathBuf]) -> Result<Vec<Option<Ident>>, String> {
        let Some(remote) = &self.remote else {
            return Ok(paths.iter().map(|p| Ident::of(p).ok()).collect());
        };
        let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let mut remote =
            remote.lock().map_err(|_| "the connection is in a bad state".to_string())?;
        let host = remote.host.clone();
        remote.identify(&names).map_err(|e| format!("asking {host}: {e}"))
    }

    fn unstage(&mut self) {
        let targets = self.targets();
        let removed = targets.iter().filter(|id| self.staged.remove(id).is_some()).count();
        self.marks.clear();
        self.status = Some(match removed {
            0 => "nothing staged here".to_string(),
            n => format!("unstaged {n}"),
        });
    }

    fn prompt_key(&mut self, action: Action) {
        let Some(prompt) = self.prompt.as_mut() else { return };
        match action {
            Action::Input(c) => prompt.input.push(c),
            Action::Backspace => {
                prompt.input.pop();
            }
            Action::Dismiss | Action::Quit => {
                self.prompt = None;
                self.status = Some("move cancelled".to_string());
            }
            Action::Submit => {
                let prompt = self.prompt.take().expect("checked above");
                self.submit_move(prompt);
            }
            _ => {}
        }
    }

    fn submit_move(&mut self, prompt: Prompt) {
        let dir = PathBuf::from(prompt.input.trim());
        if dir.as_os_str().is_empty() {
            self.status = Some("move cancelled".to_string());
            return;
        }
        if !dir.is_absolute() {
            self.status = Some("destination must be an absolute path".to_string());
            return;
        }

        // Each entry keeps its own name under the destination directory, the way `mv` into a
        // directory behaves.
        let names: HashMap<NodeId, PathBuf> =
            prompt.targets.iter().map(|&id| (id, dir.join(self.tree.name(id)))).collect();

        match self.record_all(&prompt.targets, |id| StagedKind::Move(names[&id].clone())) {
            Ok(staged) => {
                self.marks.clear();
                self.status = Some(format!("staged {staged} to move into {}", dir.display()));
            }
            Err(message) => self.status = Some(message),
        }
    }

    /// Build the plan from what is staged and validate it.
    pub fn open_plan(&mut self) {
        self.refresh_plan();
        self.view = View::Plan;
        self.plan_cursor = 0;
    }

    pub fn refresh_plan(&mut self) {
        let mut plan = Plan::new(self.tree.root_path().to_path_buf());
        plan.id = self.plan.id.clone();

        // Node order, so a plan reads top-down the way the tree does and two builds of the same
        // staging produce the same file.
        let mut ids: Vec<NodeId> = self.staged.keys().copied().collect();
        ids.sort_unstable();

        self.plan_nodes = ids.clone();
        plan.ops = ids
            .iter()
            .map(|&id| {
                let staged = &self.staged[&id];
                let path = self.tree.path_of(id);
                match &staged.kind {
                    StagedKind::Delete => Op::Delete {
                        path,
                        ident: staged.ident.clone(),
                        est_bytes: staged.est_bytes,
                    },
                    StagedKind::Move(dst) => Op::Move {
                        src: path,
                        dst: dst.clone(),
                        ident: staged.ident.clone(),
                        est_bytes: staged.est_bytes,
                        on_conflict: Conflict::Fail,
                    },
                }
            })
            .collect();

        self.findings = validate(
            &plan,
            &ValidateOptions {
                protected: self.protected.clone(),
                headroom: self.headroom,
                ..Default::default()
            },
        );
        self.plan = plan;
    }

    fn plan_key(&mut self, action: Action) {
        match action {
            Action::Up => self.plan_cursor = self.plan_cursor.saturating_sub(1),
            Action::Down => {
                if self.plan_cursor + 1 < self.plan.ops.len() {
                    self.plan_cursor += 1;
                }
            }
            Action::Top => self.plan_cursor = 0,
            Action::Bottom => self.plan_cursor = self.plan.ops.len().saturating_sub(1),
            Action::Unstage => {
                if let Some(&id) = self.plan_nodes.get(self.plan_cursor) {
                    self.staged.remove(&id);
                    self.refresh_plan();
                    self.plan_cursor = self.plan_cursor.min(self.plan.ops.len().saturating_sub(1));
                    self.status = Some("unstaged".to_string());
                }
            }
            Action::SavePlan => self.save_plan(),
            Action::Commit => self.ask_to_commit(),
            Action::ToggleHelp => self.show_help = true,
            Action::TogglePlan | Action::Dismiss | Action::Leave => self.view = View::Browse,
            Action::Quit => self.view = View::Browse,
            _ => {}
        }
    }

    /// Move to the confirmation screen, unless validation says the plan cannot run.
    fn ask_to_commit(&mut self) {
        self.refresh_plan();
        if self.plan.ops.is_empty() {
            self.status = Some("nothing staged".to_string());
            return;
        }
        let errors = self.findings.iter().filter(|f| f.severity == Severity::Error).count();
        if errors > 0 {
            self.status =
                Some(format!("{errors} error{} to resolve before committing", plural(errors)));
            return;
        }
        self.view = View::Confirm;
    }

    fn confirm_key(&mut self, action: Action) {
        match action {
            Action::Confirm => self.start_commit(),
            Action::Dismiss | Action::Quit | Action::TogglePlan | Action::Leave => {
                self.view = View::Plan;
                self.status = Some("not committed".to_string());
            }
            _ => {}
        }
    }

    /// Save the plan, then run it on a background thread.
    ///
    /// Saving first is not a convenience: if this process dies during the commit, the plan and its
    /// journal are what make the run resumable, and a plan that only existed in memory would take
    /// the record of what was half-done with it.
    fn start_commit(&mut self) {
        let mut plan = self.plan.clone();
        plan.normalize();
        let total = plan.ops.len();
        let plan_id = plan.id.clone();

        let control = Arc::new(Control::new());
        let (event_tx, events) = crossbeam_channel::unbounded();
        let (result_tx, result) = crossbeam_channel::bounded(1);

        // Where the files are is where the plan, the journal and the work all belong. Running a
        // commit from here against another machine would leave the record of a half-finished run
        // on the wrong side of the connection.
        if let Some(remote) = self.remote.clone() {
            let saved = remote
                .lock()
                .map_err(|_| "the connection is in a bad state".to_string())
                .and_then(|mut r| r.save_plan(&plan).map_err(|e| e.to_string()));
            let id = match saved {
                Ok((id, _)) => id,
                Err(message) => {
                    self.status = Some(format!("could not store the plan: {message}"));
                    self.view = View::Plan;
                    return;
                }
            };
            std::thread::spawn(move || {
                let outcome = remote
                    .lock()
                    .map_err(|_| io::Error::other("the connection is in a bad state"))
                    .and_then(|mut r| {
                        r.apply(&id, false, |event| {
                            event_tx.send(event).ok();
                        })
                        .map(|(outcome, _)| outcome)
                    });
                drop(event_tx);
                result_tx.send(outcome).ok();
            });
        } else {
            let dir = match self.store.save(&plan) {
                Ok(path) => path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
                Err(e) => {
                    self.status = Some(format!("could not save the plan: {e}"));
                    self.view = View::Plan;
                    return;
                }
            };
            let worker_control = Arc::clone(&control);
            std::thread::spawn(move || {
                let outcome = exec::execute(
                    &plan,
                    &dir,
                    &ExecOptions::default(),
                    &worker_control,
                    Some(&event_tx),
                );
                drop(event_tx);
                result_tx.send(outcome).ok();
            });
        }

        self.run = Some(Running {
            control,
            events,
            result,
            log: Vec::new(),
            total,
            completed: 0,
            summary: None,
            plan_id,
            remote: self.remote.is_some(),
        });
        self.view = View::Running;
        self.status = None;
    }

    fn running_key(&mut self, action: Action) {
        let Some(run) = self.run.as_mut() else {
            self.view = View::Browse;
            return;
        };
        match action {
            // Pausing mid-commit is the same mechanism recovery uses, so it is always safe —
            // locally. A commit running on another machine has its own switch, and reaching it
            // would mean interrupting the stream we are reading; say so rather than do nothing.
            Action::TogglePlan | Action::Input('p') if !run.is_finished() && run.remote => {
                self.status = Some(format!(
                    "this commit is running elsewhere; stop it there, then `ccdu resume {}`",
                    run.plan_id
                ));
            }
            Action::TogglePlan | Action::Input('p') if !run.is_finished() => run.pause(),
            Action::Dismiss | Action::Quit | Action::Leave if run.is_finished() => {
                self.view = View::Browse;
            }
            Action::ToggleHelp => self.show_help = true,
            _ => {}
        }
    }

    /// Drain whatever the commit thread has reported. Called once per frame.
    pub fn poll_run(&mut self) {
        let Some(run) = self.run.as_mut() else { return };
        if run.is_finished() {
            return;
        }

        for event in run.events.try_iter() {
            match event {
                ExecEvent::Started { index, summary } => {
                    run.log.push(format!("#{index} {summary}"))
                }
                ExecEvent::Finished { index, freed } => {
                    run.completed += 1;
                    run.log.push(format!("#{index} done, {} reclaimed", human_size(freed)));
                }
                ExecEvent::Failed { index, error } => {
                    run.completed += 1;
                    run.log.push(format!("#{index} FAILED: {error}"));
                }
                ExecEvent::AlreadyDone { index } => {
                    run.completed += 1;
                    run.log.push(format!("#{index} already done"));
                }
            }
        }

        if let Ok(outcome) = run.result.try_recv() {
            run.summary = Some(match outcome {
                Ok(o) if o.paused => format!(
                    "paused after {} operations, {} reclaimed — resume with `ccdu resume {}`",
                    o.done,
                    human_size(o.freed),
                    run.plan_id
                ),
                Err(e) if run.remote => {
                    format!("{e}; the run is recoverable there with `ccdu resume {}`", run.plan_id)
                }
                Ok(o) if o.failed > 0 => format!(
                    "{} done, {} failed, {} reclaimed",
                    o.done,
                    o.failed,
                    human_size(o.freed)
                ),
                Ok(o) => format!("{} operations done, {} reclaimed", o.done, human_size(o.freed)),
                Err(e) => format!("could not run: {e}"),
            });
            // Whatever happened, entries are gone and the tree in memory is a description of a
            // disk that no longer exists. Say so rather than show numbers that are quietly wrong.
            // The header carries a terse [stale]; the reason goes here, where there is room.
            self.stale = true;
            self.status = Some(
                "files were removed — the listing is from before the commit; rerun ccdu to rescan"
                    .to_string(),
            );
            self.staged.clear();
            self.marks.clear();
        }
    }

    fn save_plan(&mut self) {
        if self.plan.ops.is_empty() {
            self.status = Some("nothing staged".to_string());
            return;
        }
        let mut plan = self.plan.clone();
        // Dropping entries already covered by a deleted parent is a courtesy, not a correction:
        // say so, so the saved plan is not silently smaller than what was reviewed.
        let dropped = plan.normalize();

        match self.store.save(&plan) {
            Ok(path) => {
                let mut notes = Vec::new();
                if dropped > 0 {
                    notes.push(format!("{dropped} redundant dropped"));
                }
                // A plan with errors is still worth saving — it is a document, and the errors may
                // be fixable — but it must not look clean.
                let errors = self.findings.iter().filter(|f| f.severity == Severity::Error).count();
                if errors > 0 {
                    notes.push(format!("{errors} error{} to resolve", plural(errors)));
                }
                let extra = if notes.is_empty() {
                    String::new()
                } else {
                    format!(", {}", notes.join(", "))
                };
                self.status =
                    Some(format!("saved {} ({} ops{extra})", path.display(), plan.ops.len()));
            }
            Err(e) => self.status = Some(format!("could not save: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccdu_core::scan::{scan, ScanOptions};
    use std::fs;

    /// A tree with a predictable shape: `big/` (3 files), `small/` (1 file), `tiny`.
    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("big")).unwrap();
        for i in 0..3 {
            fs::write(root.join("big").join(format!("f{i}")), vec![0u8; 100_000]).unwrap();
        }
        fs::create_dir(root.join("small")).unwrap();
        fs::write(root.join("small/one"), vec![0u8; 10_000]).unwrap();
        fs::write(root.join("tiny"), b"x").unwrap();

        let tree = scan(root, &ScanOptions::default(), None, None).unwrap();
        (dir, App::new(tree))
    }

    fn name_of(app: &App, id: NodeId) -> String {
        app.tree.name(id).to_string_lossy().into_owned()
    }

    fn row_names(app: &App) -> Vec<String> {
        app.rows.iter().map(|&id| name_of(app, id)).collect()
    }

    fn type_in(app: &mut App, text: &str) {
        for c in text.chars() {
            app.apply(Action::Input(c));
        }
    }

    #[test]
    fn opens_sorted_by_size_descending() {
        let (_d, app) = fixture();
        assert_eq!(row_names(&app), ["big", "small", "tiny"]);
    }

    #[test]
    fn repeating_a_sort_key_reverses_it() {
        let (_d, mut app) = fixture();
        app.apply(Action::Sort(Sort::Name));
        assert_eq!(row_names(&app), ["big", "small", "tiny"]);
        app.apply(Action::Sort(Sort::Name));
        assert_eq!(row_names(&app), ["tiny", "small", "big"]);
        app.apply(Action::Sort(Sort::Size));
        assert_eq!(row_names(&app), ["big", "small", "tiny"]);
    }

    #[test]
    fn entering_and_leaving_restores_the_cursor() {
        let (_d, mut app) = fixture();
        app.apply(Action::Down);
        assert_eq!(name_of(&app, app.selected().unwrap()), "small");

        app.apply(Action::Enter);
        assert_eq!(row_names(&app), ["one"]);

        app.apply(Action::Leave);
        assert_eq!(name_of(&app, app.selected().unwrap()), "small", "cursor did not come back");
    }

    #[test]
    fn files_and_empty_directories_are_not_enterable() {
        let (_d, mut app) = fixture();
        app.apply(Action::Bottom);
        let before = app.dir;
        app.apply(Action::Enter);
        assert_eq!(app.dir, before);
    }

    #[test]
    fn leaving_the_root_does_nothing() {
        let (_d, mut app) = fixture();
        app.apply(Action::Leave);
        assert_eq!(app.dir, ROOT);
        assert_eq!(row_names(&app), ["big", "small", "tiny"]);
    }

    #[test]
    fn cursor_stays_inside_the_listing() {
        let (_d, mut app) = fixture();
        for _ in 0..10 {
            app.apply(Action::Up);
        }
        assert_eq!(app.cursor, 0);
        for _ in 0..10 {
            app.apply(Action::Down);
        }
        assert_eq!(app.cursor, app.rows.len() - 1);
    }

    #[test]
    fn apparent_and_disk_sizes_can_disagree() {
        let (_d, mut app) = fixture();
        let tiny = *app.rows.last().unwrap();
        assert_eq!(app.size_of(tiny), app.tree.node(tiny).disk);
        app.apply(Action::ToggleApparent);
        assert_eq!(app.size_of(tiny), 1);
    }

    #[test]
    fn only_one_overlay_is_open_at_a_time() {
        let (_d, mut app) = fixture();
        app.apply(Action::ToggleInfo);
        app.apply(Action::ToggleHelp);
        assert!(app.show_help && !app.show_info);
        app.apply(Action::ToggleInfo);
        assert!(app.show_info && !app.show_help);
    }

    #[test]
    fn overlays_swallow_navigation() {
        let (_d, mut app) = fixture();
        app.apply(Action::ToggleHelp);
        app.apply(Action::Down);
        assert_eq!(app.cursor, 0, "help overlay let a keypress through");
        app.apply(Action::Dismiss);
        app.apply(Action::Down);
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn staging_a_delete_touches_nothing_on_disk() {
        let (dir, mut app) = fixture();
        app.apply(Action::StageDelete);

        assert_eq!(app.staged.len(), 1);
        assert!(dir.path().join("big").exists(), "staging must not delete anything");
        assert!(app.staged_bytes() >= 300_000);
    }

    #[test]
    fn marking_applies_an_action_to_every_marked_entry() {
        let (_d, mut app) = fixture();
        app.apply(Action::Mark); // marks "big", cursor moves to "small"
        app.apply(Action::Mark); // marks "small", cursor moves to "tiny"
        app.apply(Action::StageDelete);

        assert_eq!(app.staged.len(), 2);
        assert!(app.marks.is_empty(), "marks should clear once acted on");
        let names: Vec<_> = app.staged.keys().map(|&id| name_of(&app, id)).collect();
        assert!(names.contains(&"big".to_string()) && names.contains(&"small".to_string()));
    }

    #[test]
    fn unstaging_removes_what_staging_added() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageDelete);
        assert_eq!(app.staged.len(), 1);
        app.apply(Action::Unstage);
        assert!(app.staged.is_empty());
    }

    #[test]
    fn a_move_prompt_stages_one_destination_per_entry() {
        let (_d, mut app) = fixture();
        app.apply(Action::Mark);
        app.apply(Action::Mark);
        app.apply(Action::StageMove);
        assert!(app.prompt.is_some());

        type_in(&mut app, "/mnt/archive");
        app.apply(Action::Submit);

        assert!(app.prompt.is_none());
        assert_eq!(app.staged.len(), 2);
        let mut destinations: Vec<String> = app
            .staged
            .values()
            .map(|s| match &s.kind {
                StagedKind::Move(dst) => dst.display().to_string(),
                StagedKind::Delete => unreachable!(),
            })
            .collect();
        destinations.sort();
        assert_eq!(destinations, ["/mnt/archive/big", "/mnt/archive/small"]);
    }

    #[test]
    fn a_relative_move_destination_is_refused() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageMove);
        type_in(&mut app, "somewhere");
        app.apply(Action::Submit);

        assert!(app.staged.is_empty());
        assert!(app.status.as_deref().unwrap().contains("absolute"), "{:?}", app.status);
    }

    #[test]
    fn cancelling_the_prompt_stages_nothing() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageMove);
        type_in(&mut app, "/mnt/x");
        app.apply(Action::Dismiss);

        assert!(app.prompt.is_none());
        assert!(app.staged.is_empty());
    }

    #[test]
    fn the_prompt_swallows_keys_that_would_otherwise_navigate() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageMove);
        app.apply(Action::Down);
        app.apply(Action::Quit);
        // Quit inside a prompt cancels the prompt, it does not exit the program.
        assert!(!app.quit);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn the_plan_view_reflects_what_is_staged() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageDelete);
        app.apply(Action::TogglePlan);

        assert_eq!(app.view, View::Plan);
        assert_eq!(app.plan.ops.len(), 1);
        assert!(app.plan.ops[0].is_delete());
        assert!(!app.has_errors(), "{:?}", app.findings);
    }

    #[test]
    fn the_plan_view_reports_a_staged_move_onto_a_protected_path() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageMove);
        type_in(&mut app, "/");
        app.apply(Action::Submit);
        app.apply(Action::TogglePlan);

        // "/" + "big" is /big, which does not exist, so its parent "/" is unwritable for a
        // normal user. Either way this must not be committable.
        assert!(app.has_errors(), "{:?}", app.findings);
    }

    #[test]
    fn unstaging_from_the_plan_view_updates_the_plan() {
        let (_d, mut app) = fixture();
        app.apply(Action::Mark);
        app.apply(Action::Mark);
        app.apply(Action::StageDelete);
        app.apply(Action::TogglePlan);
        assert_eq!(app.plan.ops.len(), 2);

        app.apply(Action::Unstage);
        assert_eq!(app.plan.ops.len(), 1);
        assert_eq!(app.staged.len(), 1);
    }

    #[test]
    fn nested_staging_is_reported_as_redundant_not_as_an_error() {
        let (_d, mut app) = fixture();
        app.apply(Action::StageDelete); // "big/"
        app.apply(Action::Enter);
        app.apply(Action::StageDelete); // "big/f?" inside it
        app.apply(Action::TogglePlan);

        assert_eq!(app.plan.ops.len(), 2);
        assert!(!app.has_errors(), "{:?}", app.findings);
        assert!(app.findings.iter().any(|f| f.message.contains("redundant")), "{:?}", app.findings);
    }

    /// Drive a commit to completion, polling the way the event loop does.
    fn commit_and_wait(app: &mut App) {
        app.apply(Action::Commit);
        assert_eq!(app.view, View::Confirm, "commit should stop at a confirmation");
        app.apply(Action::Confirm);
        assert_eq!(app.view, View::Running);

        for _ in 0..2000 {
            app.poll_run();
            if app.run.as_ref().is_some_and(|r| r.is_finished()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("commit never finished");
    }

    #[test]
    fn committing_deletes_what_was_staged_and_nothing_else() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-commit"));

        app.apply(Action::StageDelete); // "big/"
        app.apply(Action::TogglePlan);
        commit_and_wait(&mut app);

        let run = app.run.as_ref().unwrap();
        assert!(
            run.summary.as_deref().unwrap().contains("1 operations done"),
            "{run:?}",
            run = run.summary
        );
        assert!(!dir.path().join("big").exists(), "the staged directory survived");
        assert!(dir.path().join("small/one").exists(), "something unstaged was deleted");
        assert!(dir.path().join("tiny").exists());
    }

    #[test]
    fn a_commit_leaves_a_resumable_plan_behind() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-record"));

        app.apply(Action::StageDelete);
        app.apply(Action::TogglePlan);
        commit_and_wait(&mut app);

        // The plan and its journal outlive the run, which is what makes a crash recoverable.
        let listed = app.store.list().unwrap();
        assert_eq!(listed.len(), 1);
        let plan_dir = app.store.dir_for(&listed[0].id).unwrap();
        assert_eq!(ccdu_core::exec::state(&plan_dir).unwrap(), ccdu_core::exec::RunState::Finished);
    }

    #[test]
    fn backing_out_of_the_confirmation_changes_nothing() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-cancel"));

        app.apply(Action::StageDelete);
        app.apply(Action::TogglePlan);
        app.apply(Action::Commit);
        assert_eq!(app.view, View::Confirm);

        app.apply(Action::Dismiss);
        assert_eq!(app.view, View::Plan);
        assert!(dir.path().join("big").exists(), "backing out still deleted something");
        assert!(app.run.is_none());
        assert_eq!(app.staged.len(), 1, "backing out should not unstage anything");
    }

    #[test]
    fn a_plan_with_errors_cannot_be_committed() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-blocked"));

        app.apply(Action::StageMove);
        type_in(&mut app, "/");
        app.apply(Action::Submit);
        app.apply(Action::TogglePlan);
        app.apply(Action::Commit);

        assert_eq!(app.view, View::Plan, "a plan with errors reached the confirmation screen");
        assert!(app.status.as_deref().unwrap_or("").contains("to resolve"), "{:?}", app.status);
    }

    #[test]
    fn committing_marks_the_tree_stale() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-stale"));
        assert!(!app.stale);

        app.apply(Action::StageDelete);
        app.apply(Action::TogglePlan);
        commit_and_wait(&mut app);

        // The sizes in memory now describe a disk that no longer exists.
        assert!(app.stale, "the browser would keep showing totals for deleted files");
        assert!(app.staged.is_empty(), "staging survived the commit that consumed it");
    }

    #[test]
    fn committing_is_only_reachable_through_the_review_screen() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-route"));
        app.apply(Action::StageDelete);

        // `c` from the browser opens the plan rather than the confirmation.
        app.apply(Action::Commit);
        assert_eq!(app.view, View::Plan);
        assert!(dir.path().join("big").exists());
    }

    /// Drive a duplicate scan to completion, polling the way the event loop does.
    fn find_dupes(app: &mut App) {
        app.apply(Action::ToggleDupes);
        assert_eq!(app.view, View::Dupes);
        for _ in 0..2000 {
            app.poll_dupes();
            if app.dupes.as_ref().is_some_and(|d| !d.is_scanning()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("duplicate scan never finished");
    }

    /// Three copies of one file plus an unrelated one.
    fn dup_fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        for name in ["copy-a", "sub/copy-b", "sub/copy-c"] {
            fs::write(root.join(name), vec![8u8; 30_000]).unwrap();
        }
        fs::write(root.join("unique"), vec![9u8; 30_001]).unwrap();

        let tree = scan(root, &ScanOptions::default(), None, None).unwrap();
        (dir, App::new(tree))
    }

    #[test]
    fn the_duplicates_view_groups_identical_files() {
        let (_d, mut app) = dup_fixture();
        find_dupes(&mut app);

        let dupes = app.dupes.as_ref().unwrap();
        assert_eq!(dupes.groups.len(), 1, "{:?}", dupes.groups);
        assert_eq!(dupes.groups[0].nodes.len(), 3);
        // One header plus three files.
        assert_eq!(dupes.rows.len(), 4);
        assert!(matches!(dupes.rows[0], DupRow::Header(0)));
    }

    #[test]
    fn the_cursor_skips_group_headers() {
        let (_d, mut app) = dup_fixture();
        find_dupes(&mut app);

        // It starts on a file, not the header above it.
        assert!(app.dupes.as_ref().unwrap().selected().is_some());
        for _ in 0..5 {
            app.apply(Action::Down);
            assert!(
                app.dupes.as_ref().unwrap().selected().is_some(),
                "the cursor landed on a header"
            );
        }
        for _ in 0..10 {
            app.apply(Action::Up);
            assert!(app.dupes.as_ref().unwrap().selected().is_some());
        }
    }

    #[test]
    fn staging_all_but_the_first_never_empties_a_group() {
        let (dir, mut app) = dup_fixture();
        find_dupes(&mut app);

        app.apply(Action::StageGroupRest);

        assert_eq!(app.staged.len(), 2, "one copy must survive");
        let kept = app.dupes.as_ref().unwrap().groups[0].nodes[0];
        assert!(!app.staged.contains_key(&kept), "the kept copy was staged for deletion");
        // Still nothing on disk has changed.
        assert!(dir.path().join("copy-a").exists());
    }

    #[test]
    fn marking_in_the_duplicates_view_stages_those_copies() {
        let (_d, mut app) = dup_fixture();
        find_dupes(&mut app);

        app.apply(Action::Mark);
        app.apply(Action::Mark);
        app.apply(Action::StageDelete);

        assert_eq!(app.staged.len(), 2);
        assert!(app.marks.is_empty());
    }

    #[test]
    fn duplicates_can_be_staged_and_then_reviewed_as_a_plan() {
        let (_d, mut app) = dup_fixture();
        find_dupes(&mut app);

        app.apply(Action::StageGroupRest);
        app.apply(Action::TogglePlan);

        assert_eq!(app.view, View::Plan);
        assert_eq!(app.plan.ops.len(), 2);
        assert!(app.plan.ops.iter().all(|op| op.is_delete()));
        assert!(!app.has_errors(), "{:?}", app.findings);
    }

    #[test]
    fn leaving_the_duplicates_view_returns_to_the_browser() {
        let (_d, mut app) = dup_fixture();
        find_dupes(&mut app);
        app.apply(Action::Dismiss);
        assert_eq!(app.view, View::Browse);
        assert!(!app.quit, "leaving a subview should not quit the program");
    }

    #[test]
    fn the_treemap_toggles_without_disturbing_the_listing() {
        let (_d, mut app) = fixture();
        let before = app.rows.clone();
        assert!(!app.show_treemap);

        app.apply(Action::ToggleTreemap);
        assert!(app.show_treemap);
        assert_eq!(app.rows, before);

        app.apply(Action::ToggleTreemap);
        assert!(!app.show_treemap);
    }

    #[test]
    fn a_read_only_tree_refuses_staging_and_says_why() {
        let (dir, mut app) = fixture();
        app.read_only = Some("these files are on server".to_string());

        app.apply(Action::StageDelete);
        assert!(app.staged.is_empty(), "a tree we cannot act on was staged against");
        assert_eq!(app.status.as_deref(), Some("these files are on server"));

        // A move is refused for the same reason, at the same point.
        app.apply(Action::StageMove);
        type_in(&mut app, "/mnt/elsewhere");
        app.apply(Action::Submit);
        assert!(app.staged.is_empty());
        assert!(dir.path().join("big").exists());
    }

    #[test]
    fn saving_from_the_browser_works_without_visiting_the_plan_view() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-direct"));

        app.apply(Action::StageDelete);
        app.apply(Action::SavePlan);

        assert!(app.status.as_deref().unwrap_or("").contains("saved"), "{:?}", app.status);
        assert_eq!(app.store.list().unwrap().len(), 1);
    }

    #[test]
    fn saving_a_plan_with_errors_says_so_rather_than_looking_clean() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state-errors"));

        app.apply(Action::StageMove);
        type_in(&mut app, "/");
        app.apply(Action::Submit);
        app.apply(Action::SavePlan);

        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("saved"), "{status}");
        assert!(status.contains("to resolve"), "{status}");
    }

    #[test]
    fn saving_writes_a_normalized_plan_to_the_store() {
        let (dir, mut app) = fixture();
        app.store = Store::at(dir.path().join("state"));

        app.apply(Action::StageDelete); // "big/"
        app.apply(Action::Enter);
        app.apply(Action::Mark);
        app.apply(Action::Mark);
        app.apply(Action::Mark);
        app.apply(Action::StageDelete); // its three children too
        app.apply(Action::TogglePlan);
        assert_eq!(app.plan.ops.len(), 4);

        app.apply(Action::SavePlan);
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("saved"), "{status}");
        assert!(status.contains("3 redundant dropped"), "{status}");

        let listed = app.store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].ops, 1, "only the parent deletion should survive");
    }
}
