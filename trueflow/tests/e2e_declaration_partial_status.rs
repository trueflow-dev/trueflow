#![cfg(feature = "tui-test-support")]

use std::collections::HashSet;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::commands::review::ResolvedReviewQuery;
use trueflow::commands::tui::declaration::{
    DeclarationAppRuntime, DeclarationRecordAppender, DeclarationReviewActionKind,
    prepare_declaration_launch,
};
use trueflow::config::BlockFilters;
use trueflow::declaration::capture::capture_declaration_sources;
use trueflow::declaration::diff::{DiffDiagnosticKind, diff_declarations};
use trueflow::declaration::review::DeclarationReviewStatus;
use trueflow::repo_path::RepoPath;
use trueflow::scanner::ScanOptions;
use trueflow::store::{Identity, Record};
use trueflow::targets::{ReviewContentSource, ReviewDiffSelection, ReviewPathSelection};
use trueflow::vcs::ChangedPath;
use trueflow_test_support::TestRepo;

#[derive(Debug, Default)]
struct ValidatingAppender;

impl DeclarationRecordAppender for ValidatingAppender {
    fn append(&mut self, record: &Record) -> Result<()> {
        record.validate()
    }
}

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

fn reviewer_identity() -> Identity {
    Identity::Email {
        email: "reviewer@example.com".to_owned(),
    }
}

#[test]
fn mixed_unsupported_projection_remains_visible_during_and_after_supported_review() -> Result<()> {
    let repo = TestRepo::new("declaration_partial_status")?;
    repo.write("src/lib.rs", "pub fn convert(value: u8) -> u8 { value }\n")?;
    repo.write(
        "src/Converter.java",
        "public final class Converter { public int convert(int value) { return value; } }\n",
    )?;
    repo.commit_all("mixed declaration base")?;
    repo.write(
        "src/lib.rs",
        "pub fn convert(value: u16) -> u16 { value }\n",
    )?;
    repo.write(
        "src/Converter.java",
        "public final class Converter { public long convert(long value) { return value; } }\n",
    )?;

    let query = dirty_query(&["src/lib.rs", "src/Converter.java"])?;
    let captures = capture_declaration_sources(&repo.path, &query)?;
    let [capture] = captures.as_slice() else {
        anyhow::bail!("expected one dirty capture batch, got {}", captures.len());
    };
    assert!(
        capture.diagnostics.is_empty(),
        "fixture capture failed: {:?}",
        capture.diagnostics
    );
    let java_pair = capture
        .pairs
        .iter()
        .find(|pair| {
            pair.head.as_ref().is_some_and(|snapshot| {
                snapshot.path == std::path::Path::new("src/Converter.java")
                    && snapshot.language == Language::Java
            })
        })
        .context("capture omitted the changed Java path")?;
    let rust_pair = capture
        .pairs
        .iter()
        .find(|pair| {
            pair.head.as_ref().is_some_and(|snapshot| {
                snapshot.path == std::path::Path::new("src/lib.rs")
                    && snapshot.language == Language::Rust
            })
        })
        .context("capture omitted the changed Rust path")?;

    let diff = diff_declarations(&capture.pairs)?;
    assert!(
        diff.units.iter().any(|unit| {
            unit.snapshot_pair_id == rust_pair.id
                && unit.review_target
                && unit
                    .head
                    .as_ref()
                    .is_some_and(|node| node.name == "convert")
        }),
        "changed Rust declaration did not become reviewable: {:?}",
        diff.units
    );
    assert!(
        diff.diagnostics.iter().any(|diagnostic| {
            diagnostic.snapshot_pair_id == java_pair.id
                && diagnostic.kind == DiffDiagnosticKind::ProjectionDiagnostic
                && diagnostic.message.contains("Java")
                && diagnostic.message.contains("no declaration projector")
        }),
        "changed src/Converter.java did not retain its Java unsupported reason: {:?}",
        diff.diagnostics
    );

    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    assert_eq!(
        prepared.status(),
        &DeclarationReviewStatus::UnsupportedLanguage {
            languages: vec![Language::Java],
        },
        "mixed launch must not collapse its incomplete status to Ready"
    );
    let [rust_target] = prepared.targets() else {
        anyhow::bail!(
            "unsupported Java must leave exactly one reviewable Rust target, got {:?}",
            prepared.targets()
        );
    };
    assert_eq!(rust_target.display_path, RepoPath::new("src/lib.rs")?);
    assert_eq!(rust_target.snapshot.language, Language::Rust);
    assert_eq!(rust_target.declaration.name, "convert");

    let mut runtime =
        DeclarationAppRuntime::new(prepared, reviewer_identity(), ValidatingAppender, 120, 20)?;
    let before_review = runtime.visible_text();
    assert!(
        before_review.contains("pub fn convert(value: u16) -> u16"),
        "active Rust declaration disappeared from review: {before_review}"
    );

    runtime.submit(DeclarationReviewActionKind::Approve, None)?;
    assert!(runtime.is_finished());
    let after_review = runtime.visible_text();
    assert!(
        after_review.contains("Unsupported language") && after_review.contains("Java"),
        "finishing Rust hid the incomplete Java projection: {after_review}"
    );
    assert!(
        !after_review.contains("Declaration review complete"),
        "incomplete Java work was reported as complete: {after_review}"
    );

    assert!(
        before_review.contains("Unsupported language") && before_review.contains("Java"),
        "the active Rust controller hid the incomplete Java projection status: {before_review}"
    );
    assert!(
        !before_review.contains("Declaration review complete"),
        "incomplete Java work was reported as complete before Rust review: {before_review}"
    );
    Ok(())
}
