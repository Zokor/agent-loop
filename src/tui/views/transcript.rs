use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
};

use crate::tui::project::ProjectTab;

pub fn render(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    // Transcript data comes from recent_logs for now.
    // A full implementation would query transcript_entries from the DB.
    let items: Vec<ListItem> = tab
        .cached
        .recent_logs
        .iter()
        .rev()
        .enumerate()
        .map(|(i, msg)| {
            let is_expanded = tab.expanded_entry == Some(i);
            let style = if is_expanded {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };

            let prefix = if is_expanded { "▼" } else { "▶" };
            let line = Line::from(vec![
                Span::styled(format!("{prefix} "), Style::default().fg(Color::DarkGray)),
                Span::styled(truncate_line(msg, 120), style),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            "Transcript",
            Style::default().add_modifier(Modifier::BOLD),
        )));

    f.render_widget(list, area);
}

fn truncate_line(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{truncated}...")
    }
}
