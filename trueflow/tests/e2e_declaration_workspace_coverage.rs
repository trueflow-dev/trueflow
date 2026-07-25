use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::commands::review::ResolvedReviewQuery;
use trueflow::config::BlockFilters;
use trueflow::declaration::Capability;
use trueflow::declaration::capture::capture_declaration_sources;
use trueflow::declaration::diff::diff_declarations;
use trueflow::declaration::review::{
    DeclarationReviewDiffBatch, DeclarationReviewQuery, DeclarationReviewStatus,
    collect_declaration_review,
};
use trueflow::repo_path::RepoPath;
use trueflow::scanner::ScanOptions;
use trueflow::targets::{ReviewContentSource, ReviewDiffSelection, ReviewPathSelection};
use trueflow::vcs::ChangedPath;
use trueflow_test_support::TestRepo;

fn dirty_query(paths: &[&str]) -> Result<ResolvedReviewQuery> {
    let changed = paths
        .iter()
        .map(|path| RepoPath::new(*path).map(ChangedPath::identity))
        .collect::<Result<HashSet<_>>>()?;
    Ok(ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Workdir,
        path_selection: ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: Vec::new(),
            changed: Some(changed),
        },
        diff_selection: ReviewDiffSelection::None,
    })
}

#[test]
fn typical_rust_workspace_survives_capture_diff_and_collection_with_nix_and_just() -> Result<()> {
    const RUST_FILES: [(&str, &str); 8] = [
        (
            "src/lib.rs",
            r#"//! Workspace library.

pub mod model {
    #[derive(Debug, Clone)]
    pub struct Record {
        pub id: u64,
    }

    pub trait Store {
        fn load(&self, id: u64) -> Option<Record>;
    }
}

pub fn open(id: u64) -> model::Record {
    model::Record { id }
}
"#,
        ),
        ("src/main.rs", "fn main() { println!(\"workspace\"); }\n"),
        (
            "src/bin/admin.rs",
            "pub fn run_admin() -> bool { true }\nfn main() { let _ = run_admin(); }\n",
        ),
        (
            "tests/workspace.rs",
            "#[test]\nfn opens_a_record() { assert_eq!(2 + 2, 4); }\n",
        ),
        (
            "examples/client.rs",
            "pub fn example_request() -> &'static str { \"ok\" }\nfn main() {}\n",
        ),
        (
            "benches/throughput.rs",
            "pub fn benchmark_batch_size() -> usize { 128 }\nfn main() {}\n",
        ),
        (
            "build.rs",
            "fn emit_build_metadata() { println!(\"cargo:rerun-if-changed=build.rs\"); }\nfn main() { emit_build_metadata(); }\n",
        ),
        (
            "src/bin/worker.rs",
            "pub(crate) async fn run_worker() -> Result<(), String> { Ok(()) }\nfn main() {}\n",
        ),
    ];
    const NIX: &str = include_str!("../example_repos/nix_support/default.nix");
    const JUST: &str = include_str!("../example_repos/all_languages/main.just");
    const CARGO_TOML: &str = r#"[package]
name = "declaration-workspace"
version = "0.1.0"
edition = "2024"

[dependencies]
"#;

    let repo = TestRepo::new("declaration_workspace_coverage")?;
    repo.write("baseline.txt", "baseline\n")?;
    repo.commit_all("baseline")?;

    for (path, source) in RUST_FILES {
        repo.write(path, source)?;
    }
    repo.write("Cargo.toml", CARGO_TOML)?;
    repo.write("flake.nix", NIX)?;
    repo.write("Justfile", JUST)?;

    let mut changed_paths = RUST_FILES.iter().map(|(path, _)| *path).collect::<Vec<_>>();
    changed_paths.extend(["Cargo.toml", "flake.nix", "Justfile"]);
    let captures = capture_declaration_sources(&repo.path, &dirty_query(&changed_paths)?)?;
    let [capture] = captures.as_slice() else {
        anyhow::bail!("expected one dirty capture batch, got {}", captures.len());
    };
    assert!(
        capture.diagnostics.is_empty(),
        "workspace capture failed: {:?}",
        capture.diagnostics
    );

    let captured = capture
        .pairs
        .iter()
        .filter_map(|pair| pair.head.as_ref())
        .map(|snapshot| {
            (
                snapshot.path.to_string_lossy().into_owned(),
                snapshot.language,
            )
        })
        .collect::<HashSet<_>>();
    for (path, _) in RUST_FILES {
        assert!(
            captured.contains(&(path.to_owned(), Language::Rust)),
            "declaration capture omitted Rust target {path}: {captured:?}"
        );
    }
    assert!(captured.contains(&("flake.nix".to_owned(), Language::Nix)));
    assert!(captured.contains(&("Justfile".to_owned(), Language::Just)));

    let diff = diff_declarations(&capture.pairs)?;
    let collection = collect_declaration_review(&DeclarationReviewQuery::new(vec![
        DeclarationReviewDiffBatch::new(capture.pairs.clone(), diff)
            .with_capability_notices(capture.capability_notices.clone()),
    ]))?;
    assert_eq!(
        collection.status,
        DeclarationReviewStatus::Partial {
            diagnostic_count: collection.diagnostics.len(),
        },
        "useful partial Nix/Just support must not be reported as unsupported"
    );

    let review_paths = collection
        .items
        .iter()
        .map(|item| item.display_path.as_str())
        .collect::<BTreeSet<_>>();
    for (path, _) in RUST_FILES {
        assert!(
            review_paths.contains(path),
            "mixed-language collection dropped Rust declarations from {path}; paths: {review_paths:?}; diagnostics: {:#?}",
            collection.diagnostics
        );
    }

    let declaration_names = collection
        .items
        .iter()
        .map(|item| (item.snapshot.language, item.declaration.name.as_str()))
        .collect::<HashSet<_>>();
    for nix_name in ["defaults", "mkWorker", "selected", "worker"] {
        assert!(
            declaration_names.contains(&(Language::Nix, nix_name)),
            "missing Nix declaration {nix_name}: {declaration_names:?}"
        );
    }
    for recipe in ["default", "build", "loop", "greet", "process"] {
        assert!(
            declaration_names.contains(&(Language::Just, recipe)),
            "missing Just recipe {recipe}: {declaration_names:?}"
        );
    }

    let cargo_manifest = RepoPath::new("Cargo.toml")?;
    let capture_notice = capture
        .capability_notices
        .iter()
        .find(|notice| notice.path == cargo_manifest)
        .context("capture did not audit Cargo.toml as non-applicable")?;
    assert_eq!(capture_notice.language, Language::Toml);
    assert!(matches!(
        capture_notice.inventory,
        Capability::NotApplicable { .. }
    ));
    assert!(
        collection
            .capability_notices
            .iter()
            .any(|notice| notice == capture_notice),
        "Cargo.toml non-applicable capability did not survive collection: {:?}",
        collection.capability_notices
    );
    assert!(
        !collection
            .items
            .iter()
            .any(|item| item.display_path == cargo_manifest),
        "Cargo.toml must never become a declaration review target"
    );
    assert!(
        captured
            .iter()
            .all(|(path, _)| Path::new(path) != Path::new("Cargo.toml")),
        "manifest handling should remain metadata-only rather than loading non-declaration source"
    );
    Ok(())
}
