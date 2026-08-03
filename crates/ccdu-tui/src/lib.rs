//! Terminal user interface for ccdu.
//!
//! Two phases: a scanning screen that can be cut short, then the browser. The browser only ever
//! reads the tree, so the same [`App`] drives both this and the snapshot tests.

pub mod app;
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

fn browse(terminal: &mut DefaultTerminal, tree: Tree) -> io::Result<()> {
    let mut app = App::new(tree);
    let mut list_state = ListState::default();

    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, &app, &mut list_state))?;

        if !event::poll(TICK)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(action) = translate(&key) {
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

/// Map a key press onto a browser action. Keys follow ncdu wherever it has one.
fn translate(key: &KeyEvent) -> Option<Action> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Action::Quit);
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
        assert_eq!(translate(&key('j')), Some(Action::Down));
        assert_eq!(translate(&key('s')), Some(Action::Sort(Sort::Size)));
        assert_eq!(translate(&key('M')), Some(Action::Sort(Sort::Mtime)));
        assert_eq!(translate(&key('q')), Some(Action::Quit));
        assert_eq!(
            translate(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(translate(&key('z')), None);
    }
}
