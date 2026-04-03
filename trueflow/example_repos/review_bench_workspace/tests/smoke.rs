use review_bench_workspace::config::{AppConfig, ReviewMode};
use review_bench_workspace::default_state;
use review_bench_workspace::store::MemoryStore;

#[test]
fn full_mode_uses_larger_batch_size() {
    let config = AppConfig::default();
    assert!(config.effective_batch_size(ReviewMode::Full) > config.effective_batch_size(ReviewMode::Incremental));
}

#[test]
fn state_can_run_a_review() {
    let state = default_state();
    let store = MemoryStore::default();
    let result = state.run_review(&store, ReviewMode::Incremental);
    assert!(result.is_ok(), "expected review run to succeed: {result:?}");
}
