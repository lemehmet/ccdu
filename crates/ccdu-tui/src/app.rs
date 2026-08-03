//! Browser state and input handling.
//!
//! Kept free of rendering so it can be driven from a test without a terminal.

use std::collections::HashMap;

use ccdu_core::model::{flags, NodeId, Tree, ROOT};

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

/// How much of the size column is given over to the bar graph.
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
    Dismiss,
    Quit,
}

pub struct App {
    pub tree: Tree,
    /// Directory currently listed.
    pub dir: NodeId,
    /// Index into [`App::rows`].
    pub cursor: usize,
    /// First visible row, adjusted during rendering once the viewport height is known.
    pub offset: usize,
    /// Children of `dir` in display order.
    pub rows: Vec<NodeId>,
    pub sort: Sort,
    pub reverse: bool,
    /// Show `st_size` rather than actual disk usage.
    pub apparent: bool,
    pub graph: Graph,
    pub show_help: bool,
    pub show_info: bool,
    pub quit: bool,
    /// Where the cursor was in each directory, so leaving and re-entering lands where you left.
    remembered: HashMap<NodeId, usize>,
}

impl App {
    pub fn new(tree: Tree) -> Self {
        let mut app = App {
            tree,
            dir: ROOT,
            cursor: 0,
            offset: 0,
            rows: Vec::new(),
            sort: Sort::Size,
            reverse: false,
            apparent: false,
            graph: Graph::Both,
            show_help: false,
            show_info: false,
            quit: false,
            remembered: HashMap::new(),
        };
        app.rebuild_rows();
        app
    }

    /// Size of a node under the current apparent/disk setting.
    pub fn size_of(&self, id: NodeId) -> u64 {
        let n = self.tree.node(id);
        if self.apparent {
            n.apparent
        } else {
            n.disk
        }
    }

    /// The node the cursor is on, if the listing is not empty.
    pub fn selected(&self) -> Option<NodeId> {
        self.rows.get(self.cursor).copied()
    }

    /// Largest entry in the current listing, which sets the bar graph's full scale.
    pub fn max_row_size(&self) -> u64 {
        self.rows.iter().map(|&id| self.size_of(id)).max().unwrap_or(0)
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

    pub fn apply(&mut self, action: Action) {
        // The overlays swallow navigation so you cannot scroll a list you cannot see.
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
            Action::ToggleInfo => self.show_info = self.selected().is_some(),
            Action::ToggleHelp => self.show_help = true,
            Action::Dismiss => {}
            Action::Quit => self.quit = true,
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
        self.offset = 0;
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
        self.offset = 0;
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
        // A different key resets to that key's natural direction.
        app.apply(Action::Sort(Sort::Size));
        assert_eq!(row_names(&app), ["big", "small", "tiny"]);
    }

    #[test]
    fn entering_and_leaving_restores_the_cursor() {
        let (_d, mut app) = fixture();
        app.apply(Action::Down); // onto "small"
        assert_eq!(name_of(&app, app.selected().unwrap()), "small");

        app.apply(Action::Enter);
        assert_eq!(row_names(&app), ["one"]);

        app.apply(Action::Leave);
        assert_eq!(name_of(&app, app.selected().unwrap()), "small", "cursor did not come back");
    }

    #[test]
    fn files_and_empty_directories_are_not_enterable() {
        let (_d, mut app) = fixture();
        app.apply(Action::Bottom); // "tiny", a file
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
        // One byte occupies a whole block on disk.
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
}
