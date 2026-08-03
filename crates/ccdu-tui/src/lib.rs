//! Terminal user interface for ccdu.
//!
//! Two phases: a scanning screen that can be cut short, then the browser. The browser only ever
//! reads the tree, so the same [`App`] drives both this and the snapshot tests.

pub mod app;
pub mod treemap;
pub mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ccdu_core::model::Tree;
use ccdu_core::scan::{scan, Progress, ScanOptions};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use ratatui::DefaultTerminal;

use crate::app::{Action, App, Sort};

/// How long to wait for a key before redrawing anyway. Also the spinner's tick.
const TICK: Duration = Duration::from_millis(100);

/// Scan `root` and browse it. Restores the terminal on the way out, including on panic.
pub fn run(root: PathBuf, opts: ScanOptions) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = scan_phase(&mut terminal, &root, &opts).and_then(|tree| match tree {
        Some(tree) => browse(&mut terminal, tree),
        None => Ok(()),
    });
    ratatui::restore();
    result
}

/// Run the scan on background threads while showing progress. Returns `None` if the user quit
/// before anything was scanned.
fn scan_phase(
    terminal: &mut DefaultTerminal,
    root: &Path,
    opts: &ScanOptions,
) -> io::Result<Option<Tree>> {
    let cancel = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();

    std::thread::scope(|scope| {
        let handle = scope.spawn(|| scan(root, opts, Some(&tx), Some(&cancel)));

        let mut latest = Progress::default();
        let started = Instant::now();
        loop {
            if handle.is_finished() {
                break;
            }
            // Keep only the newest progress; the scanner outruns the display.
            if let Some(p) = rx.try_iter().last() {
                latest = p;
            }
            let spin = (started.elapsed().as_millis() / 100) as usize;
            terminal.draw(|frame| ui::draw_scanning(frame, root, &latest, spin))?;

            if event::poll(TICK)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press && is_quit(&key) {
                        // Not an abort: the partial tree is still worth browsing.
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        handle.join().unwrap_or_else(|_| Err(io::Error::other("scan thread panicked"))).map(Some)
    })
}

/// Browse a tree that was loaded or fetched rather than scanned here and now.
///
/// `read_only` explains why staging is refused; a tree from another machine or another moment
/// describes paths this process cannot safely act on.
pub fn browse_tree(tree: Tree, read_only: Option<String>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = browse_with(&mut terminal, tree, read_only, None);
    ratatui::restore();
    result
}

/// Browse a tree that lives on another machine, staging and committing through the connection
/// that produced it.
pub fn browse_remote(tree: Tree, remote: ccdu_remote::Remote) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = browse_with(
        &mut terminal,
        tree,
        None,
        Some(std::sync::Arc::new(std::sync::Mutex::new(remote))),
    );
    ratatui::restore();
    result
}

fn browse(terminal: &mut DefaultTerminal, tree: Tree) -> io::Result<()> {
    browse_with(terminal, tree, None, None)
}

fn browse_with(
    terminal: &mut DefaultTerminal,
    tree: Tree,
    read_only: Option<String>,
    remote: Option<std::sync::Arc<std::sync::Mutex<ccdu_remote::Remote>>>,
) -> io::Result<()> {
    let mut app = App::new(tree);
    app.read_only = read_only;
    app.remote = remote;
    let mut list_state = ListState::default();

    while !app.quit {
        // Both the commit and the duplicate scan run on their own threads; this is where their
        // progress reaches the screen.
        app.poll_run();
        app.poll_dupes();
        terminal.draw(|frame| ui::draw(frame, &app, &mut list_state))?;

        if !event::poll(TICK)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(action) = translate(&key, app.prompt.is_some()) {
                // A status line answers the last action, so it should not outlive it.
                app.status = None;
                app.apply(action);
            }
        }
    }
    Ok(())
}

fn is_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Map a key press onto an action. Keys follow ncdu wherever it has one.
///
/// While a prompt is open almost every key is text, so only the few that steer the prompt are
/// interpreted — otherwise typing a path named `dst` would stage three deletions.
fn translate(key: &KeyEvent, prompting: bool) -> Option<Action> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Action::Quit);
    }
    if prompting {
        return Some(match key.code {
            KeyCode::Char(c) => Action::Input(c),
            KeyCode::Backspace => Action::Backspace,
            KeyCode::Enter => Action::Submit,
            KeyCode::Esc => Action::Dismiss,
            _ => return None,
        });
    }
    Some(match key.code {
        KeyCode::Char('j') | KeyCode::Down => Action::Down,
        KeyCode::Char('k') | KeyCode::Up => Action::Up,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::Home => Action::Top,
        KeyCode::End => Action::Bottom,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Action::Enter,
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => Action::Leave,
        KeyCode::Char('s') => Action::Sort(Sort::Size),
        KeyCode::Char('n') => Action::Sort(Sort::Name),
        KeyCode::Char('C') => Action::Sort(Sort::Items),
        KeyCode::Char('M') => Action::Sort(Sort::Mtime),
        KeyCode::Char('a') => Action::ToggleApparent,
        KeyCode::Char('g') => Action::CycleGraph,
        KeyCode::Char('i') => Action::ToggleInfo,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char(' ') => Action::Mark,
        KeyCode::Char('d') => Action::StageDelete,
        KeyCode::Char('m') => Action::StageMove,
        KeyCode::Char('u') => Action::Unstage,
        KeyCode::Char('p') => Action::TogglePlan,
        KeyCode::Char('w') => Action::SavePlan,
        KeyCode::Char('c') => Action::Commit,
        KeyCode::Char('y') => Action::Confirm,
        KeyCode::Char('t') => Action::ToggleTreemap,
        KeyCode::Char('D') => Action::ToggleDupes,
        KeyCode::Char('A') => Action::StageGroupRest,
        KeyCode::Esc => Action::Dismiss,
        KeyCode::Char('q') => Action::Quit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    /// Render the browser into a fixed-size buffer and return it as plain text.
    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut state = ListState::default();
        terminal.draw(|frame| ui::draw(frame, app, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("logs")).unwrap();
        fs::write(root.join("logs/a.log"), vec![0u8; 300_000]).unwrap();
        fs::write(root.join("notes.txt"), vec![0u8; 50_000]).unwrap();
        let tree = scan(root, &ScanOptions::default(), None, None).unwrap();
        (dir, App::new(tree))
    }

    #[test]
    fn browser_shows_sizes_bars_and_names() {
        let (_d, app) = fixture();
        let out = render(&app, 90, 8);

        assert!(out.contains("ccdu"), "{out}");
        assert!(out.contains("logs/"), "{out}");
        assert!(out.contains("notes.txt"), "{out}");
        assert!(out.contains('█'), "bar graph missing:\n{out}");
        assert!(out.contains('%'), "percentages missing:\n{out}");
        assert!(out.contains("sort: size"), "{out}");
    }

    #[test]
    fn graph_cycles_off() {
        let (_d, mut app) = fixture();
        for _ in 0..4 {
            app.apply(Action::CycleGraph);
        }
        // Back to Both after a full cycle.
        assert!(render(&app, 90, 8).contains('█'));
        app.apply(Action::CycleGraph);
        app.apply(Action::CycleGraph);
        app.apply(Action::CycleGraph);
        app.apply(Action::CycleGraph);
        app.apply(Action::CycleGraph);
        let out = render(&app, 90, 8);
        assert!(!out.contains('█'), "graph should be off:\n{out}");
    }

    #[test]
    fn overlays_render_over_the_listing() {
        let (_d, mut app) = fixture();
        app.apply(Action::ToggleHelp);
        let out = render(&app, 90, 20);
        assert!(out.contains("keys"), "{out}");
        assert!(out.contains("sort by size"), "{out}");

        app.apply(Action::Dismiss);
        app.apply(Action::ToggleInfo);
        let out = render(&app, 90, 20);
        assert!(out.contains("info"), "{out}");
        assert!(out.contains("disk usage"), "{out}");
        assert!(out.contains("modified"), "{out}");
    }

    /// A warning must never be the thing that falls off the end of the header. The path gives way
    /// first, then the totals.
    #[test]
    fn header_markers_survive_a_narrow_terminal() {
        let (_d, mut app) = fixture();
        app.stale = true;
        app.apply(Action::StageDelete);

        for width in [100u16, 80, 60, 44] {
            let out = render(&app, width, 6);
            let header = out.lines().next().unwrap();
            assert!(
                header.contains("[stale]"),
                "at {width} columns the stale warning was lost:\n{header}"
            );
            assert!(
                header.contains("staged"),
                "at {width} columns the staged marker was lost:\n{header}"
            );
            assert_eq!(
                header.chars().count(),
                width as usize,
                "at {width} columns the header overflowed"
            );
        }

        // Squeezed hard, the path is what gets shortened.
        let narrow = render(&app, 44, 6);
        assert!(narrow.lines().next().unwrap().contains('…'), "{narrow}");

        // And the reason sits in the footer, where a keystroke cannot clear it. (A status
        // message still takes the line while it is showing; it is gone by the next keypress.)
        app.status = None;
        let out = render(&app, 100, 6);
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("before the commit"), "{footer}");
    }

    #[test]
    fn the_treemap_panel_draws_beside_the_listing() {
        let (_d, mut app) = fixture();
        let without = render(&app, 90, 12);
        assert!(!without.contains("treemap"));

        app.apply(Action::ToggleTreemap);
        let with = render(&app, 90, 12);

        assert!(with.contains("treemap"), "{with}");
        // The listing is still there beside it.
        assert!(with.contains("logs/"), "{with}");
        assert!(with.contains("notes.txt"), "{with}");
        // And a label made it into a tile.
        assert!(with.matches("logs").count() >= 2, "no tile label:\n{with}");
    }

    #[test]
    fn the_treemap_survives_a_panel_too_small_to_draw_in() {
        let (_d, mut app) = fixture();
        app.apply(Action::ToggleTreemap);
        // Must not panic when the split leaves almost nothing for the map.
        render(&app, 24, 4);
        render(&app, 18, 3);
    }

    #[test]
    fn duplicate_rows_stay_distinguishable_in_a_narrow_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("deeply/nested/first")).unwrap();
        fs::create_dir_all(root.join("deeply/nested/second")).unwrap();
        for name in ["deeply/nested/first/payload.bin", "deeply/nested/second/payload.bin"] {
            fs::write(root.join(name), vec![5u8; 30_000]).unwrap();
        }
        let tree = scan(root, &ScanOptions::default(), None, None).unwrap();
        let mut app = App::new(tree);

        app.apply(Action::ToggleDupes);
        for _ in 0..2000 {
            app.poll_dupes();
            if app.dupes.as_ref().is_some_and(|d| !d.is_scanning()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Copies of one file share everything but the tail. A row that shows only the shared part
        // tells you nothing about which copy it is.
        let out = render(&app, 80, 10);
        assert!(out.contains("first/payload.bin"), "cannot tell the copies apart:\n{out}");
        assert!(out.contains("second/payload.bin"), "cannot tell the copies apart:\n{out}");
        assert!(out.contains("reclaimable"), "{out}");
        assert!(!out.contains(&root.display().to_string()), "the shared root wastes the line");
    }

    #[test]
    fn a_tiny_terminal_still_renders() {
        let (_d, app) = fixture();
        // Must not panic on a viewport too small for the overlays or the footer.
        render(&app, 20, 3);
        render(&app, 8, 1);
    }

    #[test]
    fn keys_map_to_the_expected_actions() {
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(translate(&key('j'), false), Some(Action::Down));
        assert_eq!(translate(&key('s'), false), Some(Action::Sort(Sort::Size)));
        assert_eq!(translate(&key('M'), false), Some(Action::Sort(Sort::Mtime)));
        assert_eq!(translate(&key('m'), false), Some(Action::StageMove));
        assert_eq!(translate(&key('d'), false), Some(Action::StageDelete));
        assert_eq!(translate(&key(' '), false), Some(Action::Mark));
        assert_eq!(translate(&key('q'), false), Some(Action::Quit));
        assert_eq!(
            translate(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            Some(Action::Quit)
        );
        assert_eq!(translate(&key('z'), false), None);
    }

    #[test]
    fn a_prompt_turns_command_keys_back_into_text() {
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        // Without this, typing a destination like "/mnt/dump" would fire d, u, m and p.
        for c in ['d', 'u', 'm', 'p', 'q', 'w'] {
            assert_eq!(translate(&key(c), true), Some(Action::Input(c)), "{c} leaked");
        }
        assert_eq!(
            translate(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), true),
            Some(Action::Submit)
        );
        assert_eq!(
            translate(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true),
            Some(Action::Dismiss)
        );
        // Ctrl-C still gets you out of anything.
        assert_eq!(
            translate(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), true),
            Some(Action::Quit)
        );
    }
}
