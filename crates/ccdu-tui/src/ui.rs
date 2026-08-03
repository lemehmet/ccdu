//! Rendering. All of it reads [`App`]; none of it mutates browsing state.

use ccdu_core::format::{format_time, human_count, human_size};
use ccdu_core::model::{flags, NodeId};
use ccdu_core::plan::{Severity, ValidateOptions};
use ccdu_core::scan::Progress;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Graph, StagedKind, View};

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

    frame.render_widget(Paragraph::new(body).alignment(Alignment::Center), middle);
}

/// Header, body, footer, plus whatever overlay is open.
pub fn draw(frame: &mut Frame, app: &App, list_state: &mut ListState) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
            .areas(frame.area());

    match app.view {
        View::Browse => {
            draw_header(frame, app, header);
            draw_listing(frame, app, body, list_state);
        }
        View::Plan => {
            draw_plan_header(frame, app, header);
            draw_plan(frame, app, body, list_state);
        }
    }
    draw_footer(frame, app, footer);

    if app.show_info {
        draw_info(frame, app);
    }
    if app.show_help {
        draw_help(frame, app.view);
    }
    if app.prompt.is_some() {
        draw_prompt(frame, app);
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
    if !app.staged.is_empty() {
        spans.push(Span::styled(
            format!("  [{} staged, {}]", app.staged.len(), human_size(app.staged_bytes())),
            Style::new().fg(Color::Magenta).bold(),
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

    let mut spans = vec![stage_marker(app, id)];
    spans.push(Span::styled(format!("{:>10}  ", human_size(size)), Style::new().fg(Color::Green)));

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

/// The two-column gutter: what is marked, and what is staged.
fn stage_marker(app: &App, id: NodeId) -> Span<'static> {
    let marked = app.marks.contains(&id);
    let (glyph, style) = match app.staged.get(&id).map(|s| &s.kind) {
        Some(StagedKind::Delete) => ("D", Style::new().fg(Color::Red).bold()),
        Some(StagedKind::Move(_)) => ("M", Style::new().fg(Color::Yellow).bold()),
        None if marked => ("*", Style::new().fg(Color::Magenta).bold()),
        None => (" ", Style::new()),
    };
    let prefix = if marked { "*" } else { " " };
    Span::styled(format!("{prefix}{glyph}"), style)
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

fn draw_plan_header(frame: &mut Frame, app: &App, area: Rect) {
    let errors = app.findings.iter().filter(|f| f.severity == Severity::Error).count();
    let warnings = app.findings.len() - errors;

    let mut spans = vec![
        Span::styled(" plan ", Style::new().bg(Color::Magenta).fg(Color::Black).bold()),
        Span::raw(" "),
        Span::raw(format!("{} operations", app.plan.ops.len())),
        Span::raw("  "),
        Span::styled(
            format!("reclaims {}", human_size(app.plan.delete_bytes())),
            Style::new().fg(Color::Green),
        ),
    ];
    if app.plan.move_bytes() > 0 {
        spans.push(Span::raw(format!("  moves {}", human_size(app.plan.move_bytes()))));
    }
    if errors > 0 {
        spans.push(Span::styled(
            format!("  {errors} error{}", plural(errors)),
            Style::new().fg(Color::Red).bold(),
        ));
    }
    if warnings > 0 {
        spans.push(Span::styled(
            format!("  {warnings} warning{}", plural(warnings)),
            Style::new().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn draw_plan(frame: &mut Frame, app: &App, area: Rect, list_state: &mut ListState) {
    if app.plan.ops.is_empty() {
        let hint = vec![
            Line::styled("  nothing staged yet", Style::new().fg(Color::DarkGray)),
            Line::raw(""),
            Line::styled(
                "  in the browser: space to mark, d to stage a deletion, m to stage a move",
                Style::new().fg(Color::DarkGray),
            ),
        ];
        frame.render_widget(Paragraph::new(hint), area);
        return;
    }

    let items: Vec<ListItem> = app
        .plan
        .ops
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    if op.is_delete() { " D " } else { " M " },
                    if op.is_delete() {
                        Style::new().fg(Color::Red).bold()
                    } else {
                        Style::new().fg(Color::Yellow).bold()
                    },
                ),
                Span::styled(
                    format!("{:>10}  ", human_size(op.est_bytes())),
                    Style::new().fg(Color::Green),
                ),
                Span::raw(op.source().display().to_string()),
            ])];
            if let Some(dst) = op.destination() {
                lines.push(Line::from(vec![
                    Span::raw("              → "),
                    Span::styled(dst.display().to_string(), Style::new().fg(Color::Cyan)),
                ]));
            }
            // Findings sit under the operation they belong to, where they can be acted on.
            for finding in app.findings.iter().filter(|f| f.op == Some(i)) {
                lines.push(finding_line(finding, "    "));
            }
            ListItem::new(lines)
        })
        .collect();

    // Plan-wide findings have no operation to sit under, so they lead.
    let mut all = Vec::new();
    let global: Vec<Line> =
        app.findings.iter().filter(|f| f.op.is_none()).map(|f| finding_line(f, " ")).collect();
    if !global.is_empty() {
        all.push(ListItem::new(global));
    }
    all.extend(items);

    let offset = all.len() - app.plan.ops.len();
    list_state.select(Some(app.plan_cursor + offset));
    frame.render_stateful_widget(
        List::new(all).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        list_state,
    );
}

fn finding_line(finding: &ccdu_core::plan::Finding, indent: &str) -> Line<'static> {
    let (glyph, style) = match finding.severity {
        Severity::Error => ("✗", Style::new().fg(Color::Red)),
        Severity::Warning => ("!", Style::new().fg(Color::Yellow)),
    };
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(format!("{glyph} {}", finding.message), style),
    ])
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // A status message is a reply to something the user just did, so it outranks the key hints.
    if let Some(status) = &app.status {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {status}"),
                Style::new().bg(Color::Blue).fg(Color::White),
            )),
            area,
        );
        return;
    }

    let (left, right) = match app.view {
        View::Browse => (
            format!(
                " sort: {}{}  size: {}",
                app.sort.label(),
                if app.reverse { "▲" } else { "▼" },
                if app.apparent { "apparent" } else { "disk" }
            ),
            "space mark  d delete  m move  u unstage  p plan  a size  i info  ? help  q quit ",
        ),
        View::Plan => (
            format!(" {} staged", app.staged.len()),
            "↑↓ move  u unstage  w write plan  p back  ? help  q back ",
        ),
    };

    // Widths are in characters, not bytes — the arrows and the return symbol are multi-byte. If
    // the hint cannot fit with a gap, drop it rather than let the two halves collide.
    let width = area.width as usize;
    let line = match width.checked_sub(left.chars().count() + right.chars().count()) {
        Some(pad) => format!("{left}{}{right}", " ".repeat(pad)),
        None => left,
    };

    frame.render_widget(
        Paragraph::new(Line::styled(line, Style::new().bg(Color::DarkGray).fg(Color::White))),
        area,
    );
}

fn draw_prompt(frame: &mut Frame, app: &App) {
    let Some(prompt) = &app.prompt else { return };

    let lines = vec![
        Line::styled(format!(" {}", prompt.label), Style::new().fg(Color::DarkGray)),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(prompt.input.clone(), Style::new().fg(Color::Cyan)),
            Span::styled("▏", Style::new().add_modifier(Modifier::SLOW_BLINK)),
        ]),
        Line::styled(" ⏎ to stage, Esc to cancel", Style::new().fg(Color::DarkGray)),
    ];

    let area = centered(frame.area(), 76, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" move ")
                .border_style(Style::new().fg(Color::Yellow)),
        ),
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
    match app.staged.get(&id).map(|s| &s.kind) {
        Some(StagedKind::Delete) => lines.push(field("staged", "delete".to_string())),
        Some(StagedKind::Move(dst)) => {
            lines.push(field("staged", format!("move to {}", dst.display())))
        }
        None => {}
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

fn draw_help(frame: &mut Frame, view: View) {
    const BROWSE: [(&str, &str); 16] = [
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
        ("Space", "mark an entry; actions apply to all marks"),
        ("d / m", "stage a deletion / a move"),
        ("u / p", "unstage / open the plan"),
        ("q Esc", "quit"),
    ];
    const PLAN: [(&str, &str); 5] = [
        ("↑ ↓ j k", "move the cursor"),
        ("u", "unstage the selected operation"),
        ("w", "write the plan to the plan store"),
        ("p Esc q", "back to the browser"),
        ("", "nothing here has touched the disk yet"),
    ];

    let keys: &[(&str, &str)] = match view {
        View::Browse => &BROWSE,
        View::Plan => &PLAN,
    };
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!(" {key:<16}"), Style::new().fg(Color::Cyan)),
                Span::raw(*desc),
            ])
        })
        .collect();

    let area = centered(frame.area(), 64, lines.len() as u16 + 2);
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

/// Validation settings the interface uses. Split out so the CLI and the TUI agree.
pub fn validate_options() -> ValidateOptions {
    ValidateOptions::default()
}
