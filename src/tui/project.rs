use std::path::PathBuf;

use crate::db::Db;

use super::views::View;

/// Cached state read from DB on each tick.
#[derive(Default)]
pub struct CachedState {
    pub status_json: String,
    pub task_status_json: String,
    pub task_metrics_json: String,
    pub recent_logs: Vec<String>,
    pub progress: String,
    pub output: String,
    #[allow(dead_code)]
    pub transcript_count: i64,
}

/// A tab representing one agent-loop project.
pub struct ProjectTab {
    pub path: PathBuf,
    pub label: String,
    pub active_view: View,
    pub scroll_offset: u16,
    pub expanded_entry: Option<usize>,
    pub cached: CachedState,
    db: Option<Db>,
}

impl ProjectTab {
    pub fn new(path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        let db_path = path
            .join(".agent-loop")
            .join("state")
            .join("agent-loop.db");

        let db = if db_path.exists() {
            Db::open_readonly(&db_path).ok()
        } else {
            None
        };

        Ok(Self {
            path,
            label,
            active_view: View::Dashboard,
            scroll_offset: 0,
            expanded_entry: None,
            cached: CachedState::default(),
            db,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Refresh cached state from DB. Called on each 200ms tick.
    pub fn poll(&mut self) {
        let Some(db) = &self.db else {
            // Try to open DB if it now exists
            let db_path = self.path
                .join(".agent-loop")
                .join("state")
                .join("agent-loop.db");
            if db_path.exists() {
                self.db = Db::open_readonly(&db_path).ok();
            }
            return;
        };

        self.cached.status_json = db.read_status().unwrap_or_default();
        self.cached.task_status_json = db.read_task_status();
        self.cached.task_metrics_json = db.read_task_metrics();
        self.cached.recent_logs = db.read_recent_logs(20);
        self.cached.progress = db.read_document("planning-progress.md");
        if self.cached.progress.is_empty() {
            self.cached.progress = db.read_document("implement-progress.md");
        }
        self.cached.output = db.read_document("changes.md");
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn toggle_expand(&mut self) {
        // Toggle expand on current scroll position (used in transcript view)
        let idx = self.scroll_offset as usize;
        if self.expanded_entry == Some(idx) {
            self.expanded_entry = None;
        } else {
            self.expanded_entry = Some(idx);
        }
    }
}
