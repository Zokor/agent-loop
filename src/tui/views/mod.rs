pub mod dashboard;
pub mod output;
pub mod progress;
pub mod tasks;
pub mod transcript;

use ratatui::{Frame, layout::Rect};

use super::project::ProjectTab;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Tasks,
    Transcript,
    Progress,
    Output,
}

impl Default for View {
    fn default() -> Self {
        Self::Dashboard
    }
}

pub fn render_view(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    match &tab.active_view {
        View::Dashboard => dashboard::render(f, area, tab),
        View::Tasks => tasks::render(f, area, tab),
        View::Transcript => transcript::render(f, area, tab),
        View::Progress => progress::render(f, area, tab),
        View::Output => output::render(f, area, tab),
    }
}
