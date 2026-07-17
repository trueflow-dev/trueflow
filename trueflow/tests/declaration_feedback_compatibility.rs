use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use trueflow::analysis::Language;
use trueflow::commands::feedback::{feedback_entries_to_json_values, feedback_entries_to_xml};
use trueflow::config::BlockFilters;
use trueflow::declaration::snapshot::{SnapshotId, SourceSnapshot};
use trueflow::declaration::{DeclarationNode, project_source};
use trueflow::feedback_export::{
    FeedbackContextResolver, FeedbackEntry, FeedbackQuery, FeedbackSinceFilter,
    RepoFeedbackContextResolver, ResolvedDeclarationFeedback, ResolvedFeedbackContext,
    collect_feedback_entries,
};
use trueflow::repo_path::RepoPath;
use trueflow::scanner::ScanOptions;
use trueflow::store::{
    BlockState, CommitId, DeclarationRecordLocator, Identity, Record, RepoRef, ReviewCheck,
    ReviewTargetRef, ReviewedDeclarationSnapshot, VcsSystem, Verdict,
};
use trueflow::targets::{ReviewContentSource, ReviewPathSelection};
use trueflow_test_support::FeedbackScenario;

const PATH: &str = "src/lib.rs";
const SOURCE: &str = "pub fn convert(value: u8) -> u8 { value }\n";
const BLOCK_NOTE: &str = "ordinary block feedback";
const DECLARATION_NOTE: &str = "declaration feedback";

type TestResult = Result<()>;

#[derive(Clone)]
enum DeclarationResolution {
    Resolved(ResolvedDeclarationFeedback),
    Unsupported,
    Error,
}

struct MixedResolver<'a> {
    ordinary: RepoFeedbackContextResolver<'a>,
    declaration: DeclarationResolution,
}

impl FeedbackContextResolver for MixedResolver<'_> {
    fn resolve_context(&mut self, record: &Record) -> Result<ResolvedFeedbackContext> {
        self.ordinary.resolve_context(record)
    }

    fn resolve_declaration_context(
        &mut self,
        _record: &Record,
    ) -> Result<Option<ResolvedDeclarationFeedback>> {
        match &self.declaration {
            DeclarationResolution::Resolved(declaration) => Ok(Some(declaration.clone())),
            DeclarationResolution::Unsupported => Ok(None),
            DeclarationResolution::Error => {
                bail!("injected declaration-specific resolution failure")
            }
        }
    }
}

struct MixedFixture {
    scenario: FeedbackScenario,
    records: Vec<Record>,
    declaration: ResolvedDeclarationFeedback,
    block_record_id: String,
    declaration_record_id: String,
}

impl MixedFixture {
    fn new(name: &str) -> Result<Self> {
        let scenario = FeedbackScenario::new(name)?;
        scenario.write(PATH, SOURCE)?;
        let revision = CommitId::new(scenario.commit_all("add mixed feedback source")?)?;

        let mut block_record = scenario.review_block_in_process(PATH, "comment")?;
        block_record.id = "ordinary-block-record".to_string();
        block_record.note = Some(BLOCK_NOTE.to_string());
        block_record.validate()?;

        let (snapshot, projected) = projected_declaration()?;
        let declaration_record = declaration_record(&revision, &snapshot, &projected)?;
        let declaration = ResolvedDeclarationFeedback {
            path: RepoPath::new(PATH)?,
            semantic_key: projected.key.clone(),
            projection_hash: projected.projection_hash.clone(),
            projection_text: projected.projection_text,
            ranges: Vec::new(),
            context: None,
        };

        Ok(Self {
            scenario,
            block_record_id: block_record.id.clone(),
            declaration_record_id: declaration_record.id.clone(),
            records: vec![block_record, declaration_record],
            declaration,
        })
    }

    fn collect(&self, declaration: DeclarationResolution) -> Result<Vec<FeedbackEntry>> {
        let scan_options = ScanOptions::default();
        let mut resolver = MixedResolver {
            ordinary: RepoFeedbackContextResolver::new_for_repo_root(
                &ReviewContentSource::Workdir,
                &scan_options,
                None,
                &self.scenario.repo().path,
            )?,
            declaration,
        };
        collect_feedback_entries(
            &self.records,
            &FeedbackSinceFilter::All,
            &FeedbackQuery {
                filters: BlockFilters::default(),
                explicit_selection: Some(ReviewPathSelection::All),
                changed_selection: None,
                allowed_revisions: None,
                include_approved: true,
            },
            &mut resolver,
        )
    }
}

fn projected_declaration() -> Result<(SourceSnapshot, DeclarationNode)> {
    let snapshot = SourceSnapshot::new(
        SnapshotId::new("feedback-compatibility-source"),
        Path::new(PATH),
        Language::Rust,
        SOURCE,
    );
    let declaration = project_source(Path::new(PATH), Language::Rust, SOURCE)?
        .declarations()
        .iter()
        .find(|declaration| declaration.name == "convert")
        .context("fixture must project convert")?
        .clone();
    Ok((snapshot, declaration))
}

fn declaration_record(
    revision: &CommitId,
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
) -> Result<Record> {
    let reviewed_snapshot = ReviewedDeclarationSnapshot {
        snapshot_id: snapshot.id.as_str().to_string(),
        content_hash: snapshot.bytes_hash().clone(),
    };
    let mut record = Record::new(
        ReviewTargetRef::Declaration {
            hash: declaration.projection_hash.clone(),
        },
        ReviewCheck::declaration(),
        Verdict::Comment,
        Identity::Email {
            email: "reviewer@example.com".to_string(),
        },
        RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: revision.clone(),
        },
        BlockState::Committed,
    );
    record.id = "declaration-record".to_string();
    record.note = Some(DECLARATION_NOTE.to_string());
    record.path_hint = Some(RepoPath::new(PATH)?);
    record.declaration_locator = Some(DeclarationRecordLocator {
        path: RepoPath::new(PATH)?,
        declaration_key: declaration.key.clone(),
        source_ordinal: declaration.source_ordinal,
        source_span: declaration.source_span.clone(),
        reviewed_snapshot,
        projection_hash: declaration.projection_hash.clone(),
    });
    record.validate()?;
    Ok(record)
}

fn json_entry_for_review<'a>(entries: &'a [Value], record_id: &str) -> Result<&'a Value> {
    entries
        .iter()
        .find(|entry| {
            entry["reviews"]
                .as_array()
                .is_some_and(|reviews| reviews.iter().any(|review| review["id"] == record_id))
        })
        .with_context(|| format!("missing feedback entry for review record {record_id}"))
}

fn assert_ordinary_json_entry(entry: &Value) {
    assert_eq!(entry["target"]["kind"], "block");
    assert_eq!(entry["block"]["content"], SOURCE.trim_end());
    assert!(entry.get("declaration").is_none());
    assert!(entry.get("resolution_error").is_none());
}

fn assert_declaration_failure_json_entry(entry: &Value) -> Result<()> {
    assert_eq!(entry["target"]["kind"], "declaration");
    assert!(entry.get("declaration").is_none_or(Value::is_null));
    let error = entry["resolution_error"]
        .as_str()
        .context("unresolved declaration entry must expose a resolution_error")?;
    assert!(
        !error.trim().is_empty(),
        "declaration resolution_error must explain the declaration-specific failure"
    );
    Ok(())
}

fn assert_mixed_failure_output(
    entries: &[FeedbackEntry],
    block_record_id: &str,
    declaration_record_id: &str,
) -> Result<()> {
    let json = feedback_entries_to_json_values(entries);
    assert_eq!(
        json.len(),
        2,
        "mixed feedback must retain the ordinary entry and represent the declaration failure"
    );
    assert_ordinary_json_entry(json_entry_for_review(&json, block_record_id)?);
    assert_declaration_failure_json_entry(json_entry_for_review(&json, declaration_record_id)?)?;

    let xml = feedback_entries_to_xml(entries)?;
    assert!(
        xml.contains("<block ") && xml.contains(BLOCK_NOTE),
        "XML must retain the ordinary block feedback: {xml}"
    );
    assert!(
        xml.contains("<declaration")
            && xml.contains("target_kind=\"declaration\"")
            && xml.contains("resolution_error=\"")
            && xml.contains(DECLARATION_NOTE),
        "XML must contain an explicit declaration-specific failure entry: {xml}"
    );
    Ok(())
}

#[test]
fn valid_mixed_declaration_and_block_feedback_survive_json_and_xml_export() -> TestResult {
    let fixture = MixedFixture::new("feedback_compatibility_valid_mixed")?;
    let entries = fixture.collect(DeclarationResolution::Resolved(fixture.declaration.clone()))?;

    let json = feedback_entries_to_json_values(&entries);
    assert_eq!(json.len(), 2);
    assert_ordinary_json_entry(json_entry_for_review(&json, &fixture.block_record_id)?);
    let declaration = json_entry_for_review(&json, &fixture.declaration_record_id)?;
    assert_eq!(declaration["target"]["kind"], "declaration");
    assert_eq!(
        declaration["target"]["semantic_key"],
        fixture.declaration.semantic_key.as_str()
    );
    assert_eq!(
        declaration["declaration"]["projection_text"],
        fixture.declaration.projection_text
    );

    let xml = feedback_entries_to_xml(&entries)?;
    assert!(
        xml.contains("<block ") && xml.contains(BLOCK_NOTE),
        "XML must retain the ordinary block feedback: {xml}"
    );
    assert!(
        xml.contains("<declaration target_kind=\"declaration\"")
            && xml.contains(fixture.declaration.semantic_key.as_str())
            && xml.contains(DECLARATION_NOTE),
        "XML must retain the resolved declaration feedback: {xml}"
    );
    Ok(())
}

#[test]
fn unsupported_declaration_resolution_is_explicit_without_dropping_block_feedback() -> TestResult {
    let fixture = MixedFixture::new("feedback_compatibility_unsupported_declaration")?;
    let entries = fixture.collect(DeclarationResolution::Unsupported)?;

    assert_mixed_failure_output(
        &entries,
        &fixture.block_record_id,
        &fixture.declaration_record_id,
    )
}

#[test]
fn declaration_resolution_error_is_explicit_without_aborting_block_feedback() -> TestResult {
    let fixture = MixedFixture::new("feedback_compatibility_declaration_error")?;
    let entries = fixture.collect(DeclarationResolution::Error)?;

    assert_mixed_failure_output(
        &entries,
        &fixture.block_record_id,
        &fixture.declaration_record_id,
    )
}
