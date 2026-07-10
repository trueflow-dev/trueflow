use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use trueflow::analysis::Language;
use trueflow::{block_splitter, complexity};
use trueflow_test_support::ReviewBenchRepo;

fn deeply_nested_rust_function(depth: usize) -> String {
    let mut content = String::from("pub fn nested(mut value: i32) -> i32 {\n");
    for level in 1..=depth {
        content.push_str(&format!("if value >= {level} {{\n"));
    }
    content.push_str("value += 1;\n");
    for _ in 0..depth {
        content.push_str("}\n");
    }
    content.push_str("value\n}\n");
    content
}

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

fn bench_batch_diff_review(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_diff_review");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for file_count in [100_usize, 200, 400] {
        let repo = ReviewBenchRepo::generated_main_diff(
            &format!("batch_diff_review_{file_count}"),
            file_count,
        )
        .unwrap_or_else(|error| panic!("failed to prepare batch diff fixture: {error}"));
        let warm_summary = repo
            .main_diff_review_summary()
            .unwrap_or_else(|error| panic!("failed to warm batch diff fixture: {error}"));
        assert_eq!(warm_summary.files.len(), file_count);
        assert_eq!(warm_summary.total_blocks, file_count);

        group.throughput(Throughput::Elements(file_count as u64));
        group.bench_with_input(
            BenchmarkId::new("main_diff", file_count),
            &repo,
            |b, repo| {
                b.iter(|| {
                    let summary = repo.main_diff_review_summary().unwrap_or_else(|error| {
                        panic!("batch diff review benchmark failed: {error}")
                    });
                    black_box((summary.files.len(), summary.total_blocks));
                });
            },
        );
    }

    group.finish();
}

fn bench_deep_nesting(c: &mut Criterion) {
    let content = deeply_nested_rust_function(4_000);
    let mut group = c.benchmark_group("deep_nesting");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("rust_complexity_4000", |b| {
        b.iter(|| {
            let score = complexity::calculate(black_box(content.as_str()), Language::Rust);
            black_box(score);
        });
    });

    group.bench_function("rust_block_split_4000", |b| {
        b.iter(|| {
            let split = block_splitter::split(black_box(content.as_str()), Language::Rust);
            black_box((split.blocks.len(), split.diagnostics.len()));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_full_review,
    bench_batch_diff_review,
    bench_deep_nesting
);
criterion_main!(benches);
