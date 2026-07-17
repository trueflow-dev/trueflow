use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::commands::mark::{StructuredMarkRequest, build_structured_record};
use crate::commands::review::ResolvedReviewQuery;
use crate::declaration::capture::{
    CaptureBatch, CaptureEndpointProvenance, capture_declaration_sources,
};
use crate::declaration::diff::diff_declarations;
use crate::declaration::review::{
    CollectedDeclarationItem, DeclarationReviewDiffBatch, DeclarationReviewQuery,
    DeclarationReviewStatus as CollectionStatus, collect_declaration_review,
};
use crate::declaration::{DeclarationId, DeclarationNode, SourceComponentRole};
use crate::repo_path::RepoPath;
use crate::store::{
    CommentAnchor, DeclarationAnchorRange as RecordAnchorRange, DeclarationCommentAnchor,
    DeclarationRecordLocator, Identity, Record, ReviewCheck, ReviewTargetRef,
    ReviewedDeclarationSnapshot, Verdict,
};

use super::{
    DeclarationController, DeclarationDocument, DeclarationReviewActionKind,
    DeclarationReviewStatus, OutlineRow,
};

/// One collected declaration together with the immutable capture endpoint that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDeclarationTarget {
    pub snapshot_pair_id: crate::declaration::snapshot::SnapshotPairId,
    pub display_path: RepoPath,
    pub snapshot: crate::declaration::snapshot::SourceSnapshot,
    pub declaration: DeclarationNode,
    pub diff_unit: crate::declaration::diff::DeclarationDiffUnit,
    pub latest_verdict: Option<Verdict>,
    provenance: CaptureEndpointProvenance,
}

impl PreparedDeclarationTarget {
    fn from_collected(
        item: CollectedDeclarationItem,
        provenance: CaptureEndpointProvenance,
    ) -> Self {
        Self {
            snapshot_pair_id: item.snapshot_pair_id,
            display_path: item.display_path,
            snapshot: item.snapshot,
            declaration: item.declaration,
            diff_unit: item.diff_unit,
            latest_verdict: item.latest_verdict,
            provenance,
        }
    }
}

/// Fully captured, diffed, and collected declaration launch input.
#[derive(Debug, Clone)]
pub struct PreparedDeclarationLaunch {
    status: CollectionStatus,
    documents: Vec<DeclarationDocument>,
    targets: Vec<PreparedDeclarationTarget>,
    canonical_order: Vec<DeclarationId>,
}

impl PreparedDeclarationLaunch {
    pub fn status(&self) -> &CollectionStatus {
        &self.status
    }

    pub fn documents(&self) -> &[DeclarationDocument] {
        &self.documents
    }

    pub fn targets(&self) -> &[PreparedDeclarationTarget] {
        &self.targets
    }

    pub fn canonical_order(&self) -> &[DeclarationId] {
        &self.canonical_order
    }

    fn document_index_for_target(&self, target: &PreparedDeclarationTarget) -> Option<usize> {
        self.documents.iter().position(|document| {
            document.snapshot_id == target.snapshot.id.as_str()
                && document.path == target.display_path.as_str()
        })
    }

    fn set_status(&mut self, owner_id: &DeclarationId, status: DeclarationReviewStatus) {
        for document in &mut self.documents {
            for row in &mut document.outline_rows {
                if row.review_owner == owner_id.as_str() {
                    row.status = status;
                }
            }
        }
    }
}

/// Atomic persistence seam used by the declaration runtime.
pub trait DeclarationRecordAppender {
    fn append(&mut self, record: &Record) -> Result<()>;
}

/// Capture once, diff every captured batch, collect against raw records, and retain endpoint
/// provenance for each resulting target.
pub fn prepare_declaration_launch(
    repo_root: &Path,
    query: &ResolvedReviewQuery,
    records: Vec<Record>,
) -> Result<PreparedDeclarationLaunch> {
    let captures = capture_declaration_sources(repo_root, query)?;
    let mut review_batches = Vec::with_capacity(captures.len());
    for capture in &captures {
        let diff = diff_declarations(&capture.pairs)?;
        review_batches.push(DeclarationReviewDiffBatch::new(capture.pairs.clone(), diff));
    }

    let collection_query = DeclarationReviewQuery::new(review_batches).with_records(records);
    let collected = collect_declaration_review(&collection_query)?;
    let mut targets = Vec::with_capacity(collected.items.len());
    for item in collected.items {
        let provenance = resolve_target_provenance(&captures, &item)?;
        targets.push(PreparedDeclarationTarget::from_collected(item, provenance));
    }
    let documents = build_documents(&targets)?;

    Ok(PreparedDeclarationLaunch {
        status: collected.status,
        documents,
        targets,
        canonical_order: collected.canonical_order,
    })
}

fn resolve_target_provenance(
    captures: &[CaptureBatch],
    item: &CollectedDeclarationItem,
) -> Result<CaptureEndpointProvenance> {
    let mut matches = Vec::new();
    for capture in captures {
        for pair in &capture.pairs {
            if pair.id != item.snapshot_pair_id {
                continue;
            }
            if pair
                .base
                .as_ref()
                .is_some_and(|snapshot| snapshot.id == item.snapshot.id)
            {
                let base = capture.provenance.base.clone().with_context(|| {
                    format!(
                        "snapshot pair {} selected a base snapshot without base capture provenance",
                        pair.id.as_str()
                    )
                })?;
                matches.push(base);
            }
            if pair
                .head
                .as_ref()
                .is_some_and(|snapshot| snapshot.id == item.snapshot.id)
            {
                matches.push(capture.provenance.head.clone());
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!(
            "missing capture provenance for declaration {} in snapshot pair {}",
            item.declaration.id.as_str(),
            item.snapshot_pair_id.as_str()
        ),
        count => bail!(
            "ambiguous capture provenance ({count} matches) for declaration {} in snapshot pair {}",
            item.declaration.id.as_str(),
            item.snapshot_pair_id.as_str()
        ),
    }
}

fn build_documents(targets: &[PreparedDeclarationTarget]) -> Result<Vec<DeclarationDocument>> {
    let mut grouped: BTreeMap<(RepoPath, String), Vec<&PreparedDeclarationTarget>> = BTreeMap::new();
    for target in targets {
        grouped
            .entry((target.display_path.clone(), target.snapshot.id.as_str().to_owned()))
            .or_default()
            .push(target);
    }

    grouped
        .into_values()
        .map(|mut group| {
            group.sort_by(|left, right| {
                left.declaration
                    .source_span
                    .start
                    .cmp(&right.declaration.source_span.start)
                    .then_with(|| left.declaration.source_ordinal.cmp(&right.declaration.source_ordinal))
                    .then_with(|| left.declaration.id.cmp(&right.declaration.id))
            });
            let first = group.first().context("empty declaration document group")?;
            let snapshot = &first.snapshot;
            let path = first.display_path.as_str().to_owned();
            ensure!(
                group.iter().all(|target| {
                    target.snapshot.id == snapshot.id
                        && target.display_path == first.display_path
                        && target.snapshot.source() == snapshot.source()
                }),
                "declaration document group contains conflicting snapshot sources"
            );

            let mut rows = Vec::with_capacity(group.len());
            let mut canonical_order = Vec::with_capacity(group.len());
            for target in group {
                let declaration = &target.declaration;
                let display_range = display_range(declaration).with_context(|| {
                    format!(
                        "declaration {} has no body-free display component",
                        declaration.id.as_str()
                    )
                })?;
                let mut row = OutlineRow::review_target(
                    declaration.id.as_str(),
                    declaration.source_span.clone(),
                    display_range,
                    declaration
                        .components
                        .iter()
                        .map(|component| component.source_range.clone())
                        .collect(),
                );
                row.status = status_for_verdict(target.latest_verdict.as_ref());
                canonical_order.push(declaration.id.as_str().to_owned());
                rows.push(row);
            }

            let document = DeclarationDocument {
                snapshot_id: snapshot.id.as_str().to_owned(),
                path,
                exact_source: snapshot.source().to_owned(),
                outline_rows: rows,
                canonical_order: canonical_order.clone(),
                relationships: BTreeMap::new(),
                initial_outline_selection: canonical_order.first().cloned(),
                initial_expanded: Default::default(),
                initial_graph_selection: None,
            };
            document.validate()?;
            Ok(document)
        })
        .collect()
}

fn display_range(declaration: &DeclarationNode) -> Option<std::ops::Range<usize>> {
    declaration
        .components
        .iter()
        .find(|component| component.role == SourceComponentRole::Signature)
        .or_else(|| {
            declaration.components.iter().find(|component| {
                matches!(
                    component.role,
                    SourceComponentRole::AggregateShape
                        | SourceComponentRole::TypeAlias
                        | SourceComponentRole::Value
                )
            })
        })
        .or_else(|| {
            declaration.components.iter().find(|component| {
                !matches!(
                    component.role,
                    SourceComponentRole::Documentation
                        | SourceComponentRole::Attribute
                        | SourceComponentRole::Layout
                )
            })
        })
        .or_else(|| declaration.components.first())
        .map(|component| component.source_range.clone())
}

fn status_for_verdict(verdict: Option<&Verdict>) -> DeclarationReviewStatus {
    match verdict {
        Some(Verdict::Approved) => DeclarationReviewStatus::Approved,
        Some(Verdict::Comment) => DeclarationReviewStatus::Commented,
        Some(Verdict::Rejected) => DeclarationReviewStatus::Rejected,
        None => DeclarationReviewStatus::Pending,
    }
}

/// Runtime coordinator that appends a validated V5 record before changing any review state.
pub struct DeclarationAppRuntime<A> {
    prepared: PreparedDeclarationLaunch,
    identity: Identity,
    appender: A,
    current_index: usize,
    controller: Option<DeclarationController>,
    inner_width: u16,
    inner_height: u16,
}

impl<A: DeclarationRecordAppender> DeclarationAppRuntime<A> {
    pub fn new(
        prepared: PreparedDeclarationLaunch,
        identity: Identity,
        appender: A,
        inner_width: u16,
        inner_height: u16,
    ) -> Result<Self> {
        let mut runtime = Self {
            prepared,
            identity,
            appender,
            current_index: 0,
            controller: None,
            inner_width,
            inner_height,
        };
        runtime.rebuild_controller()?;
        Ok(runtime)
    }

    pub fn status(&self) -> &CollectionStatus {
        self.prepared.status()
    }

    pub fn current(&self) -> Option<&PreparedDeclarationTarget> {
        self.prepared.targets.get(self.current_index)
    }

    pub fn is_finished(&self) -> bool {
        self.current().is_none()
    }

    pub fn visible_text(&self) -> String {
        if let Some(controller) = &self.controller {
            return controller.render_model().visible_text;
        }
        collection_status_text(self.prepared.status())
    }

    pub fn submit(
        &mut self,
        action: DeclarationReviewActionKind,
        note: Option<String>,
    ) -> Result<()> {
        let target = self
            .current()
            .cloned()
            .context("declaration review is already finished")?;
        validate_note(action, note.as_deref())?;
        let record = build_record(&target, self.identity.clone(), action, note)?;

        // Atomicity boundary: neither status nor cursor changes until persistence succeeds.
        self.appender.append(&record)?;

        let owner_id = target.declaration.id.clone();
        let status = match action {
            DeclarationReviewActionKind::Approve => DeclarationReviewStatus::Approved,
            DeclarationReviewActionKind::Comment => DeclarationReviewStatus::Commented,
            DeclarationReviewActionKind::Reject => DeclarationReviewStatus::Rejected,
        };
        self.prepared.set_status(&owner_id, status);
        self.current_index += 1;
        self.rebuild_controller()?;
        Ok(())
    }

    pub fn into_appender(self) -> A {
        self.appender
    }

    fn rebuild_controller(&mut self) -> Result<()> {
        let Some(target) = self.current().cloned() else {
            self.controller = None;
            return Ok(());
        };
        let document_index = self
            .prepared
            .document_index_for_target(&target)
            .context("current declaration target has no exact-source document")?;
        let mut document = self.prepared.documents[document_index].clone();
        document.initial_outline_selection = Some(target.declaration.id.as_str().to_owned());
        self.controller = Some(DeclarationController::new(
            document,
            self.inner_width,
            self.inner_height,
        )?);
        Ok(())
    }
}

fn validate_note(action: DeclarationReviewActionKind, note: Option<&str>) -> Result<()> {
    if note.is_some_and(|note| note.trim().is_empty()) {
        bail!("declaration review notes cannot be empty");
    }
    if matches!(
        action,
        DeclarationReviewActionKind::Comment | DeclarationReviewActionKind::Reject
    ) {
        ensure!(note.is_some(), "comment and rejection actions require a non-empty note");
    }
    Ok(())
}

fn build_record(
    target: &PreparedDeclarationTarget,
    identity: Identity,
    action: DeclarationReviewActionKind,
    note: Option<String>,
) -> Result<Record> {
    let reviewed_snapshot = ReviewedDeclarationSnapshot {
        snapshot_id: target.snapshot.id.as_str().to_owned(),
        content_hash: target.snapshot.bytes_hash().clone(),
    };
    let declaration = &target.declaration;
    let locator = DeclarationRecordLocator {
        path: target.display_path.clone(),
        declaration_key: declaration.key.clone(),
        source_ordinal: declaration.source_ordinal,
        source_span: declaration.source_span.clone(),
        reviewed_snapshot: reviewed_snapshot.clone(),
        projection_hash: declaration.projection_hash.clone(),
    };
    let anchor = DeclarationCommentAnchor {
        reviewed_snapshot,
        projection_hash: declaration.projection_hash.clone(),
        source_len_bytes: target.snapshot.source().len(),
        ranges: declaration
            .components
            .iter()
            .map(|component| RecordAnchorRange {
                start_byte: component.source_range.start,
                end_byte: component.source_range.end,
                exact_text: component.text.clone(),
            })
            .collect(),
    };
    anchor.validate_against_source(target.snapshot.source())?;

    let verdict = match action {
        DeclarationReviewActionKind::Approve => Verdict::Approved,
        DeclarationReviewActionKind::Comment => Verdict::Comment,
        DeclarationReviewActionKind::Reject => Verdict::Rejected,
    };
    build_structured_record(StructuredMarkRequest {
        target: ReviewTargetRef::Declaration {
            hash: declaration.projection_hash.clone(),
        },
        check: ReviewCheck::declaration(),
        verdict,
        identity,
        repo_ref: target.provenance.repo_ref.clone(),
        block_state: target.provenance.block_state.clone(),
        note,
        comment_context: None,
        comment_anchor: Some(CommentAnchor::Declaration(anchor)),
        declaration_locator: Some(locator),
    })
}

fn collection_status_text(status: &CollectionStatus) -> String {
    match status {
        CollectionStatus::Ready => "Declaration review complete".to_owned(),
        CollectionStatus::NoSurfaceChanges => "No declaration surface changes".to_owned(),
        CollectionStatus::UnsupportedLanguage { languages } => format!(
            "Unsupported language: {}",
            languages
                .iter()
                .map(|language| format!("{language:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CollectionStatus::FullyReviewed => "All declaration surfaces are already reviewed".to_owned(),
    }
}
