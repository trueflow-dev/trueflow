#![cfg(feature = "tui-test-support")]

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use async_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Location, Position,
    Range as LspRange, SymbolKind, Url,
};
use crossterm::event::KeyCode;
use trueflow::commands::review::ResolvedReviewQuery;
use trueflow::commands::tui::declaration::{
    DeclarationAppRuntime, DeclarationRecordAppender, DeclarationRelationshipProvider,
    DeclarationRelationshipRequest, DeclarationRelationshipResults, RelationshipBridge,
    RelationshipDestination, RelationshipState as GraphRelationshipState, RelationshipUpdate,
    prepare_declaration_launch,
};
use trueflow::config::BlockFilters;
use trueflow::declaration::relationships::{
    CallHierarchyBundle, DocumentHash, LspServerProfile, ProviderCallHierarchyState, ProviderError,
    RelationshipCapability, RelationshipEdge, RelationshipKind, RelationshipLocation,
    RelationshipMethod, RelationshipOutcome, RelationshipProvenance, RelationshipRequestKey,
    RelationshipResult, RelationshipScope, RelationshipTarget, SourceGeneration, WorkspaceTrust,
};
use trueflow::declaration::{DeclarationId, TypeUseRole};
use trueflow::repo_path::RepoPath;
use trueflow::scanner::ScanOptions;
use trueflow::store::{CommitId, Identity, Record};
use trueflow::targets::{
    CommitRange, ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection,
};
use trueflow::vcs::ChangedPath;
use trueflow_test_support::{TestRepo, run_git_output};

const A_BASE: &str = r#"pub fn alpha(config: &Config) -> u8 {
    config.value
}

pub struct Config {
    pub value: u8,
}
"#;

const A_HEAD: &str = r#"pub fn alpha(config: &Config) -> u16 {
    config.value
}

pub struct Config {
    pub value: u16,
}
"#;

const Z_BASE: &str = r#"pub fn beta(value: u8) -> u8 {
    value
}
"#;

const Z_HEAD: &str = r#"pub fn beta(value: u16) -> u16 {
    value
}
"#;

const DOCUMENTED_A_BASE: &str = r#"/// alpha accepts a byte-sized configuration.
pub fn alpha(config: &Config) -> u8 {
    config.value
}

pub struct Config {
    pub value: u8,
}
"#;

const DOCUMENTED_A_HEAD: &str = r#"/// alpha accepts a widened configuration.
pub fn alpha(config: &Config) -> u16 {
    config.value
}

pub struct Config {
    pub value: u16,
}
"#;

const A_LIVE: &str = r#"pub fn alpha(config: &Config) -> u32 {
    config.value
}

pub struct Config {
    pub value: u32,
}
"#;

#[derive(Debug, Default)]
struct RecordingAppender;

impl DeclarationRecordAppender for RecordingAppender {
    fn append(&mut self, record: &Record) -> Result<()> {
        record.validate()
    }
}

fn identity() -> Identity {
    Identity::Email {
        email: "reviewer@example.com".to_owned(),
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

fn historical_query(base: CommitId, head: CommitId) -> ResolvedReviewQuery {
    ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Revision(head.clone()),
        path_selection: ReviewPathSelection::All,
        diff_selection: ReviewDiffSelection::Targets(vec![ReviewDiffTarget::RevisionRange(
            CommitRange { start: base, end: head },
        )]),
    }
}

fn commit_id(repo: &TestRepo, revision: &str) -> Result<CommitId> {
    CommitId::new(run_git_output(&repo.path, &["rev-parse", revision])?)
}

struct ReviewFixture {
    repo: TestRepo,
    prepared: trueflow::commands::tui::declaration::PreparedDeclarationLaunch,
    alpha: DeclarationId,
    config: DeclarationId,
    beta: DeclarationId,
    a_uri: Url,
    z_uri: Url,
}

impl ReviewFixture {
    fn new(name: &str) -> Result<Self> {
        Self::with_sources(name, A_BASE, A_HEAD, Z_BASE, Z_HEAD)
    }

    fn with_sources(
        name: &str,
        a_base: &str,
        a_head: &str,
        z_base: &str,
        z_head: &str,
    ) -> Result<Self> {
        let repo = TestRepo::new(name)?;
        repo.write("src/a.rs", a_base)?;
        repo.write("src/z.rs", z_base)?;
        repo.commit_all("base declaration surfaces")?;
        repo.write("src/a.rs", a_head)?;
        repo.write("src/z.rs", z_head)?;
        let prepared = prepare_declaration_launch(
            &repo.path,
            &dirty_query(&["src/a.rs", "src/z.rs"])?,
            Vec::new(),
        )?;
        let declaration_id = |name: &str| {
            prepared
                .targets()
                .iter()
                .find(|target| target.declaration.name == name)
                .map(|target| target.declaration.id.clone())
                .with_context(|| format!("missing prepared {name} declaration"))
        };
        let alpha = declaration_id("alpha")?;
        let config = declaration_id("Config")?;
        let beta = declaration_id("beta")?;
        assert_eq!(
            prepared.canonical_order(),
            [alpha.clone(), config.clone(), beta.clone()],
            "fixture must put Config, not the graph's beta edge, after alpha"
        );
        let a_uri = Url::from_file_path(repo.path.join("src/a.rs"))
            .map_err(|()| anyhow::anyhow!("cannot make src/a.rs URI"))?;
        let z_uri = Url::from_file_path(repo.path.join("src/z.rs"))
            .map_err(|()| anyhow::anyhow!("cannot make src/z.rs URI"))?;
        Ok(Self {
            repo,
            prepared,
            alpha,
            config,
            beta,
            a_uri,
            z_uri,
        })
    }

    fn runtime(&self) -> Result<DeclarationAppRuntime<RecordingAppender>> {
        DeclarationAppRuntime::new(
            self.prepared.clone(),
            identity(),
            RecordingAppender,
            120,
            18,
        )
    }

    fn full_script(&self) -> Script {
        Script::Full(Box::new(FullScript {
            config: self.config.clone(),
            beta: self.beta.clone(),
            a_uri: self.a_uri.clone(),
            z_uri: self.z_uri.clone(),
        }))
    }
}

#[derive(Debug, Clone, Copy)]
enum StaleDimension {
    Generation,
    Snapshot,
    Declaration,
}

#[derive(Debug)]
struct FullScript {
    config: DeclarationId,
    beta: DeclarationId,
    a_uri: Url,
    z_uri: Url,
}

#[derive(Debug)]
enum Script {
    SupportedEmpty,
    Unsupported,
    Partial,
    ProviderFailure,
    Full(Box<FullScript>),
    StaleThenCurrent(StaleDimension),
    NeverCompletes,
    SelfCallAt(LspRange),
    ReadyUnresolved,
}

#[derive(Debug)]
struct FakeProvider {
    scripts: VecDeque<Script>,
    completions: VecDeque<DeclarationRelationshipResults>,
    expected_root: std::path::PathBuf,
}

impl FakeProvider {
    fn new(expected_root: &Path, scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            scripts: scripts.into_iter().collect(),
            completions: VecDeque::new(),
            expected_root: expected_root.to_path_buf(),
        }
    }
}

impl DeclarationRelationshipProvider for FakeProvider {
    fn request(
        &mut self,
        request: DeclarationRelationshipRequest,
    ) -> std::result::Result<(), ProviderError> {
        assert_eq!(request.workspace_root, self.expected_root);
        assert_eq!(request.key.server_profile, LspServerProfile::RustAnalyzer);
        assert_eq!(request.key.declaration_id, request.target.declaration.id);
        assert_eq!(request.key.declaration_key, request.target.declaration.key);
        assert_eq!(request.snapshot_id, request.target.snapshot.id);
        assert_eq!(request.document.snapshot_id, request.snapshot_id.as_str());
        assert_eq!(request.document.path, request.target.display_path.as_str());
        assert_eq!(
            request.document.exact_source,
            request.target.snapshot.source()
        );
        assert_eq!(
            request.key.document_hash,
            DocumentHash::from_bytes(request.document.exact_source.as_bytes()),
            "background work must be bound to the exact prepared document bytes"
        );

        let script = self
            .scripts
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected relationship request: {request:#?}"));
        match script {
            Script::ProviderFailure => {
                return Err(ProviderError::ExecutableNotFound(
                    "injected-rust-analyzer".to_owned(),
                ));
            }
            Script::NeverCompletes => {}
            Script::SelfCallAt(selection_range) => {
                let item = call_item(
                    &request.target.declaration.name,
                    request.document.uri.clone(),
                    selection_range,
                    selection_range,
                );
                self.completions.push_back(results(
                    &request,
                    ProviderCallHierarchyState::Ready(CallHierarchyBundle {
                        key: request.key.clone(),
                        prepared: vec![item.clone()],
                        incoming: vec![CallHierarchyIncomingCall {
                            from: item,
                            from_ranges: Vec::new(),
                        }],
                        outgoing: Vec::new(),
                    }),
                    RelationshipOutcome::Complete { edges: Vec::new() },
                    RelationshipOutcome::Complete { edges: Vec::new() },
                ));
            }
            Script::ReadyUnresolved => {
                let edge = RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: request.key.declaration_id.clone(),
                    target: RelationshipTarget::Unresolved {
                        name: "CapturedOnlyType".to_owned(),
                    },
                    locations: vec![type_use_location(
                        request.document.uri.clone(),
                        LspRange::new(Position::new(0, 0), Position::new(0, 1)),
                        None,
                        RelationshipMethod::TypeDefinition,
                        TypeUseRole::Other,
                    )],
                };
                self.completions.push_back(results(
                    &request,
                    ProviderCallHierarchyState::Ready(empty_call_bundle(&request.key)),
                    RelationshipOutcome::Complete { edges: vec![edge] },
                    RelationshipOutcome::Complete { edges: Vec::new() },
                ));
            }
            Script::SupportedEmpty => self.completions.push_back(results(
                &request,
                ProviderCallHierarchyState::Ready(empty_call_bundle(&request.key)),
                RelationshipOutcome::Complete { edges: Vec::new() },
                RelationshipOutcome::Complete { edges: Vec::new() },
            )),
            Script::Unsupported => self.completions.push_back(results(
                &request,
                ProviderCallHierarchyState::Unsupported {
                    key: request.key.clone(),
                    capability: RelationshipCapability::PrepareCallHierarchy,
                },
                RelationshipOutcome::Unsupported {
                    capability: RelationshipCapability::TypeDefinition,
                },
                RelationshipOutcome::Unsupported {
                    capability: RelationshipCapability::References,
                },
            )),
            Script::Partial => {
                let edge = RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: request.key.declaration_id.clone(),
                    target: RelationshipTarget::Unresolved {
                        name: "MissingConfig".to_owned(),
                    },
                    locations: vec![type_use_location(
                        request.key.document_uri.clone(),
                        LspRange::new(Position::new(0, 21), Position::new(0, 27)),
                        None,
                        RelationshipMethod::TypeDefinition,
                        TypeUseRole::Parameter,
                    )],
                };
                self.completions.push_back(results(
                    &request,
                    ProviderCallHierarchyState::Partial {
                        bundle: empty_call_bundle(&request.key),
                        diagnostics: vec!["incoming calls exceeded the provider bound".to_owned()],
                    },
                    RelationshipOutcome::Partial {
                        edges: vec![edge],
                        diagnostics: vec!["one type location could not be resolved".to_owned()],
                    },
                    RelationshipOutcome::Complete { edges: Vec::new() },
                ));
            }
            Script::Full(full) => self.completions.push_back(full_results(
                &request,
                &full.config,
                &full.beta,
                &full.a_uri,
                &full.z_uri,
            )?),
            Script::StaleThenCurrent(dimension) => {
                let current = results(
                    &request,
                    ProviderCallHierarchyState::Ready(empty_call_bundle(&request.key)),
                    RelationshipOutcome::Complete { edges: Vec::new() },
                    RelationshipOutcome::Complete { edges: Vec::new() },
                );
                let mut stale = current.clone();
                match dimension {
                    StaleDimension::Generation => {
                        rewrite_result_keys(&mut stale, |key| {
                            key.source_generation = SourceGeneration::new(
                                key.source_generation.get().saturating_sub(1),
                            );
                        });
                    }
                    StaleDimension::Snapshot => {
                        stale.snapshot_id =
                            trueflow::declaration::snapshot::SnapshotId::new("stale-snapshot");
                    }
                    StaleDimension::Declaration => {
                        rewrite_result_keys(&mut stale, |key| {
                            key.declaration_id = DeclarationId::new("stale-declaration");
                        });
                    }
                }
                self.completions.push_back(stale);
                self.completions.push_back(current);
            }
        }
        Ok(())
    }

    fn poll(&mut self) -> Option<DeclarationRelationshipResults> {
        self.completions.pop_front()
    }

    fn shutdown(&mut self) -> std::result::Result<(), ProviderError> {
        self.completions.clear();
        Ok(())
    }
}

#[derive(Debug, Default)]
struct PanicOnRequestProvider;

impl DeclarationRelationshipProvider for PanicOnRequestProvider {
    fn request(
        &mut self,
        request: DeclarationRelationshipRequest,
    ) -> std::result::Result<(), ProviderError> {
        panic!("provider was launched without explicit trusted configuration: {request:#?}")
    }

    fn poll(&mut self) -> Option<DeclarationRelationshipResults> {
        None
    }

    fn shutdown(&mut self) -> std::result::Result<(), ProviderError> {
        Ok(())
    }
}

fn empty_call_bundle(key: &RelationshipRequestKey) -> CallHierarchyBundle {
    CallHierarchyBundle {
        key: key.clone(),
        prepared: Vec::new(),
        incoming: Vec::new(),
        outgoing: Vec::new(),
    }
}

fn results(
    request: &DeclarationRelationshipRequest,
    call_hierarchy: ProviderCallHierarchyState,
    uses_types: RelationshipOutcome,
    used_by: RelationshipOutcome,
) -> DeclarationRelationshipResults {
    DeclarationRelationshipResults {
        snapshot_id: request.snapshot_id.clone(),
        call_hierarchy,
        uses_types: RelationshipResult {
            key: request.key.clone(),
            outcome: uses_types,
        },
        used_by: RelationshipResult {
            key: request.key.clone(),
            outcome: used_by,
        },
    }
}

fn rewrite_result_keys(
    results: &mut DeclarationRelationshipResults,
    mut rewrite: impl FnMut(&mut RelationshipRequestKey),
) {
    match &mut results.call_hierarchy {
        ProviderCallHierarchyState::Ready(bundle)
        | ProviderCallHierarchyState::Partial { bundle, .. } => rewrite(&mut bundle.key),
        ProviderCallHierarchyState::Unsupported { key, .. }
        | ProviderCallHierarchyState::Failed { key, .. } => rewrite(key),
        ProviderCallHierarchyState::Stale { received, .. } => rewrite(received),
    }
    rewrite(&mut results.uses_types.key);
    rewrite(&mut results.used_by.key);
}

fn call_item(
    name: &str,
    uri: Url,
    range: LspRange,
    selection_range: LspRange,
) -> CallHierarchyItem {
    CallHierarchyItem {
        name: name.to_owned(),
        kind: SymbolKind::FUNCTION,
        tags: None,
        detail: Some(format!("fn {name}")),
        uri,
        range,
        selection_range,
        data: None,
    }
}

fn type_use_location(
    origin_uri: Url,
    origin_range: LspRange,
    target: Option<Location>,
    method: RelationshipMethod,
    role: TypeUseRole,
) -> RelationshipLocation {
    RelationshipLocation {
        origin: Location::new(origin_uri, origin_range),
        target,
        provenance: RelationshipProvenance::TypeUse {
            method,
            role,
            scope: RelationshipScope::ProjectedSubset,
        },
    }
}

fn full_results(
    request: &DeclarationRelationshipRequest,
    config: &DeclarationId,
    beta: &DeclarationId,
    a_uri: &Url,
    z_uri: &Url,
) -> std::result::Result<DeclarationRelationshipResults, ProviderError> {
    let beta_item = call_item(
        "beta",
        z_uri.clone(),
        LspRange::new(Position::new(0, 0), Position::new(2, 1)),
        LspRange::new(Position::new(0, 7), Position::new(0, 11)),
    );
    let same_name_wrong_range = call_item(
        "beta",
        z_uri.clone(),
        LspRange::new(Position::new(0, 0), Position::new(0, 3)),
        LspRange::new(Position::new(0, 0), Position::new(0, 3)),
    );
    let registry_uri = Url::parse("file:///registry/types/index.d.ts")
        .map_err(|error| ProviderError::InvalidDocument(error.to_string()))?;
    let external_item = call_item(
        "registry_start",
        registry_uri.clone(),
        LspRange::new(Position::new(8, 0), Position::new(10, 1)),
        LspRange::new(Position::new(8, 9), Position::new(8, 23)),
    );
    let call_hierarchy = ProviderCallHierarchyState::Ready(CallHierarchyBundle {
        key: request.key.clone(),
        prepared: vec![call_item(
            "alpha",
            a_uri.clone(),
            LspRange::new(Position::new(0, 0), Position::new(2, 1)),
            LspRange::new(Position::new(0, 7), Position::new(0, 12)),
        )],
        incoming: vec![
            CallHierarchyIncomingCall {
                from: beta_item.clone(),
                from_ranges: vec![LspRange::new(Position::new(1, 4), Position::new(1, 9))],
            },
            CallHierarchyIncomingCall {
                from: same_name_wrong_range,
                from_ranges: vec![LspRange::new(Position::new(0, 0), Position::new(0, 3))],
            },
        ],
        outgoing: vec![
            CallHierarchyOutgoingCall {
                to: external_item,
                from_ranges: vec![LspRange::new(Position::new(1, 4), Position::new(1, 18))],
            },
            CallHierarchyOutgoingCall {
                to: beta_item,
                from_ranges: vec![LspRange::new(Position::new(1, 4), Position::new(1, 8))],
            },
        ],
    });
    let uses_types = RelationshipOutcome::Complete {
        edges: vec![
            RelationshipEdge {
                kind: RelationshipKind::UsesType,
                source: request.key.declaration_id.clone(),
                target: RelationshipTarget::InReview(config.clone()),
                locations: vec![type_use_location(
                    a_uri.clone(),
                    LspRange::new(Position::new(0, 21), Position::new(0, 27)),
                    Some(Location::new(
                        a_uri.clone(),
                        LspRange::new(Position::new(4, 11), Position::new(4, 17)),
                    )),
                    RelationshipMethod::Definition,
                    TypeUseRole::Parameter,
                )],
            },
            RelationshipEdge {
                kind: RelationshipKind::UsesType,
                source: request.key.declaration_id.clone(),
                target: RelationshipTarget::External {
                    uri: registry_uri.clone(),
                    range: LspRange::new(Position::new(4, 2), Position::new(4, 20)),
                },
                locations: vec![type_use_location(
                    a_uri.clone(),
                    LspRange::new(Position::new(0, 32), Position::new(0, 35)),
                    Some(Location::new(
                        registry_uri,
                        LspRange::new(Position::new(4, 2), Position::new(4, 20)),
                    )),
                    RelationshipMethod::TypeDefinition,
                    TypeUseRole::Return,
                )],
            },
            RelationshipEdge {
                kind: RelationshipKind::UsesType,
                source: request.key.declaration_id.clone(),
                target: RelationshipTarget::Unresolved {
                    name: "MissingConfig".to_owned(),
                },
                locations: vec![type_use_location(
                    a_uri.clone(),
                    LspRange::new(Position::new(0, 21), Position::new(0, 27)),
                    None,
                    RelationshipMethod::TypeDefinition,
                    TypeUseRole::Parameter,
                )],
            },
        ],
    };
    let used_by = RelationshipOutcome::Complete {
        edges: vec![RelationshipEdge {
            kind: RelationshipKind::UsedBy,
            source: request.key.declaration_id.clone(),
            target: RelationshipTarget::InReview(beta.clone()),
            locations: vec![type_use_location(
                z_uri.clone(),
                LspRange::new(Position::new(0, 14), Position::new(0, 17)),
                Some(Location::new(
                    z_uri.clone(),
                    LspRange::new(Position::new(0, 7), Position::new(0, 11)),
                )),
                RelationshipMethod::References,
                TypeUseRole::Parameter,
            )],
        }],
    };
    Ok(results(request, call_hierarchy, uses_types, used_by))
}

fn apply(
    bridge: &mut RelationshipBridge<impl DeclarationRelationshipProvider>,
    runtime: &mut DeclarationAppRuntime<RecordingAppender>,
    update: RelationshipUpdate,
) -> Result<bool> {
    bridge.apply(runtime, update)
}

fn assert_unavailable(update: &RelationshipUpdate, expected_reason: &str) {
    match update.state() {
        GraphRelationshipState::Unavailable { reason } => assert!(
            reason.contains(expected_reason),
            "expected unavailable reason containing {expected_reason:?}, got {reason:?}"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn construction_is_lazy_and_untrusted_or_unconfigured_requests_never_reach_the_provider()
-> Result<()> {
    let fixture = ReviewFixture::new("relationship_bridge_trust")?;

    let lazy = RelationshipBridge::new(
        &fixture.prepared,
        &fixture.repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        PanicOnRequestProvider,
    )?;
    drop(lazy);

    let cases = [
        (
            "untrusted",
            WorkspaceTrust::Untrusted,
            Some(LspServerProfile::RustAnalyzer),
            "not trusted",
        ),
        (
            "unconfigured",
            WorkspaceTrust::TrustedForInvocation,
            None,
            "not configured",
        ),
    ];
    for (case, trust, profile, reason) in cases {
        let mut bridge = RelationshipBridge::new(
            &fixture.prepared,
            &fixture.repo.path,
            trust,
            profile,
            PanicOnRequestProvider,
        )?;
        let update = bridge.request(&fixture.alpha)?;
        assert_eq!(update.declaration_id(), &fixture.alpha, "{case}");
        assert_unavailable(&update, reason);
        assert!(bridge.poll().is_none(), "{case} queued background work");
    }
    Ok(())
}

#[test]
fn loading_empty_unsupported_partial_and_provider_failure_reach_distinct_live_graph_states()
-> Result<()> {
    let cases = [
        (
            "supported empty",
            Script::SupportedEmpty,
            "No relationships found",
        ),
        ("unsupported", Script::Unsupported, "Unavailable —"),
        ("partial", Script::Partial, "Partial —"),
        ("provider failure", Script::ProviderFailure, "Unavailable —"),
    ];

    for (case, script, completed_status) in cases {
        let fixture = ReviewFixture::new(&format!("relationship_bridge_{case}"))?;
        let provider = FakeProvider::new(&fixture.repo.path, [script]);
        let mut bridge = RelationshipBridge::new(
            &fixture.prepared,
            &fixture.repo.path,
            WorkspaceTrust::TrustedForInvocation,
            Some(LspServerProfile::RustAnalyzer),
            provider,
        )?;
        let mut runtime = fixture.runtime()?;
        let before = runtime
            .current()
            .context("missing current declaration")?
            .declaration
            .id
            .clone();

        let loading = bridge.request(&fixture.alpha)?;
        if case == "provider failure" {
            assert_unavailable(&loading, "injected-rust-analyzer");
            assert!(apply(&mut bridge, &mut runtime, loading)?);
        } else {
            assert_eq!(loading.state(), &GraphRelationshipState::Checking, "{case}");
            assert!(apply(&mut bridge, &mut runtime, loading)?);
            assert!(runtime.visible_text().contains("Checking…"), "{case}");

            let completed = bridge
                .poll()
                .with_context(|| format!("missing {case} completion"))?;
            match (case, completed.state()) {
                ("supported empty", GraphRelationshipState::NoRelationships) => {}
                ("unsupported", GraphRelationshipState::Unavailable { reason }) => {
                    assert!(reason.contains("unsupported"), "{reason}");
                }
                ("partial", GraphRelationshipState::Partial { reason, groups }) => {
                    assert!(reason.contains("provider bound"), "{reason}");
                    assert!(reason.contains("could not be resolved"), "{reason}");
                    assert_eq!(groups.len(), 1);
                    assert_eq!(groups[0].label, "Uses types");
                    assert_eq!(groups[0].relationships.len(), 1);
                }
                (_, other) => bail!("{case} completed as {other:?}"),
            }
            assert!(apply(&mut bridge, &mut runtime, completed)?, "{case}");
        }

        assert!(
            runtime.visible_text().contains(completed_status),
            "{case} did not reach the live graph pane: {}",
            runtime.visible_text()
        );
        assert_eq!(
            runtime.current().map(|target| &target.declaration.id),
            Some(&before),
            "{case} relationship state moved the canonical review cursor"
        );
    }
    Ok(())
}

#[test]
fn provider_results_map_exact_targets_into_stable_advisory_groups_without_reordering_review()
-> Result<()> {
    let fixture = ReviewFixture::new("relationship_bridge_mapping")?;
    let provider = FakeProvider::new(&fixture.repo.path, [fixture.full_script()]);
    let mut bridge = RelationshipBridge::new(
        &fixture.prepared,
        &fixture.repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        provider,
    )?;
    let mut runtime = fixture.runtime()?;
    let loading = bridge.request(&fixture.alpha)?;
    assert!(apply(&mut bridge, &mut runtime, loading)?);
    let completed = bridge
        .poll()
        .context("missing full relationship completion")?;

    let groups = match completed.state() {
        GraphRelationshipState::Ready(groups) => groups,
        other => bail!("full provider results did not become Ready: {other:?}"),
    };
    assert_eq!(
        groups
            .iter()
            .map(|group| group.label.as_str())
            .collect::<Vec<_>>(),
        ["Called by", "Calls", "Uses types", "Used by"],
        "semantic groups must have one stable order independent of canonical review order"
    );

    let called_by = &groups[0].relationships;
    assert!(called_by.iter().any(|relationship| {
        relationship.destination
            == RelationshipDestination::InReview {
                declaration_id: fixture.beta.as_str().to_owned(),
            }
    }));
    let same_uri_wrong_range = called_by
        .iter()
        .find_map(|relationship| match &relationship.destination {
            RelationshipDestination::External { context }
                if context.contains(fixture.z_uri.as_str()) =>
            {
                Some(context)
            }
            _ => None,
        })
        .context("same URI/name with a nonmatching range was not retained as external")?;
    assert!(
        same_uri_wrong_range.contains("1:1-1:4"),
        "external context lost the exact nonmatching range: {same_uri_wrong_range}"
    );

    let calls = &groups[1].relationships;
    assert!(calls.iter().any(|relationship| {
        matches!(
            &relationship.destination,
            RelationshipDestination::External { context }
                if context.contains("file:///registry/types/index.d.ts")
        )
    }));
    assert!(calls.iter().any(|relationship| {
        relationship.destination
            == RelationshipDestination::InReview {
                declaration_id: fixture.beta.as_str().to_owned(),
            }
    }));

    let uses_types = &groups[2].relationships;
    assert!(uses_types.iter().any(|relationship| {
        relationship.destination
            == RelationshipDestination::InReview {
                declaration_id: fixture.config.as_str().to_owned(),
            }
    }));
    let external_type = uses_types
        .iter()
        .find(|relationship| {
            matches!(
                relationship.destination,
                RelationshipDestination::External { .. }
            )
        })
        .context("missing external type use")?;
    let RelationshipDestination::External {
        context: external_context,
    } = &external_type.destination
    else {
        bail!("selected type-use relationship was not external");
    };
    for retained in [
        "file:///registry/types/index.d.ts",
        "5:3-5:21",
        "type definition",
        "return",
        "projected subset",
    ] {
        assert!(
            external_context.to_ascii_lowercase().contains(retained),
            "external type relationship lost {retained:?} provenance: {external_context}"
        );
    }
    let unresolved = uses_types
        .iter()
        .find(|relationship| relationship.destination == RelationshipDestination::Unresolved)
        .context("missing unresolved type use")?;
    assert!(unresolved.label.contains("MissingConfig"));
    assert!(unresolved.label.to_ascii_lowercase().contains("parameter"));

    let used_by = &groups[3].relationships;
    assert_eq!(used_by.len(), 1);
    assert_eq!(
        used_by[0].destination,
        RelationshipDestination::InReview {
            declaration_id: fixture.beta.as_str().to_owned(),
        }
    );

    assert!(apply(&mut bridge, &mut runtime, completed)?);
    assert_eq!(
        runtime.current().map(|target| &target.declaration.id),
        Some(&fixture.alpha),
        "graph application changed the runtime's canonical cursor"
    );
    let controller = runtime
        .controller_mut()
        .context("missing declaration controller")?;
    controller.handle_key(KeyCode::Tab)?;
    controller.handle_key(KeyCode::Down)?;
    controller.handle_key(KeyCode::Char('a'))?;
    controller.handle_key(KeyCode::Char('c'))?;
    assert!(
        controller.take_actions().is_empty(),
        "relationship rows became review actions"
    );
    assert!(
        controller.state_snapshot().comment_draft.is_none(),
        "relationship rows opened an exact-source comment editor"
    );

    runtime.skip_current()?;
    assert_eq!(
        runtime.current().map(|target| &target.declaration.id),
        Some(&fixture.config),
        "graph order replaced alpha's canonical successor with beta"
    );
    Ok(())
}

#[test]
fn stale_generation_snapshot_or_declaration_completions_never_mutate_the_live_pane() -> Result<()> {
    for dimension in [
        StaleDimension::Generation,
        StaleDimension::Snapshot,
        StaleDimension::Declaration,
    ] {
        let fixture = ReviewFixture::new(&format!("relationship_bridge_stale_{dimension:?}"))?;
        let provider = FakeProvider::new(&fixture.repo.path, [Script::StaleThenCurrent(dimension)]);
        let mut bridge = RelationshipBridge::new(
            &fixture.prepared,
            &fixture.repo.path,
            WorkspaceTrust::TrustedForInvocation,
            Some(LspServerProfile::RustAnalyzer),
            provider,
        )?;
        let mut runtime = fixture.runtime()?;
        let loading = bridge.request(&fixture.alpha)?;
        let generation = loading.source_generation();
        assert!(apply(&mut bridge, &mut runtime, loading)?);
        let visible_loading = runtime.visible_text();
        assert!(visible_loading.contains("Checking…"));

        assert!(
            bridge.poll().is_none(),
            "stale {dimension:?} completion escaped the bridge"
        );
        assert_eq!(
            runtime.visible_text(),
            visible_loading,
            "stale {dimension:?} completion mutated the live pane"
        );

        let current = bridge
            .poll()
            .context("missing current completion after stale reply")?;
        assert_eq!(current.declaration_id(), &fixture.alpha);
        assert_eq!(current.source_generation(), generation);
        assert_eq!(current.state(), &GraphRelationshipState::NoRelationships);
        assert!(apply(&mut bridge, &mut runtime, current)?);
        assert!(runtime.visible_text().contains("No relationships found"));
    }
    Ok(())
}

#[test]
fn shutdown_discards_in_flight_work_instead_of_applying_it_late() -> Result<()> {
    let fixture = ReviewFixture::new("relationship_bridge_shutdown")?;
    let provider = FakeProvider::new(
        &fixture.repo.path,
        [Script::NeverCompletes, Script::SupportedEmpty],
    );
    let mut bridge = RelationshipBridge::new(
        &fixture.prepared,
        &fixture.repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        provider,
    )?;
    let first = bridge.request(&fixture.alpha)?;
    bridge.shutdown()?;
    assert!(
        bridge.poll().is_none(),
        "shutdown exposed an in-flight completion"
    );

    let rejected = bridge.request(&fixture.alpha);
    ensure!(
        rejected.is_err(),
        "a shut down bridge accepted new background work"
    );
    assert_ne!(
        first.state(),
        &GraphRelationshipState::NoRelationships,
        "fixture accidentally completed synchronously"
    );
    Ok(())
}

#[test]
fn documented_declaration_reconciles_the_projected_identifier_instead_of_its_prose_name()
-> Result<()> {
    let fixture = ReviewFixture::with_sources(
        "relationship_bridge_documented_identifier",
        DOCUMENTED_A_BASE,
        DOCUMENTED_A_HEAD,
        Z_BASE,
        Z_HEAD,
    )?;
    let projected_identifier =
        LspRange::new(Position::new(1, 7), Position::new(1, 12));
    let provider = FakeProvider::new(
        &fixture.repo.path,
        [Script::SelfCallAt(projected_identifier)],
    );
    let mut bridge = RelationshipBridge::new(
        &fixture.prepared,
        &fixture.repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        provider,
    )?;

    let checking = bridge.request(&fixture.alpha)?;
    assert_eq!(checking.state(), &GraphRelationshipState::Checking);
    let completed = bridge
        .poll()
        .context("missing documented declaration relationship completion")?;
    let groups = match completed.state() {
        GraphRelationshipState::Ready(groups) => groups,
        other => bail!("documented declaration did not produce relationships: {other:?}"),
    };
    let caller = groups
        .iter()
        .find(|group| group.label == "Called by")
        .and_then(|group| group.relationships.first())
        .context("missing projected self-call")?;
    assert_eq!(
        caller.destination,
        RelationshipDestination::InReview {
            declaration_id: fixture.alpha.as_str().to_owned(),
        },
        "the identifier range on the declaration line must reconcile to the review target even when prose names it first"
    );
    Ok(())
}

#[test]
fn historical_snapshot_cannot_become_ready_from_a_different_live_workspace_generation()
-> Result<()> {
    let repo = TestRepo::new("relationship_bridge_historical_live_generation")?;
    repo.write("src/a.rs", A_BASE)?;
    repo.commit_all("historical base")?;
    let base = commit_id(&repo, "HEAD")?;
    repo.write("src/a.rs", A_HEAD)?;
    repo.commit_all("historical reviewed head")?;
    let head = commit_id(&repo, "HEAD")?;
    repo.write("src/a.rs", A_LIVE)?;

    let prepared = prepare_declaration_launch(
        &repo.path,
        &historical_query(base, head),
        Vec::new(),
    )?;
    let alpha = prepared
        .targets()
        .iter()
        .find(|target| target.declaration.name == "alpha")
        .map(|target| target.declaration.id.clone())
        .context("missing historical alpha declaration")?;
    let provider = FakeProvider::new(&repo.path, [Script::ReadyUnresolved]);
    let mut bridge = RelationshipBridge::new(
        &prepared,
        &repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        provider,
    )?;

    let initial = bridge.request(&alpha)?;
    if !matches!(initial.state(), GraphRelationshipState::Unavailable { .. }) {
        let completed = bridge
            .poll()
            .context("historical request remained Checking without a terminal state")?;
        assert!(
            matches!(completed.state(), GraphRelationshipState::Unavailable { .. }),
            "captured historical bytes were reported from the unrelated live workspace generation as {:?}",
            completed.state()
        );
    }
    Ok(())
}

#[test]
fn same_uri_snapshots_with_distinct_hashes_reconcile_within_their_captured_generation()
-> Result<()> {
    let repo = TestRepo::new("relationship_bridge_same_uri_snapshots")?;
    repo.write("src/a.rs", A_BASE)?;
    repo.commit_all("snapshot base")?;
    let base = commit_id(&repo, "HEAD")?;
    repo.write("src/a.rs", A_HEAD)?;
    repo.commit_all("first captured generation")?;
    let first = commit_id(&repo, "HEAD")?;
    repo.write("src/a.rs", A_LIVE)?;
    repo.commit_all("second captured generation")?;
    let second = commit_id(&repo, "HEAD")?;
    let query = ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Revision(second.clone()),
        path_selection: ReviewPathSelection::All,
        diff_selection: ReviewDiffSelection::Targets(vec![
            ReviewDiffTarget::RevisionRange(CommitRange {
                start: base,
                end: first.clone(),
            }),
            ReviewDiffTarget::RevisionRange(CommitRange {
                start: first,
                end: second,
            }),
        ]),
    };
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let alphas = prepared
        .targets()
        .iter()
        .filter(|target| target.declaration.name == "alpha")
        .map(|target| target.declaration.id.clone())
        .collect::<Vec<_>>();
    ensure!(
        alphas.len() == 2,
        "fixture expected two captured alpha generations, got {}",
        alphas.len()
    );
    ensure!(alphas[0] != alphas[1], "captured generations reused one declaration ID");

    let identifier = LspRange::new(Position::new(0, 7), Position::new(0, 12));
    let provider = FakeProvider::new(
        &repo.path,
        [Script::SelfCallAt(identifier), Script::SelfCallAt(identifier)],
    );
    let mut bridge = RelationshipBridge::new(
        &prepared,
        &repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        provider,
    )?;

    for alpha in &alphas {
        let checking = bridge.request(alpha)?;
        assert_eq!(checking.state(), &GraphRelationshipState::Checking);
        let completed = bridge
            .poll()
            .context("missing isolated captured-generation completion")?;
        let destination = match completed.state() {
            GraphRelationshipState::Ready(groups) => groups
                .iter()
                .find(|group| group.label == "Called by")
                .and_then(|group| group.relationships.first())
                .map(|relationship| &relationship.destination)
                .context("missing captured-generation self-call")?,
            other => bail!("captured generation did not produce relationships: {other:?}"),
        };
        assert_eq!(
            destination,
            &RelationshipDestination::InReview {
                declaration_id: alpha.as_str().to_owned(),
            },
            "same-URI relationship escaped its snapshot/hash generation"
        );
    }
    Ok(())
}

#[test]
fn repeated_requests_surface_the_latest_accepted_generation_without_stale_poll_barriers()
-> Result<()> {
    let fixture = ReviewFixture::new("relationship_bridge_repeated_generation")?;
    let provider = FakeProvider::new(
        &fixture.repo.path,
        [
            Script::SupportedEmpty,
            Script::SupportedEmpty,
            Script::SupportedEmpty,
        ],
    );
    let mut bridge = RelationshipBridge::new(
        &fixture.prepared,
        &fixture.repo.path,
        WorkspaceTrust::TrustedForInvocation,
        Some(LspServerProfile::RustAnalyzer),
        provider,
    )?;

    let _obsolete_first = bridge.request(&fixture.alpha)?;
    let _obsolete_second = bridge.request(&fixture.alpha)?;
    let latest = bridge.request(&fixture.alpha)?;
    let completed = bridge.poll().context(
        "the latest accepted generation was blocked behind an obsolete queued completion",
    )?;
    assert_eq!(
        completed.source_generation(),
        latest.source_generation(),
        "the first surfaced completion must belong to the latest accepted request"
    );
    assert_eq!(completed.state(), &GraphRelationshipState::NoRelationships);
    Ok(())
}
