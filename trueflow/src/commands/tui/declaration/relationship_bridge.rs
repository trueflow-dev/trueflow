use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

use crate::analysis::Language;
use anyhow::{Context, Result, anyhow, ensure};
use async_lsp::lsp_types::{CallHierarchyItem, Range, Url};

use crate::declaration::relationships::{
    CallHierarchyBundle, DocumentHash, DocumentSnapshot, LspServerProfile, PositionEncoding,
    ProjectedDocument, ProviderCallHierarchyState, ProviderError, RelationshipEdge,
    RelationshipKind, RelationshipLocation, RelationshipMethod, RelationshipOutcome,
    RelationshipProjectionIndex, RelationshipProvenance, RelationshipProvider,
    RelationshipRequestKey, RelationshipResult, RelationshipScope, RelationshipTarget,
    SourceGeneration, WorkspaceTrust, execute_relationship_plan, lsp_position_for_byte_offset,
    plan_used_by, plan_uses_types, reconcile_relationship_execution,
};
use crate::declaration::snapshot::SnapshotId;
use crate::declaration::{DeclarationId, TypeUseRole, project_source};

use super::{
    DeclarationAppRuntime, PreparedDeclarationLaunch, PreparedDeclarationTarget, Relationship,
    RelationshipDestination, RelationshipGroup, RelationshipState,
};

const WORK_QUEUE_DEPTH: usize = 16;
const RESULT_QUEUE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
pub struct DeclarationRelationshipDocument {
    pub snapshot_id: String,
    pub path: String,
    pub uri: Url,
    pub version: i32,
    pub exact_source: String,
}

#[derive(Debug, Clone)]
pub struct DeclarationRelationshipRequest {
    pub workspace_root: PathBuf,
    pub key: RelationshipRequestKey,
    pub snapshot_id: SnapshotId,
    pub document: DeclarationRelationshipDocument,
    pub target: PreparedDeclarationTarget,
    documents: Vec<PreparedProviderDocument>,
    projection: RelationshipProjectionIndex,
}

#[derive(Debug, Clone)]
struct PreparedProviderDocument {
    uri: Url,
    version: i32,
    language: Language,
    exact_source: String,
}

#[derive(Debug, Clone)]
pub struct DeclarationRelationshipResults {
    pub snapshot_id: SnapshotId,
    pub call_hierarchy: ProviderCallHierarchyState,
    pub uses_types: RelationshipResult,
    pub used_by: RelationshipResult,
}

pub trait DeclarationRelationshipProvider {
    fn request(
        &mut self,
        request: DeclarationRelationshipRequest,
    ) -> std::result::Result<(), ProviderError>;

    fn poll(&mut self) -> Option<DeclarationRelationshipResults>;

    fn shutdown(&mut self) -> std::result::Result<(), ProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipUpdate {
    declaration_id: DeclarationId,
    declaration_key: crate::declaration::DeclarationKey,
    snapshot_id: SnapshotId,
    source_generation: SourceGeneration,
    state: RelationshipState,
}

impl RelationshipUpdate {
    pub fn declaration_id(&self) -> &DeclarationId {
        &self.declaration_id
    }

    pub const fn source_generation(&self) -> SourceGeneration {
        self.source_generation
    }

    pub const fn state(&self) -> &RelationshipState {
        &self.state
    }
}

#[derive(Debug, Clone)]
struct CatalogEntry {
    target: PreparedDeclarationTarget,
    document: DeclarationRelationshipDocument,
    language: Language,
    selection_ranges: Vec<Range>,
}

pub struct RelationshipBridge<P> {
    provider: P,
    workspace_root: PathBuf,
    trust: WorkspaceTrust,
    configured_profile: Option<LspServerProfile>,
    catalog: HashMap<DeclarationId, CatalogEntry>,
    documents: Vec<PreparedProviderDocument>,
    projection: RelationshipProjectionIndex,
    current: HashMap<DeclarationId, RelationshipRequestKey>,
    current_snapshots: HashMap<DeclarationId, SnapshotId>,
    latest_generations: HashMap<DeclarationId, SourceGeneration>,
    next_generation: u64,
    closed: bool,
}

impl<P: DeclarationRelationshipProvider> RelationshipBridge<P> {
    pub fn new(
        prepared: &PreparedDeclarationLaunch,
        workspace_root: &Path,
        trust: WorkspaceTrust,
        configured_profile: Option<LspServerProfile>,
        provider: P,
    ) -> Result<Self> {
        let uri_root = workspace_root.canonicalize().with_context(|| {
            format!(
                "cannot resolve relationship workspace {}",
                workspace_root.display()
            )
        })?;
        let mut projected = BTreeMap::new();
        let mut documents = Vec::new();
        for target in prepared.targets() {
            let path = target.display_path.as_str();
            let key = (path.to_owned(), target.snapshot.id.as_str().to_owned());
            if projected.contains_key(&key) {
                continue;
            }
            let absolute = uri_root.join(path);
            let uri = Url::from_file_path(&absolute)
                .map_err(|()| anyhow!("cannot create an LSP URI for {}", absolute.display()))?;
            let source = target.snapshot.source().to_owned();
            let facts = project_source(&target.snapshot.path, target.snapshot.language, &source)
                .with_context(|| format!("cannot project exact relationship source for {path}"))?;
            projected.insert(
                key,
                ProjectedDocument::new(uri.clone(), source.clone(), facts),
            );
            documents.push(PreparedProviderDocument {
                uri,
                version: 1,
                language: target.snapshot.language,
                exact_source: source,
            });
        }
        let projection = RelationshipProjectionIndex::new(
            RelationshipScope::ProjectedSubset,
            projected.into_values(),
        );

        let mut catalog = HashMap::with_capacity(prepared.targets().len());
        for target in prepared.targets() {
            let absolute = uri_root.join(target.display_path.as_str());
            let uri = Url::from_file_path(&absolute)
                .map_err(|()| anyhow!("cannot create an LSP URI for {}", absolute.display()))?;
            let exact_source = target.snapshot.source().to_owned();
            let name_start = declaration_name_start(target)?;
            let mut selection_ranges = Vec::with_capacity(3);
            for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
                let range = Range::new(
                    lsp_position_for_byte_offset(&exact_source, name_start, encoding)?,
                    lsp_position_for_byte_offset(
                        &exact_source,
                        name_start + target.declaration.name.len(),
                        encoding,
                    )?,
                );
                if !selection_ranges.contains(&range) {
                    selection_ranges.push(range);
                }
            }
            catalog.insert(
                target.declaration.id.clone(),
                CatalogEntry {
                    target: target.clone(),
                    document: DeclarationRelationshipDocument {
                        snapshot_id: target.snapshot.id.as_str().to_owned(),
                        path: target.display_path.as_str().to_owned(),
                        uri,
                        version: 1,
                        exact_source,
                    },
                    language: target.snapshot.language,
                    selection_ranges,
                },
            );
        }

        Ok(Self {
            provider,
            workspace_root: workspace_root.to_path_buf(),
            trust,
            configured_profile,
            catalog,
            documents,
            projection,
            current: HashMap::new(),
            current_snapshots: HashMap::new(),
            latest_generations: HashMap::new(),
            next_generation: 1,
            closed: false,
        })
    }

    pub fn request(&mut self, declaration_id: &DeclarationId) -> Result<RelationshipUpdate> {
        ensure!(!self.closed, "relationship bridge is shut down");
        let entry = self
            .catalog
            .get(declaration_id)
            .with_context(|| format!("unknown prepared declaration {}", declaration_id.as_str()))?
            .clone();
        let generation = SourceGeneration::new(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .context("relationship source generation exhausted")?;
        self.latest_generations
            .insert(declaration_id.clone(), generation);

        let unavailable = |reason: String| RelationshipUpdate {
            declaration_id: entry.target.declaration.id.clone(),
            declaration_key: entry.target.declaration.key.clone(),
            snapshot_id: entry.target.snapshot.id.clone(),
            source_generation: generation,
            state: RelationshipState::Unavailable { reason },
        };
        if self.trust != WorkspaceTrust::TrustedForInvocation {
            return Ok(unavailable(
                "workspace is not trusted; pass --trust-lsp-workspace for semantic relationships"
                    .to_owned(),
            ));
        }
        let Some(profile) = self.configured_profile else {
            return Ok(unavailable(
                "a fixed LSP server profile is not configured for this language".to_owned(),
            ));
        };
        if LspServerProfile::for_language(entry.language) != Some(profile) {
            return Ok(unavailable(format!(
                "the configured LSP profile does not support {:?}",
                entry.language
            )));
        }

        let key = RelationshipRequestKey {
            source_generation: generation,
            server_profile: profile,
            declaration_id: entry.target.declaration.id.clone(),
            declaration_key: entry.target.declaration.key.clone(),
            document_uri: entry.document.uri.clone(),
            document_version: entry.document.version,
            document_hash: DocumentHash::from_bytes(entry.document.exact_source.as_bytes()),
        };
        let request = DeclarationRelationshipRequest {
            workspace_root: self.workspace_root.clone(),
            key: key.clone(),
            snapshot_id: entry.target.snapshot.id.clone(),
            document: entry.document.clone(),
            target: entry.target.clone(),
            documents: self.documents.clone(),
            projection: self.projection.clone(),
        };
        self.current.insert(declaration_id.clone(), key);
        self.current_snapshots
            .insert(declaration_id.clone(), entry.target.snapshot.id.clone());
        if let Err(error) = self.provider.request(request) {
            return Ok(unavailable(error.to_string()));
        }
        Ok(RelationshipUpdate {
            declaration_id: entry.target.declaration.id,
            declaration_key: entry.target.declaration.key,
            snapshot_id: entry.target.snapshot.id,
            source_generation: generation,
            state: RelationshipState::Checking,
        })
    }

    pub fn poll(&mut self) -> Option<RelationshipUpdate> {
        if self.closed {
            return None;
        }
        let results = self.provider.poll()?;
        let received = result_key(&results.call_hierarchy);
        let expected = self.current.get(&received.declaration_id)?;
        let expected_snapshot = self.current_snapshots.get(&received.declaration_id)?;
        if expected != received
            || expected_snapshot != &results.snapshot_id
            || results.uses_types.key != *expected
            || results.used_by.key != *expected
        {
            return None;
        }
        let entry = self.catalog.get(&expected.declaration_id)?;
        if entry.target.declaration.key != expected.declaration_key
            || entry.target.snapshot.id != results.snapshot_id
        {
            return None;
        }
        let state = normalize_results(&results, &self.catalog);
        Some(RelationshipUpdate {
            declaration_id: expected.declaration_id.clone(),
            declaration_key: expected.declaration_key.clone(),
            snapshot_id: results.snapshot_id,
            source_generation: expected.source_generation,
            state,
        })
    }

    pub fn apply<A>(
        &mut self,
        runtime: &mut DeclarationAppRuntime<A>,
        update: RelationshipUpdate,
    ) -> Result<bool> {
        if self.closed || runtime.active_declaration_id() != Some(update.declaration_id.as_str()) {
            return Ok(false);
        }
        let Some(latest_generation) = self.latest_generations.get(&update.declaration_id) else {
            return Ok(false);
        };
        let Some(entry) = self.catalog.get(&update.declaration_id) else {
            return Ok(false);
        };
        if *latest_generation != update.source_generation
            || entry.target.declaration.key != update.declaration_key
            || entry.target.snapshot.id != update.snapshot_id
        {
            return Ok(false);
        }
        runtime.apply_relationship_state(update.declaration_id.as_str(), update.state)?;
        Ok(true)
    }

    pub fn shutdown(&mut self) -> std::result::Result<(), ProviderError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.current.clear();
        self.current_snapshots.clear();
        self.latest_generations.clear();
        self.provider.shutdown()
    }
}

pub(crate) struct ProductionRelationshipCoordinator {
    bridges: Vec<RelationshipBridge<BackgroundRelationshipProvider>>,
    declaration_bridges: HashMap<DeclarationId, usize>,
}

impl ProductionRelationshipCoordinator {
    pub(crate) fn new(
        prepared: &PreparedDeclarationLaunch,
        workspace_root: &Path,
        trust: WorkspaceTrust,
    ) -> Result<Self> {
        let mut profiles = Vec::new();
        for target in prepared.targets() {
            let profile = LspServerProfile::for_language(target.snapshot.language);
            if !profiles.contains(&profile) {
                profiles.push(profile);
            }
        }
        let mut bridges = Vec::with_capacity(profiles.len());
        for profile in &profiles {
            bridges.push(RelationshipBridge::new(
                prepared,
                workspace_root,
                trust,
                *profile,
                BackgroundRelationshipProvider::new()?,
            )?);
        }
        let declaration_bridges = prepared
            .targets()
            .iter()
            .map(|target| {
                let profile = LspServerProfile::for_language(target.snapshot.language);
                let index = profiles
                    .iter()
                    .position(|candidate| *candidate == profile)
                    .with_context(|| {
                        format!(
                            "no relationship bridge was prepared for server profile {profile:?}"
                        )
                    })?;
                Ok((target.declaration.id.clone(), index))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            bridges,
            declaration_bridges,
        })
    }

    pub(crate) fn request(&mut self, declaration_id: &DeclarationId) -> Result<RelationshipUpdate> {
        let index = *self
            .declaration_bridges
            .get(declaration_id)
            .with_context(|| format!("unknown prepared declaration {}", declaration_id.as_str()))?;
        self.bridges[index].request(declaration_id)
    }

    pub(crate) fn poll(&mut self) -> Option<RelationshipUpdate> {
        self.bridges.iter_mut().find_map(RelationshipBridge::poll)
    }

    pub(crate) fn apply<A>(
        &mut self,
        runtime: &mut DeclarationAppRuntime<A>,
        update: RelationshipUpdate,
    ) -> Result<bool> {
        let Some(index) = self
            .declaration_bridges
            .get(update.declaration_id())
            .copied()
        else {
            return Ok(false);
        };
        self.bridges[index].apply(runtime, update)
    }

    pub(crate) fn shutdown(&mut self) {
        for bridge in &mut self.bridges {
            let _ = bridge.shutdown();
        }
    }
}

impl Drop for ProductionRelationshipCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn declaration_name_start(target: &PreparedDeclarationTarget) -> Result<usize> {
    let source = target
        .snapshot
        .source()
        .get(target.declaration.source_span.clone())
        .context("declaration span is outside its exact snapshot")?;
    let relative = source
        .find(&target.declaration.name)
        .context("declaration name is absent from its exact source span")?;
    Ok(target.declaration.source_span.start + relative)
}

fn result_key(state: &ProviderCallHierarchyState) -> &RelationshipRequestKey {
    match state {
        ProviderCallHierarchyState::Ready(bundle)
        | ProviderCallHierarchyState::Partial { bundle, .. } => &bundle.key,
        ProviderCallHierarchyState::Unsupported { key, .. }
        | ProviderCallHierarchyState::Failed { key, .. } => key,
        ProviderCallHierarchyState::Stale { received, .. } => received,
    }
}

fn normalize_results(
    results: &DeclarationRelationshipResults,
    catalog: &HashMap<DeclarationId, CatalogEntry>,
) -> RelationshipState {
    let mut grouped: BTreeMap<u8, Vec<Relationship>> = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut unsupported = Vec::new();
    let mut failures = Vec::new();

    match &results.call_hierarchy {
        ProviderCallHierarchyState::Ready(bundle) => collect_calls(bundle, catalog, &mut grouped),
        ProviderCallHierarchyState::Partial {
            bundle,
            diagnostics: partial,
        } => {
            collect_calls(bundle, catalog, &mut grouped);
            diagnostics.extend(partial.iter().cloned());
        }
        ProviderCallHierarchyState::Unsupported { capability, .. } => {
            unsupported.push(format!("{capability:?}"));
        }
        ProviderCallHierarchyState::Failed { message, .. } => failures.push(message.clone()),
        ProviderCallHierarchyState::Stale { .. } => {
            return RelationshipState::Unavailable {
                reason: "stale call hierarchy response".to_owned(),
            };
        }
    }
    collect_outcome(
        &results.uses_types.outcome,
        RelationshipKind::UsesType,
        catalog,
        &mut grouped,
        &mut diagnostics,
        &mut unsupported,
        &mut failures,
    );
    collect_outcome(
        &results.used_by.outcome,
        RelationshipKind::UsedBy,
        catalog,
        &mut grouped,
        &mut diagnostics,
        &mut unsupported,
        &mut failures,
    );

    let groups = build_groups(grouped);
    if !failures.is_empty() {
        return RelationshipState::Unavailable {
            reason: failures.join("; "),
        };
    }
    if groups.is_empty() && diagnostics.is_empty() && !unsupported.is_empty() {
        return RelationshipState::Unavailable {
            reason: format!(
                "semantic relationship capabilities are unsupported: {}",
                unsupported.join(", ")
            ),
        };
    }
    if !diagnostics.is_empty() || !unsupported.is_empty() {
        diagnostics.extend(
            unsupported
                .into_iter()
                .map(|capability| format!("unsupported capability {capability}")),
        );
        return RelationshipState::Partial {
            reason: diagnostics.join("; "),
            groups,
        };
    }
    if groups.is_empty() {
        RelationshipState::NoRelationships
    } else {
        RelationshipState::Ready(groups)
    }
}

fn collect_calls(
    bundle: &CallHierarchyBundle,
    catalog: &HashMap<DeclarationId, CatalogEntry>,
    grouped: &mut BTreeMap<u8, Vec<Relationship>>,
) {
    for call in &bundle.incoming {
        grouped.entry(0).or_default().push(call_relationship(
            RelationshipKind::CalledBy,
            &call.from,
            &call.from_ranges,
            catalog,
        ));
    }
    for call in &bundle.outgoing {
        grouped.entry(1).or_default().push(call_relationship(
            RelationshipKind::Calls,
            &call.to,
            &call.from_ranges,
            catalog,
        ));
    }
}

fn call_relationship(
    kind: RelationshipKind,
    item: &CallHierarchyItem,
    call_ranges: &[Range],
    catalog: &HashMap<DeclarationId, CatalogEntry>,
) -> Relationship {
    let mut details = vec![format!("selection {}", display_range(item.selection_range))];
    if item.range != item.selection_range {
        details.push(format!("symbol {}", display_range(item.range)));
    }
    if !call_ranges.is_empty() {
        details.push(format!(
            "call sites {}",
            call_ranges
                .iter()
                .map(|range| display_range(*range))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let detail = details.join(" · ");
    let destination = exact_call_target(item, catalog)
        .map(|id| RelationshipDestination::InReview {
            declaration_id: id.as_str().to_owned(),
        })
        .unwrap_or_else(|| RelationshipDestination::External {
            context: format!("{} · {detail}", item.uri),
        });
    Relationship {
        id: format!(
            "{}:{}:{}:{}:{}",
            kind_key(kind),
            item.uri,
            display_range(item.selection_range),
            detail,
            item.name
        ),
        label: format!("{} — {detail}", item.name),
        destination,
    }
}

fn exact_call_target<'a>(
    item: &CallHierarchyItem,
    catalog: &'a HashMap<DeclarationId, CatalogEntry>,
) -> Option<&'a DeclarationId> {
    catalog.iter().find_map(|(id, entry)| {
        (entry.document.uri == item.uri && entry.selection_ranges.contains(&item.selection_range))
            .then_some(id)
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_outcome(
    outcome: &RelationshipOutcome,
    expected_kind: RelationshipKind,
    catalog: &HashMap<DeclarationId, CatalogEntry>,
    grouped: &mut BTreeMap<u8, Vec<Relationship>>,
    diagnostics: &mut Vec<String>,
    unsupported: &mut Vec<String>,
    failures: &mut Vec<String>,
) {
    let edges = match outcome {
        RelationshipOutcome::Complete { edges } => edges,
        RelationshipOutcome::Partial {
            edges,
            diagnostics: partial,
        } => {
            diagnostics.extend(partial.iter().cloned());
            edges
        }
        RelationshipOutcome::Unsupported { capability } => {
            unsupported.push(format!("{capability:?}"));
            return;
        }
        RelationshipOutcome::Failed { message } => {
            failures.push(message.clone());
            return;
        }
    };
    for edge in edges {
        if edge.kind != expected_kind {
            diagnostics.push(format!(
                "provider returned {:?} while resolving {expected_kind:?}",
                edge.kind
            ));
            continue;
        }
        grouped
            .entry(group_order(edge.kind))
            .or_default()
            .push(edge_relationship(edge, catalog));
    }
}

fn edge_relationship(
    edge: &RelationshipEdge,
    catalog: &HashMap<DeclarationId, CatalogEntry>,
) -> Relationship {
    let location_details = edge
        .locations
        .iter()
        .map(relationship_location_text)
        .collect::<Vec<_>>()
        .join("; ");
    let (target_label, destination, identity) = match &edge.target {
        RelationshipTarget::InReview(id) => {
            let label = catalog
                .get(id)
                .map(|entry| entry.target.declaration.name.clone())
                .unwrap_or_else(|| id.as_str().to_owned());
            (
                label,
                RelationshipDestination::InReview {
                    declaration_id: id.as_str().to_owned(),
                },
                format!("review:{}", id.as_str()),
            )
        }
        RelationshipTarget::External { uri, range } => (
            uri.to_string(),
            RelationshipDestination::External {
                context: external_context(uri, *range, &location_details),
            },
            format!("external:{uri}:{}", display_range(*range)),
        ),
        RelationshipTarget::Unresolved { name } => (
            name.clone(),
            RelationshipDestination::Unresolved,
            format!("unresolved:{name}"),
        ),
    };
    let label = if location_details.is_empty() {
        target_label
    } else {
        format!("{target_label} — {location_details}")
    };
    Relationship {
        id: format!("{}:{identity}:{location_details}", kind_key(edge.kind)),
        label,
        destination,
    }
}

fn relationship_location_text(location: &RelationshipLocation) -> String {
    let target = location.target.as_ref().map_or_else(
        || "unresolved".to_owned(),
        |target| format!("{} {}", target.uri, display_range(target.range)),
    );
    format!(
        "{} {} -> {target} · {}",
        location.origin.uri,
        display_range(location.origin.range),
        provenance_text(location)
    )
}

fn provenance_text(location: &RelationshipLocation) -> String {
    match location.provenance {
        RelationshipProvenance::TypeUse {
            method,
            role,
            scope,
        } => format!(
            "{} · {} · {}",
            method_text(method),
            role_text(role),
            scope_text(scope)
        ),
    }
}

fn external_context(uri: &Url, range: Range, details: &str) -> String {
    if details.is_empty() {
        format!("{uri} {}", display_range(range))
    } else {
        format!("{uri} {} · {details}", display_range(range))
    }
}

fn build_groups(mut grouped: BTreeMap<u8, Vec<Relationship>>) -> Vec<RelationshipGroup> {
    let mut groups = Vec::new();
    for (order, label) in [
        (0, "Called by"),
        (1, "Calls"),
        (2, "Uses types"),
        (3, "Used by"),
    ] {
        let Some(mut relationships) = grouped.remove(&order) else {
            continue;
        };
        relationships.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.id.cmp(&right.id))
        });
        relationships.dedup_by(|left, right| left.id == right.id);
        groups.push(RelationshipGroup {
            label: label.to_owned(),
            relationships,
        });
    }
    groups
}

const fn group_order(kind: RelationshipKind) -> u8 {
    match kind {
        RelationshipKind::CalledBy => 0,
        RelationshipKind::Calls => 1,
        RelationshipKind::UsesType => 2,
        RelationshipKind::UsedBy => 3,
    }
}

const fn kind_key(kind: RelationshipKind) -> &'static str {
    match kind {
        RelationshipKind::CalledBy => "called-by",
        RelationshipKind::Calls => "calls",
        RelationshipKind::UsesType => "uses-type",
        RelationshipKind::UsedBy => "used-by",
    }
}

fn display_range(range: Range) -> String {
    format!(
        "{}:{}-{}:{}",
        range.start.line + 1,
        range.start.character + 1,
        range.end.line + 1,
        range.end.character + 1
    )
}

const fn method_text(method: RelationshipMethod) -> &'static str {
    match method {
        RelationshipMethod::Declaration => "declaration",
        RelationshipMethod::Definition => "definition",
        RelationshipMethod::TypeDefinition => "type definition",
        RelationshipMethod::References => "references",
    }
}

const fn role_text(role: TypeUseRole) -> &'static str {
    match role {
        TypeUseRole::Parameter => "parameter",
        TypeUseRole::Return => "return",
        TypeUseRole::Field => "field",
        TypeUseRole::AliasTarget => "alias target",
        TypeUseRole::Variant => "variant",
        TypeUseRole::Bound => "bound",
        TypeUseRole::Other => "other",
    }
}

const fn scope_text(scope: RelationshipScope) -> &'static str {
    match scope {
        RelationshipScope::Workspace => "workspace",
        RelationshipScope::ProjectedSubset => "projected subset",
    }
}

pub struct BackgroundRelationshipProvider {
    work_tx: Option<SyncSender<DeclarationRelationshipRequest>>,
    result_rx: Receiver<DeclarationRelationshipResults>,
    closed: bool,
}

impl BackgroundRelationshipProvider {
    pub fn new() -> std::result::Result<Self, ProviderError> {
        let (work_tx, work_rx) = mpsc::sync_channel(WORK_QUEUE_DEPTH);
        let (result_tx, result_rx) = mpsc::sync_channel(RESULT_QUEUE_DEPTH);
        thread::Builder::new()
            .name("trueflow-relationship-coordinator".to_owned())
            .spawn(move || provider_worker(&work_rx, &result_tx))
            .map_err(|error| {
                ProviderError::Protocol(format!(
                    "cannot start relationship coordinator worker: {error}"
                ))
            })?;
        Ok(Self {
            work_tx: Some(work_tx),
            result_rx,
            closed: false,
        })
    }
}

impl DeclarationRelationshipProvider for BackgroundRelationshipProvider {
    fn request(
        &mut self,
        request: DeclarationRelationshipRequest,
    ) -> std::result::Result<(), ProviderError> {
        if self.closed {
            return Err(ProviderError::SessionClosed);
        }
        match self
            .work_tx
            .as_ref()
            .ok_or(ProviderError::SessionClosed)?
            .try_send(request)
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(ProviderError::ResourceLimit(
                "relationship background work queue is full".to_owned(),
            )),
            Err(TrySendError::Disconnected(_)) => Err(ProviderError::SessionClosed),
        }
    }

    fn poll(&mut self) -> Option<DeclarationRelationshipResults> {
        if self.closed {
            return None;
        }
        self.result_rx.try_recv().ok()
    }

    fn shutdown(&mut self) -> std::result::Result<(), ProviderError> {
        self.closed = true;
        self.work_tx.take();
        while self.result_rx.try_recv().is_ok() {}
        Ok(())
    }
}

impl Drop for BackgroundRelationshipProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn provider_worker(
    work_rx: &Receiver<DeclarationRelationshipRequest>,
    result_tx: &SyncSender<DeclarationRelationshipResults>,
) {
    let mut providers: HashMap<(PathBuf, LspServerProfile, Language), RelationshipProvider> =
        HashMap::new();
    while let Ok(request) = work_rx.recv() {
        let result = execute_provider_request(&request, &mut providers);
        if result_tx.send(result).is_err() {
            break;
        }
    }
}

fn execute_provider_request(
    request: &DeclarationRelationshipRequest,
    providers: &mut HashMap<(PathBuf, LspServerProfile, Language), RelationshipProvider>,
) -> DeclarationRelationshipResults {
    let session_key = (
        request.workspace_root.clone(),
        request.key.server_profile,
        request.target.snapshot.language,
    );
    let provider = match providers.entry(session_key) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            match RelationshipProvider::launch(
                request.key.server_profile,
                request.target.snapshot.language,
                &request.workspace_root,
                WorkspaceTrust::TrustedForInvocation,
            ) {
                Ok(provider) => entry.insert(provider),
                Err(error) => return failed_results(request, error.to_string()),
            }
        }
    };

    for document in &request.documents {
        if document.language != provider.language() {
            continue;
        }
        if let Err(error) = provider.synchronize_document(DocumentSnapshot::new(
            document.uri.clone(),
            document.version,
            document.language,
            document.exact_source.clone(),
        )) {
            return failed_results(request, error.to_string());
        }
    }

    let name_start = match declaration_name_start(&request.target) {
        Ok(start) => start,
        Err(error) => return failed_results(request, error.to_string()),
    };
    let position = match lsp_position_for_byte_offset(
        &request.document.exact_source,
        name_start,
        provider.position_encoding(),
    ) {
        Ok(position) => position,
        Err(error) => return failed_results(request, error.to_string()),
    };
    let call_hierarchy = provider.call_hierarchy(request.key.clone(), position);
    let uses_types = run_relationship_plan(request, provider, true);
    let used_by = run_relationship_plan(request, provider, false);
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

fn run_relationship_plan(
    request: &DeclarationRelationshipRequest,
    provider: &mut RelationshipProvider,
    uses_types: bool,
) -> RelationshipOutcome {
    let plan = if uses_types {
        plan_uses_types(
            &request.projection,
            &request.key.declaration_id,
            provider.position_encoding(),
        )
    } else {
        plan_used_by(
            &request.projection,
            &request.key.declaration_id,
            provider.position_encoding(),
        )
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            return RelationshipOutcome::Failed {
                message: error.to_string(),
            };
        }
    };
    let execution = match execute_relationship_plan(&plan, provider) {
        Ok(execution) => execution,
        Err(error) => {
            return RelationshipOutcome::Failed {
                message: error.to_string(),
            };
        }
    };
    reconcile_relationship_execution(&plan, execution, &request.projection).unwrap_or_else(
        |error| RelationshipOutcome::Failed {
            message: error.to_string(),
        },
    )
}

fn failed_results(
    request: &DeclarationRelationshipRequest,
    message: String,
) -> DeclarationRelationshipResults {
    DeclarationRelationshipResults {
        snapshot_id: request.snapshot_id.clone(),
        call_hierarchy: ProviderCallHierarchyState::Failed {
            key: request.key.clone(),
            message: message.clone(),
        },
        uses_types: RelationshipResult {
            key: request.key.clone(),
            outcome: RelationshipOutcome::Failed {
                message: message.clone(),
            },
        },
        used_by: RelationshipResult {
            key: request.key.clone(),
            outcome: RelationshipOutcome::Failed { message },
        },
    }
}
