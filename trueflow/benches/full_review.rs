#[path = "../tests/common/review_bench_support.rs"]
mod review_bench_support;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use review_bench_support::ReviewBenchRepo;
use std::hint::black_box;
use std::time::Duration;

fn bench_full_review(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_review");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    group.bench_function("review_bench_workspace_cold", |b| {
        b.iter_batched(
            || {
                ReviewBenchRepo::fixture("review_bench_workspace").unwrap_or_else(|error| {
                    panic!("failed to prepare cold benchmark fixture: {error}")
                })
            },
            |repo| {
                let summary = repo
                    .full_review_summary()
                    .unwrap_or_else(|error| panic!("cold full review benchmark failed: {error}"));
                black_box((summary.files.len(), summary.total_blocks));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("review_bench_workspace_warm", |b| {
        b.iter_batched(
            || {
                let repo =
                    ReviewBenchRepo::fixture("review_bench_workspace").unwrap_or_else(|error| {
                        panic!("failed to prepare warm benchmark fixture: {error}")
                    });
                repo.full_review_summary()
                    .unwrap_or_else(|error| panic!("failed to warm benchmark fixture: {error}"));
                repo
            },
            |repo| {
                let summary = repo
                    .full_review_summary()
                    .unwrap_or_else(|error| panic!("warm full review benchmark failed: {error}"));
                black_box((summary.files.len(), summary.total_blocks));
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_full_review);
criterion_main!(benches);
