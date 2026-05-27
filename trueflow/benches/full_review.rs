use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
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

criterion_group!(benches, bench_full_review, bench_deep_nesting);
criterion_main!(benches);
