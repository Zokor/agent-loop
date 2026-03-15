use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::tui::project::ProjectTab;

pub fn render(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // status card
            Constraint::Min(3),   // recent logs
        ])
        .split(area);

    render_status_card(f, chunks[0], tab);
    render_recent_logs(f, chunks[1], tab);
}

fn render_status_card(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    let status_json = &tab.cached.status_json;
    let (status, round, implementer, reviewer, mode) = if status_json.is_empty() {
        (
            "UNKNOWN".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
        )
    } else {
        match serde_json::from_str::<serde_json::Value>(status_json) {
            Ok(v) => (
                v["status"].as_str().unwrap_or("UNKNOWN").to_string(),
                v["round"]
                    .as_u64()
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "—".to_string()),
                v["implementer"].as_str().unwrap_or("—").to_string(),
                v["reviewer"].as_str().unwrap_or("—").to_string(),
                v["mode"].as_str().unwrap_or("—").to_string(),
            ),
            Err(_) => (
                "PARSE_ERROR".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
            ),
        }
    };

    let status_color = match status.as_str() {
        "APPROVED" | "CONSENSUS" => Color::Green,
        "IMPLEMENTING" | "REVIEWING" | "PLANNING" => Color::Yellow,
        "ERROR" | "STUCK" | "MAX_ROUNDS" => Color::Red,
        "INTERRUPTED" => Color::Magenta,
        _ => Color::White,
    };

    let lines = vec![
        Line::from(vec![
            Span::raw("  Status: "),
            Span::styled(&status, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(format!("  Round:  {round}")),
        Line::from(format!("  Impl:   {implementer}")),
        Line::from(format!("  Review: {reviewer}")),
        Line::from(format!("  Mode:   {mode}")),
    ];

    let card = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(card, area);
}

fn render_recent_logs(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    let lines: Vec<Line> = tab
        .cached
        .recent_logs
        .iter()
        .rev()
        .map(|msg| Line::from(msg.as_str()))
        .collect();

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Recent Logs"));
    f.render_widget(paragraph, area);
}
