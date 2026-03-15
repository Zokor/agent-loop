mod project;
mod views;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
};

use project::ProjectTab;
use views::{View, render_view};

/// Top-level TUI application state.
struct App {
    tabs: Vec<ProjectTab>,
    active_tab: usize,
    quit: bool,
    show_help: bool,
}

impl App {
    fn new(paths: Vec<PathBuf>) -> Self {
        let tabs: Vec<ProjectTab> = paths
            .into_iter()
            .filter_map(|p| ProjectTab::new(p).ok())
            .collect();

        Self {
            tabs,
            active_tab: 0,
            quit: false,
            show_help: false,
        }
    }

    fn active_tab_mut(&mut self) -> Option<&mut ProjectTab> {
        self.tabs.get_mut(self.active_tab)
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('d') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.active_view = View::Dashboard;
                }
            }
            KeyCode::Char('t') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.active_view = View::Tasks;
                }
            }
            KeyCode::Char('r') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.active_view = View::Transcript;
                }
            }
            KeyCode::Char('p') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.active_view = View::Progress;
                }
            }
            KeyCode::Char('o') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.active_view = View::Output;
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.tabs.len() {
                    self.active_tab = idx;
                }
            }
            KeyCode::Char('x') => {
                if self.tabs.len() > 1 {
                    self.tabs.remove(self.active_tab);
                    if self.active_tab >= self.tabs.len() {
                        self.active_tab = self.tabs.len() - 1;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.scroll_up();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.scroll_down();
                }
            }
            KeyCode::Enter => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.toggle_expand();
                }
            }
            _ => {}
        }
    }

    fn poll_all(&mut self) {
        for tab in &mut self.tabs {
            tab.poll();
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Length(1), // view selector
            Constraint::Min(3),   // main area
            Constraint::Length(1), // keybindings bar
        ])
        .split(f.area());

    // Tab bar
    let tab_titles: Vec<Line> = app
        .tabs
        .iter()
        .map(|t| Line::from(t.label()))
        .collect();
    let tabs_widget = Tabs::new(tab_titles)
        .select(app.active_tab)
        .style(Style::default().fg(Color::White))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .divider(Span::raw(" | "));
    f.render_widget(tabs_widget, chunks[0]);

    // View selector
    let active_view = app
        .tabs
        .get(app.active_tab)
        .map(|t| &t.active_view)
        .unwrap_or(&View::Dashboard);
    let view_labels = [
        ("d", "Dashboard", View::Dashboard),
        ("t", "Tasks", View::Tasks),
        ("r", "Transcript", View::Transcript),
        ("p", "Progress", View::Progress),
        ("o", "Output", View::Output),
    ];
    let view_spans: Vec<Span> = view_labels
        .iter()
        .flat_map(|(key, label, view)| {
            let style = if view == active_view {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            vec![
                Span::styled(format!("[{key}]"), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{label} "), style),
            ]
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(view_spans)), chunks[1]);

    // Main area
    if app.show_help {
        render_help(f, chunks[2]);
    } else if let Some(tab) = app.tabs.get(app.active_tab) {
        render_view(f, chunks[2], tab);
    } else {
        let msg = Paragraph::new("No projects loaded. Start agent-loop in a project directory first.")
            .block(Block::default().borders(Borders::ALL).title("agent-loop TUI"));
        f.render_widget(msg, chunks[2]);
    }

    // Keybindings bar
    let help_line = Line::from(vec![
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::raw(":quit  "),
        Span::styled("1-9", Style::default().fg(Color::Yellow)),
        Span::raw(":tab  "),
        Span::styled("d/t/r/p/o", Style::default().fg(Color::Yellow)),
        Span::raw(":view  "),
        Span::styled("j/k", Style::default().fg(Color::Yellow)),
        Span::raw(":scroll  "),
        Span::styled("x", Style::default().fg(Color::Yellow)),
        Span::raw(":close  "),
        Span::styled("?", Style::default().fg(Color::Yellow)),
        Span::raw(":help"),
    ]);
    f.render_widget(Paragraph::new(help_line), chunks[3]);
}

fn render_help(f: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::from(""),
        Line::from(Span::styled("  agent-loop TUI — Keyboard Shortcuts", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("  q           Quit"),
        Line::from("  1-9         Switch project tab"),
        Line::from("  d           Dashboard view"),
        Line::from("  t           Tasks view"),
        Line::from("  r           Transcript view"),
        Line::from("  p           Progress view"),
        Line::from("  o           Output view"),
        Line::from("  j/k/↑/↓    Scroll up/down"),
        Line::from("  Enter       Expand/collapse entry"),
        Line::from("  x           Close current tab"),
        Line::from("  ?           Toggle this help"),
        Line::from(""),
    ];
    let help = Paragraph::new(help_text)
        .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(help, area);
}

/// Entry point for the TUI.
pub fn run(paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let resolved_paths = if paths.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        paths
    };

    let mut app = App::new(resolved_paths);

    if app.tabs.is_empty() {
        eprintln!("No valid agent-loop projects found. Ensure .agent-loop/state/ exists.");
        return Ok(());
    }

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(200);

    loop {
        // Poll DB on each tick
        app.poll_all();

        terminal.draw(|f| draw(f, &app))?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }

        if app.quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybinding_dispatch_switches_views() {
        let mut app = App {
            tabs: vec![],
            active_tab: 0,
            quit: false,
            show_help: false,
        };

        // Can't test view switching without tabs, but quit works
        app.handle_key(KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn help_toggle() {
        let mut app = App {
            tabs: vec![],
            active_tab: 0,
            quit: false,
            show_help: false,
        };

        app.handle_key(KeyCode::Char('?'));
        assert!(app.show_help);
        app.handle_key(KeyCode::Char('?'));
        assert!(!app.show_help);
    }
}
