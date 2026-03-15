use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Cell, Row, Table},
};

use crate::tui::project::ProjectTab;

struct TaskRow {
    index: usize,
    title: String,
    status: String,
    retries: String,
    wave: String,
}

pub fn render(f: &mut Frame, area: Rect, tab: &ProjectTab) {
    let json = &tab.cached.task_status_json;

    let task_rows = parse_tasks(json);

    let rows: Vec<Row> = if task_rows.is_empty() {
        vec![Row::new(vec![Cell::from("No task data available")])]
    } else {
        task_rows
            .iter()
            .map(|t| {
                use ratatui::style::Color;
                let status_color = match t.status.as_str() {
                    "done" => Color::Green,
                    "running" => Color::Yellow,
                    "failed" => Color::Red,
                    "skipped" => Color::DarkGray,
                    _ => Color::White,
                };
                Row::new(vec![
                    Cell::from(format!("{}", t.index)),
                    Cell::from(truncate(&t.title, 50)),
                    Cell::from(t.status.clone()).style(Style::default().fg(status_color)),
                    Cell::from(t.retries.clone()),
                    Cell::from(t.wave.clone()),
                ])
            })
            .collect()
    };

    let header = Row::new(vec!["#", "Title", "Status", "Retries", "Wave"])
        .style(Style::default().add_modifier(Modifier::BOLD))
        .bottom_margin(1);

    let widths = [
        ratatui::layout::Constraint::Length(4),
        ratatui::layout::Constraint::Min(20),
        ratatui::layout::Constraint::Length(10),
        ratatui::layout::Constraint::Length(8),
        ratatui::layout::Constraint::Length(6),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Tasks"));

    f.render_widget(table, area);
}

fn parse_tasks(json: &str) -> Vec<TaskRow> {
    if json.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(tasks) = v["tasks"].as_array() else {
        return Vec::new();
    };
    tasks
        .iter()
        .enumerate()
        .map(|(i, task)| TaskRow {
            index: i + 1,
            title: task["title"].as_str().unwrap_or("—").to_string(),
            status: task["status"].as_str().unwrap_or("—").to_string(),
            retries: task["retries"]
                .as_u64()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "0".to_string()),
            wave: task["wave_index"]
                .as_u64()
                .map(|w| w.to_string())
                .unwrap_or_else(|| "—".to_string()),
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{truncated}...")
    }
}
