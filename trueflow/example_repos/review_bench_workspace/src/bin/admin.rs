use review_bench_workspace::config::{AppConfig, ReviewMode};
use review_bench_workspace::default_state;
use review_bench_workspace::store::MemoryStore;

fn main() {
    let config = AppConfig::default();
    let state = default_state();
    let store = MemoryStore::default();
    let result = state.run_review(&store, ReviewMode::Full);

    println!("admin review run for {} => {:?}", config.repository_name, result);
}
