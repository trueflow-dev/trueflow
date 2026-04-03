pub mod config;
pub mod indexer;
pub mod metrics;
pub mod review;
pub mod store;

use crate::config::{AppConfig, ReviewMode};
use crate::indexer::{IndexPlan, IndexSummary};
use crate::review::{ReviewDecision, ReviewSession};
use crate::store::ReviewStore;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub index_plan: IndexPlan,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let index_plan = IndexPlan::from_config(&config);
        Self { config, index_plan }
    }

    pub fn run_review<S: ReviewStore>(
        &self,
        store: &S,
        mode: ReviewMode,
    ) -> Result<(IndexSummary, Vec<ReviewDecision>), String> {
        let summary = self.index_plan.execute(mode)?;
        let session = ReviewSession::from_summary(&summary);
        let decisions = session.default_decisions();
        store.persist_batch(&decisions)?;
        Ok((summary, decisions))
    }
}

pub fn default_state() -> AppState {
    AppState::new(AppConfig::default())
}
