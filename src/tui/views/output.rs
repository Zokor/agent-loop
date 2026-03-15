use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::tui::project::ProjectTab;

pub fn render(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    let content = if tab.cached.output.is_empty() {
        "No output data available.".to_string()
    } else {
        // ansi-to-tui could be used here for ANSI rendering,
        // but for simplicity we render plain text for now.
        tab.cached.output.clone()
    };

    let scroll = tab.scroll_offset;
    let paragraph = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title("Output"))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    f.render_widget(paragraph, area);
}
