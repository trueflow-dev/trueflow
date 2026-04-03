use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ReviewMode {
    Incremental,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub repository_name: String,
    pub include_generated: bool,
    pub review_batch_size: usize,
    pub max_file_bytes: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            repository_name: "review-bench-workspace".to_string(),
            include_generated: false,
            review_batch_size: 24,
            max_file_bytes: 256 * 1024,
        }
    }
}

impl AppConfig {
    pub fn effective_batch_size(&self, mode: ReviewMode) -> usize {
        match mode {
            ReviewMode::Incremental => self.review_batch_size,
            ReviewMode::Full => self.review_batch_size.saturating_mul(2),
        }
    }
}
