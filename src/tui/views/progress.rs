use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::tui::project::ProjectTab;

pub fn render(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    let content = if tab.cached.progress.is_empty() {
        "No progress data available.".to_string()
    } else {
        tab.cached.progress.clone()
    };

    let scroll = tab.scroll_offset;
    let paragraph = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title("Progress"))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    f.render_widget(paragraph, area);
}
