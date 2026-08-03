//! Rendering. All of it reads [`App`]; none of it mutates browsing state.

use ccdu_core::format::{format_time, human_count, human_size};
use ccdu_core::model::{flags, NodeId};
use ccdu_core::scan::Progress;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Graph};

const BAR_WIDTH: usize = 12;

/// The scanning screen, shown until the walk finishes.
pub fn draw_scanning(frame: &mut Frame, root: &std::path::Path, p: &Progress, spin: usize) {
    const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
    let [_, middle, _] =
        Layout::vertical([Constraint::Percentage(40), Constraint::Length(6), Constraint::Min(0)])
            .areas(frame.area());

    let body = vec![
        Line::from(vec![
            Span::styled(format!("{} ", SPINNER[spin % SPINNER.len()]), Style::new().cyan()),
            Span::styled("scanning ", Style::new().bold()),
            Span::raw(root.display().to_string()),
        ]),
        Line::raw(""),
        Line::from(format!(
            "{} entries in {} directories, {}",
            human_count(p.entries),
            human_count(p.dirs),
            human_size(p.disk)
        )),
        Line::styled(p.current.display().to_string(), Style::new().fg(Color::DarkGray)),
        Line::raw(""),
        Line::styled("q to stop and browse what has been found", Style::new().fg(Color::DarkGray)),
    ];

    frame.render_widget(
        Paragraph::new(body).alignment(Alignment::Center).block(Block::new()),
        middle,
    );
}

/// The browser: header, listing, footer, plus any overlay.
pub fn draw(frame: &mut Frame, app: &App, list_state: &mut ListState) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
            .areas(frame.area());

    draw_header(frame, app, header);
    draw_listing(frame, app, body, list_state);
    draw_footer(frame, app, footer);

    if app.show_info {
        draw_info(frame, app);
    }
    if app.show_help {
        draw_help(frame);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let node = app.tree.node(app.dir);
    let mut spans = vec![
        Span::styled(" ccdu ", Style::new().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::raw(" "),
        Span::styled(app.tree.path_of(app.dir).display().to_string(), Style::new().bold()),
        Span::raw("  "),
        Span::raw(format!(
            "{} in {} items",
            human_size(app.size_of(app.dir)),
            human_count(node.items as u64)
        )),
    ];
    if app.tree.cancelled {
        spans.push(Span::styled("  [partial scan]", Style::new().fg(Color::Yellow)));
    }
    if app.tree.errors > 0 {
        spans.push(Span::styled(
            format!("  [{} unreadable]", app.tree.errors),
            Style::new().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_listing(frame: &mut Frame, app: &App, area: Rect, list_state: &mut ListState) {
    if app.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("  (empty)", Style::new().fg(Color::DarkGray))),
            area,
        );
        return;
    }

    let max = app.max_row_size().max(1);
    let total = app.size_of(app.dir).max(1);
    let items: Vec<ListItem> =
        app.rows.iter().map(|&id| ListItem::new(row_line(app, id, max, total))).collect();

    list_state.select(Some(app.cursor));
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        list_state,
    );
}

fn row_line(app: &App, id: NodeId, max: u64, total: u64) -> Line<'static> {
    let node = app.tree.node(id);
    let size = app.size_of(id);
    let mut spans =
        vec![Span::styled(format!("{:>10}  ", human_size(size)), Style::new().fg(Color::Green))];

    if matches!(app.graph, Graph::Bar | Graph::Both) {
        let filled = ((size as u128 * BAR_WIDTH as u128) / max as u128) as usize;
        let bar: String = std::iter::repeat_n('█', filled)
            .chain(std::iter::repeat_n(' ', BAR_WIDTH - filled))
            .collect();
        spans.push(Span::styled(format!("[{bar}] "), Style::new().fg(Color::Blue)));
    }
    if matches!(app.graph, Graph::Percent | Graph::Both) {
        let pct = size as f64 * 100.0 / total as f64;
        spans.push(Span::styled(format!("{pct:>5.1}%  "), Style::new().fg(Color::DarkGray)));
    }

    let mut name = app.tree.name(id).to_string_lossy().into_owned();
    if node.is_dir() {
        name.push('/');
    }
    spans.push(Span::styled(name, name_style(node.flags)));

    if let Some(note) = annotation(node.flags) {
        spans.push(Span::styled(format!("  {note}"), Style::new().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn name_style(f: u16) -> Style {
    if f & flags::ERR != 0 {
        Style::new().fg(Color::Red)
    } else if f & (flags::HARDLINK_DUP | flags::EXCLUDED | flags::LOOP) != 0 {
        Style::new().fg(Color::DarkGray)
    } else if f & flags::SYMLINK != 0 {
        Style::new().fg(Color::Magenta)
    } else if f & flags::DIR != 0 {
        Style::new().fg(Color::Cyan).bold()
    } else if f & flags::OTHER != 0 {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    }
}

/// A short reason why an entry's size may not be what it looks like.
fn annotation(f: u16) -> Option<&'static str> {
    if f & flags::ERR != 0 {
        Some("unreadable")
    } else if f & flags::EXCLUDED != 0 {
        Some("excluded")
    } else if f & flags::OTHER_FS != 0 {
        Some("other filesystem")
    } else if f & flags::HARDLINK_DUP != 0 {
        Some("hardlink, counted elsewhere")
    } else if f & flags::LOOP != 0 {
        Some("already visited")
    } else {
        None
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let dir = if app.reverse { "▲" } else { "▼" };
    let mode = if app.apparent { "apparent" } else { "disk" };
    let left = format!(" sort: {}{dir}  size: {mode}", app.sort.label());
    let right = "↑↓ move  ⏎ open  ← up  s n C M sort  a size  g graph  i info  ? help  q quit ";

    // Widths are in characters, not bytes — the arrows and the return symbol are multi-byte. If
    // the hint cannot fit with a gap, drop it rather than let the two halves collide.
    let width = area.width as usize;
    let (left_w, right_w) = (left.chars().count(), right.chars().count());
    let line = match width.checked_sub(left_w + right_w) {
        Some(pad) => format!("{left}{}{right}", " ".repeat(pad)),
        None => left,
    };

    frame.render_widget(
        Paragraph::new(Line::styled(line, Style::new().bg(Color::DarkGray).fg(Color::White))),
        area,
    );
}

fn draw_info(frame: &mut Frame, app: &App) {
    let Some(id) = app.selected() else { return };
    let node = app.tree.node(id);

    let kind = if node.has(flags::SYMLINK) {
        "symlink"
    } else if node.is_dir() {
        "directory"
    } else if node.has(flags::OTHER) {
        "special file"
    } else {
        "file"
    };

    let mut lines = vec![
        field("path", app.tree.path_of(id).display().to_string()),
        field("type", kind.to_string()),
        field("disk usage", human_size(node.disk)),
        field("apparent", human_size(node.apparent)),
        field("modified", format_time(node.mtime)),
    ];
    if node.is_dir() {
        lines.push(field("items", human_count(node.items as u64)));
    }
    if let Some(note) = annotation(node.flags) {
        lines.push(field("note", note.to_string()));
    }

    let area = centered(frame.area(), 72, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::new().borders(Borders::ALL).title(" info ").border_style(Style::new().cyan()),
        ),
        area,
    );
}

fn field(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<12}"), Style::new().fg(Color::DarkGray)),
        Span::raw(value),
    ])
}

fn draw_help(frame: &mut Frame) {
    const KEYS: [(&str, &str); 13] = [
        ("↑ ↓ j k", "move the cursor"),
        ("PgUp PgDn", "move a page"),
        ("Home End", "first / last entry"),
        ("⏎ → l", "open the selected directory"),
        ("← h Backspace", "go to the parent directory"),
        ("s", "sort by size (again to reverse)"),
        ("n", "sort by name"),
        ("C", "sort by item count"),
        ("M", "sort by modification time"),
        ("a", "toggle apparent size vs disk usage"),
        ("g", "cycle graph: bar, percent, both, off"),
        ("i", "show details for the selected entry"),
        ("q Esc", "quit"),
    ];

    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!(" {key:<16}"), Style::new().fg(Color::Cyan)),
                Span::raw(*desc),
            ])
        })
        .collect();

    let area = centered(frame.area(), 60, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new().borders(Borders::ALL).title(" keys ").border_style(Style::new().cyan()),
        ),
        area,
    );
}

/// A `w` by `h` rectangle in the middle of `area`, clamped to fit.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
