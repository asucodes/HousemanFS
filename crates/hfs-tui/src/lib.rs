//! The finder interface.
//!
//! This crate renders. It holds no volume handles, opens no files, and cannot initiate an
//! action — the architecture requires that the interface decides nothing, so that a rendering
//! bug can never become a filesystem bug. Everything it displays is a value it was given.
//!
//! The look is deliberate: light blue, monospace, and built to resemble a machine from the
//! 1980s. Not nostalgia for its own sake. A tool that reads a user's entire disk and reports
//! what it finds should feel like an instrument rather than a product — plain, inspectable,
//! and obviously not trying to sell anything.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hfs_index::Index;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

/// The 1980s palette. Light blue field, dark navy ink — the colours of a machine that printed
/// text and nothing else, which is all this needs to do.
const FIELD: Color = Color::Rgb(0xB5, 0xD4, 0xF4);
const PANEL: Color = Color::Rgb(0xE6, 0xF1, 0xFB);
const INK: Color = Color::Rgb(0x04, 0x2C, 0x53);
const ACCENT: Color = Color::Rgb(0x18, 0x5F, 0xA5);
const SELECTED: Color = Color::Rgb(0x85, 0xB7, 0xEB);

/// Which list is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Dirs,
    Files,
    Ext,
}

impl View {
    fn title(self) -> &'static str {
        match self {
            Self::Dirs => "largest directories",
            Self::Files => "largest files",
            Self::Ext => "occupied by extension",
        }
    }
}

/// One rendered row. Kept as plain strings because the interface only formats what it is given.
struct Row {
    size: String,
    label: String,
    detail: String,
}

struct App {
    index: Index,
    view: View,
    rows: Vec<Row>,
    selected: usize,
    /// True while the user is typing a search term, so keys go to the query rather than the list.
    searching: bool,
    query: String,
    status: String,
    volume: String,
    filesystem: String,
}

impl App {
    fn new(index: Index) -> Self {
        let volume = index
            .meta("root")
            .unwrap_or_else(|| "(unknown)".to_string());
        let filesystem = index.meta("filesystem").unwrap_or_else(|| "?".to_string());
        Self {
            index,
            view: View::Dirs,
            rows: Vec::new(),
            selected: 0,
            searching: false,
            query: String::new(),
            status: String::new(),
            volume,
            filesystem,
        }
    }

    fn reload(&mut self) {
        self.selected = 0;
        self.rows.clear();
        self.status.clear();

        let result = match self.view {
            View::Dirs => self.index.largest_dirs(500).map(|rows| {
                rows.into_iter()
                    .map(|(path, allocated, count)| Row {
                        size: human(allocated),
                        label: path,
                        detail: format!("{count} files"),
                    })
                    .collect()
            }),
            View::Files => self.index.largest_files(500).map(|rows| {
                rows.into_iter()
                    .map(|hit| Row {
                        size: human(hit.allocated),
                        label: hit.path,
                        detail: if hit.links > 1 {
                            format!("{} names share this data", hit.links)
                        } else {
                            String::new()
                        },
                    })
                    .collect()
            }),
            View::Ext => self.index.by_extension(200).map(|rows| {
                rows.into_iter()
                    .map(|(ext, allocated, count)| Row {
                        size: human(allocated),
                        label: if ext.is_empty() {
                            "(no extension)".to_string()
                        } else {
                            ext
                        },
                        detail: format!("{count} files"),
                    })
                    .collect()
            }),
        };

        match result {
            Ok(rows) => self.rows = rows,
            Err(err) => self.status = format!("query failed: {err}"),
        }
    }

    fn run_search(&mut self) {
        let term = self.query.trim().to_string();
        if term.is_empty() {
            self.reload();
            return;
        }

        match self.index.search_name(&term, 500) {
            Ok(hits) => {
                self.rows = hits
                    .into_iter()
                    .map(|hit| Row {
                        size: human(hit.allocated),
                        label: hit.path,
                        detail: String::new(),
                    })
                    .collect();
                self.selected = 0;
                if self.rows.is_empty() {
                    self.status = format!("nothing matches {term:?}");
                } else {
                    self.status = format!("{} matches for {term:?}", self.rows.len());
                }
            }
            Err(err) => self.status = format!("search failed: {err}"),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }
}

/// Run the finder against an index.
pub fn run(db: &str) -> Result<(), String> {
    let index = Index::open(db).map_err(|e| e.to_string())?;
    let mut app = App::new(index);
    app.reload();

    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;

    let outcome = event_loop(&mut terminal, &mut app);

    // Restore the terminal on every path out, including failure. Leaving a user's terminal in
    // raw mode with an alternate screen active is a worse bug than anything this tool does.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    outcome
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|e| e.to_string())?;

        let event = event::read().map_err(|e| e.to_string())?;
        let Event::Key(key) = event else { continue };
        // Windows reports both press and release; acting on both double-fires every key.
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.searching {
            match key.code {
                KeyCode::Enter => {
                    app.searching = false;
                    app.run_search();
                }
                KeyCode::Esc => {
                    app.searching = false;
                    app.query.clear();
                    app.reload();
                }
                KeyCode::Backspace => {
                    app.query.pop();
                }
                KeyCode::Char(c) => app.query.push(c),
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('1') => {
                app.view = View::Dirs;
                app.reload();
            }
            KeyCode::Char('2') => {
                app.view = View::Files;
                app.reload();
            }
            KeyCode::Char('3') => {
                app.view = View::Ext;
                app.reload();
            }
            KeyCode::Char('/') => {
                app.searching = true;
                app.query.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::PageUp => app.move_selection(-20),
            KeyCode::PageDown => app.move_selection(20),
            KeyCode::Home => app.selected = 0,
            KeyCode::End => app.selected = app.rows.len().saturating_sub(1),
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // A solid field behind everything, so the whole surface reads as one instrument.
    frame.render_widget(Block::default().style(Style::default().bg(FIELD)), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, rows[0], app);
    draw_body(frame, rows[1], app);
    draw_footer(frame, rows[2], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let left = Line::from(vec![
        Span::styled(" housemanfs 0.1 ", Style::default().fg(PANEL).bg(ACCENT)),
        Span::styled(
            format!("  {}  {} ", app.volume, app.filesystem),
            Style::default().fg(INK).bg(FIELD),
        ),
    ]);

    let right = Span::styled(
        " read-only ",
        Style::default()
            .fg(INK)
            .bg(FIELD)
            .add_modifier(Modifier::BOLD),
    );

    let width = area.width as usize;
    let used = 15 + app.volume.len() + app.filesystem.len() + 4;
    let pad = width.saturating_sub(used + 11);

    let line = Line::from(vec![
        left.spans[0].clone(),
        left.spans[1].clone(),
        Span::raw(" ".repeat(pad)),
        right,
    ]);

    frame.render_widget(Paragraph::new(line).style(Style::default().bg(FIELD)), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);

    draw_list(frame, columns[0], app);
    draw_detail(frame, columns[1], app);
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let title = if app.searching {
        format!(" search: {}_ ", app.query)
    } else {
        format!(" {} ", app.view.title())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(title, Style::default().fg(ACCENT)))
        .style(Style::default().bg(PANEL));

    let inner_width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let detail = if row.detail.is_empty() {
                String::new()
            } else {
                format!("  {}", row.detail)
            };
            let budget = inner_width
                .saturating_sub(row.size.len())
                .saturating_sub(detail.len())
                .saturating_sub(2);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>11} ", row.size), Style::default().fg(ACCENT)),
                Span::raw(shorten(&row.label, budget)),
                Span::styled(detail, Style::default().fg(ACCENT)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(block)
        .style(Style::default().fg(INK))
        .highlight_style(Style::default().bg(SELECTED).fg(INK));

    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(" detail ", Style::default().fg(ACCENT)))
        .style(Style::default().bg(PANEL));

    let lines = match app.rows.get(app.selected) {
        Some(row) => vec![
            Line::from(Span::styled(
                format!(" {} ", row.size),
                Style::default().fg(PANEL).bg(ACCENT),
            )),
            Line::from(""),
            Line::from(Span::styled(row.label.clone(), Style::default().fg(INK))),
            Line::from(""),
            Line::from(Span::styled(
                row.detail.clone(),
                Style::default().fg(ACCENT),
            )),
        ],
        None => vec![Line::from(Span::styled(
            " nothing selected ",
            Style::default().fg(ACCENT),
        ))],
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(PANEL)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let hints = if app.status.is_empty() {
        " 1 dirs   2 files   3 ext   / search   q quit ".to_string()
    } else {
        format!(" {} ", app.status)
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints,
            Style::default().fg(PANEL).bg(ACCENT),
        )))
        .style(Style::default().bg(FIELD)),
        area,
    );
}

/// Shorten text to fit, keeping the end. The tail of a path is the part that identifies it.
fn shorten(text: &str, width: usize) -> String {
    if width < 8 || text.chars().count() <= width {
        return text.to_string();
    }
    let keep = width - 3;
    let tail: String = text
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

/// Binary units, labelled honestly. Windows shows binary units but calls them GB.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_uses_binary_units() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.00 KiB");
        assert_eq!(human(1024 * 1024), "1.00 MiB");
    }

    #[test]
    fn shorten_keeps_the_end_of_a_path() {
        let long = r"C:\very\deep\path\to\something\interesting\file.bin";
        let out = shorten(long, 20);
        assert!(out.starts_with("..."));
        assert!(out.ends_with("file.bin"));
        assert!(out.chars().count() <= 20);
    }

    #[test]
    fn shorten_leaves_short_text_alone() {
        assert_eq!(shorten("C:\\a", 20), "C:\\a");
    }
}
